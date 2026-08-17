//! Context compression.
//!
//! # What was wrong before
//!
//! The previous implementation had three independent bugs that combined into
//! "compression appears to work and then the request fails anyway":
//!
//! 1. **`content.len() / 4` token estimation.** Byte-length over four is not
//!    a token count (see [`crate::util::tokens`] for why), and it ignored
//!    per-message framing and tool-call JSON entirely. Under-counting is the
//!    dangerous direction: it means we believe we are at 60% when we are at
//!    95%, so we never compress and the provider rejects the request.
//! 2. **Hardcoded 128k window.** Every model was budgeted as if it had a
//!    128k context. An 8k model would blow past its real limit at ~6% of the
//!    assumed budget, so compression would never once fire before failure.
//! 3. **The compressed result could still be over target.** The old loop
//!    broke out of message-collection on the first message that did not fit,
//!    then — if still over budget — inserted a *summary line saying it had
//!    compressed*, which added tokens rather than removing any.
//!
//! # What this does instead
//!
//! Compression is structural, not lossy-by-summarization: keep the system
//! prompt (identity and durable facts — dropping it changes who the agent
//! *is* mid-conversation), keep the most recent messages that fit the target
//! budget, and replace the dropped middle with a single short marker so the
//! model knows history was elided rather than silently rewritten.
//!
//! Two invariants the old code violated and the tests here pin down:
//!
//! - **Never split a tool-call pair.** An assistant message with `tool_calls`
//!   must keep its answering `tool` messages, and a `tool` message must keep
//!   the assistant message that requested it. Providers hard-reject an
//!   orphaned `tool` message, so a compressor that cuts mid-pair converts a
//!   context problem into an immediate 400.
//! - **Compression must strictly reduce.** If a pass cannot get below the
//!   trigger, it must not return something *larger* than it received.

use crate::message::{Message, Role};
use crate::transport::ProviderTransport;
use crate::util::tokens::{HeuristicCounter, TokenCounter};
use crate::util::{truncate_utf8, Result};

/// Context compression configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ContextCompressionConfig {
    /// Enable automatic context compression when the trigger is reached.
    pub enabled: bool,
    /// Fraction of the context window that triggers compression (0.0–1.0).
    /// e.g. 0.75 = compress once 75% of the window is in use.
    pub trigger_fraction: f32,
    /// Target fraction after compression (0.0–1.0).
    pub target_fraction: f32,
    /// Messages at the tail that are never dropped, regardless of budget —
    /// without a floor, an aggressive target can strip the very exchange the
    /// model is mid-way through answering.
    pub min_recent_messages: usize,
}

impl Default for ContextCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_fraction: 0.75,
            target_fraction: 0.5,
            min_recent_messages: 4,
        }
    }
}

impl ContextCompressionConfig {
    /// Clamp the fractions into a sane range and guarantee
    /// `target <= trigger`. A config file with `target_fraction = 0.9` and
    /// `trigger_fraction = 0.5` would otherwise "compress" to something larger
    /// than the threshold that fired it, and re-fire every single iteration.
    pub fn normalized(&self) -> Self {
        let trigger = self.trigger_fraction.clamp(0.05, 0.98);
        let target = self.target_fraction.clamp(0.05, trigger);
        Self {
            enabled: self.enabled,
            trigger_fraction: trigger,
            target_fraction: target,
            min_recent_messages: self.min_recent_messages.max(1),
        }
    }
}

/// Fallback window when neither the transport nor the static model table can
/// say. Conservative on purpose: over-compressing costs a little history,
/// under-compressing costs the whole request.
pub const FALLBACK_CONTEXT_WINDOW: u32 = 32_000;

/// The marker inserted where history was elided.
const ELISION_MARKER: &str = "[earlier conversation elided to fit the context window]";

/// System prompt for the model that generates a smart summary of dropped context.
const SUMMARY_SYSTEM_PROMPT: &str =
    "You are a context summarizer. Summarize the following conversation in a concise \
     way that preserves key facts, decisions, topics discussed, and context the assistant \
     needs to continue the conversation naturally. Focus on technical details, code changes, \
     decisions made, and the current state of any ongoing task. Keep the summary brief — \
     aim for no more than 200 tokens.";

/// What one compression pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionOutcome {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub dropped_messages: usize,
}

impl CompressionOutcome {
    /// Whether anything was actually removed.
    pub fn changed(&self) -> bool {
        self.dropped_messages > 0
    }
}

/// Full result of a model-assisted compression pass.
pub struct CompressionResult {
    pub messages: Vec<Message>,
    pub outcome: CompressionOutcome,
    /// Model-generated summary of the dropped content, or `None` when the
    /// plain elision marker was used (either no model available or summary
    /// call failed).
    pub summary: Option<String>,
}

/// Result of computing where to cut the message list.
#[derive(Debug, Clone, Copy)]
struct CompressionSplit {
    /// Number of system messages at the head (0 or 1).
    system_len: usize,
    /// Index *within* the non-system body from which to keep messages.
    keep_from: usize,
    /// Token cost of the system portion.
    system_cost: usize,
}

/// Decides when to compress and does it, against a pluggable token counter.
pub struct Compressor<C: TokenCounter = HeuristicCounter> {
    config: ContextCompressionConfig,
    counter: C,
    context_window: u32,
}

impl Compressor<HeuristicCounter> {
    /// Build a compressor for `transport`, asking it for the real context
    /// window and falling back only when the provider cannot say.
    ///
    /// This is the fix for the hardcoded 128k: the window now comes from the
    /// model actually in use.
    pub fn for_transport(
        config: &ContextCompressionConfig,
        transport: &(dyn ProviderTransport + '_),
    ) -> Self {
        let window = transport
            .context_window()
            .unwrap_or(FALLBACK_CONTEXT_WINDOW);
        Self::with_window(config, window)
    }

    /// Build a compressor against an explicit window.
    pub fn with_window(config: &ContextCompressionConfig, context_window: u32) -> Self {
        Self {
            config: config.normalized(),
            counter: HeuristicCounter,
            context_window: context_window.max(1_000),
        }
    }
}

impl<C: TokenCounter> Compressor<C> {
    /// Estimated token cost of a conversation.
    pub fn estimate(&self, messages: &[Message]) -> usize {
        self.counter.count_messages(messages)
    }

    /// The window this compressor is budgeting against.
    pub fn context_window(&self) -> u32 {
        self.context_window
    }

    /// Token count at which compression fires.
    pub fn trigger_tokens(&self) -> usize {
        (f64::from(self.context_window) * f64::from(self.config.trigger_fraction)) as usize
    }

    /// Token count compression aims to land under.
    pub fn target_tokens(&self) -> usize {
        (f64::from(self.context_window) * f64::from(self.config.target_fraction)) as usize
    }

    /// Whether `messages` currently exceeds the trigger.
    pub fn should_compress(&self, messages: &[Message]) -> bool {
        self.config.enabled && self.estimate(messages) > self.trigger_tokens()
    }

    /// Compress `messages` in place if it is over the trigger.
    ///
    /// Returns `None` when nothing was done (disabled, under the trigger, or
    /// nothing safely droppable), so a caller can distinguish "compressed" from
    /// "considered and declined".
    pub fn compress_in_place(&self, messages: &mut Vec<Message>) -> Option<CompressionOutcome> {
        if !self.should_compress(messages) {
            return None;
        }
        let before_tokens = self.estimate(messages);
        let before_len = messages.len();
        let compressed = self.compress(messages);

        // Refuse a pass that did not strictly help. The old implementation
        // could append an explanatory summary line and hand back something
        // *larger* than it was given, which then re-triggered forever.
        let after_tokens = self.estimate(&compressed);
        if compressed.len() >= before_len || after_tokens >= before_tokens {
            return None;
        }

        let dropped_messages = before_len - compressed.len();
        *messages = compressed;
        Some(CompressionOutcome {
            before_tokens,
            after_tokens,
            dropped_messages,
        })
    }

    /// Compress `messages` in place using a model-generated summary, if the
    /// trigger is exceeded.  Returns the full [`CompressionResult`] so the
    /// caller can surface the summary text.
    pub fn compress_in_place_with_model(
        &self,
        messages: &mut Vec<Message>,
        transport: &(dyn ProviderTransport + '_),
    ) -> Option<CompressionResult> {
        if !self.should_compress(messages) {
            return None;
        }
        let before_len = messages.len();
        let before_tokens = self.estimate(messages);
        let result = self.compress_with_model(messages, transport);

        match result {
            Ok(res) => {
                // Refuse a pass that did not strictly help.
                if res.messages.len() >= before_len || res.outcome.after_tokens >= before_tokens {
                    return None;
                }
                *messages = res.messages.clone();
                Some(res)
            }
            Err(_) => {
                // Summary call failed — fall back to plain compression.
                let compressed = self.compress(messages);
                let after_tokens = self.estimate(&compressed);
                if compressed.len() >= before_len || after_tokens >= before_tokens {
                    return None;
                }
                let dropped_messages = before_len - compressed.len();
                *messages = compressed;
                Some(CompressionResult {
                    messages: messages.clone(),
                    outcome: CompressionOutcome {
                        before_tokens,
                        after_tokens,
                        dropped_messages,
                    },
                    summary: None,
                })
            }
        }
    }

    /// Produce the compressed conversation without mutating the input.
    fn compress(&self, messages: &[Message]) -> Vec<Message> {
        let split = self.compute_split(messages);
        let (system, rest) = messages.split_at(split.system_len);
        // The elision marker's cost is already in the body budget, so the
        // split point is final as-is — no summary cost left to reserve.
        self.build_compressed(system, rest, split.keep_from, None)
    }

    /// Compress `messages` using a model-generated summary for the dropped
    /// portion. Falls back to the plain elision marker on any error.
    pub fn compress_with_model(
        &self,
        messages: &[Message],
        transport: &(dyn ProviderTransport + '_),
    ) -> Result<CompressionResult> {
        let before_tokens = self.estimate(messages);
        let before_len = messages.len();

        let split = self.compute_split(messages);
        let (system, rest) = messages.split_at(split.system_len);

        let marker_cost = self.counter.count_message(&Message::user(ELISION_MARKER));
        let body_budget =
            self.target_tokens().saturating_sub(split.system_cost + marker_cost);

        // Pick the cut ignoring the summary (its cost is not knowable until
        // it exists) — that span is the first thing to summarize.
        let mut keep_from = split.keep_from;
        if rest[..keep_from].is_empty() {
            return Ok(CompressionResult {
                messages: messages.to_vec(),
                outcome: CompressionOutcome {
                    before_tokens,
                    after_tokens: before_tokens,
                    dropped_messages: 0,
                },
                summary: None,
            });
        }

        // Summarize the dropped span, then re-cut against the summary's REAL
        // token cost (the old code reserved the cost of a fixed placeholder
        // string, so a longer summary silently ate into the tail budget).
        // If the re-cut drops more than the summary covers, re-summarize the
        // larger final span — every dropped message must be in the summary,
        // never neither kept nor summarized. The cut only moves forward
        // (the budget never grows) and is floored by min_recent_messages, so
        // this converges; the cap bounds pathological oscillation.
        let mut summary = summarize_dropped(&rest[..keep_from], transport).ok();
        for _ in 0..3 {
            let Some(sum) = summary.clone() else {
                break;
            };
            let summary_cost = self.counter.count_message(&Message::assistant(format!(
                "[conversation summary]\n{sum}"
            )));
            let final_keep =
                final_keep_from(rest, body_budget, summary_cost, &self.counter, &self.config);
            if final_keep == keep_from {
                break;
            }
            keep_from = final_keep;
            summary = summarize_dropped(&rest[..keep_from], transport).ok();
        }

        let compressed = self.build_compressed(system, rest, keep_from, summary.clone());

        let after_tokens = self.estimate(&compressed);
        let dropped_messages = before_len - compressed.len();

        Ok(CompressionResult {
            messages: compressed,
            outcome: CompressionOutcome {
                before_tokens,
                after_tokens,
                dropped_messages,
            },
            summary,
        })
    }

    /// Compute where to cut: returns system length, keep-from index, and costs.
    fn compute_split(&self, messages: &[Message]) -> CompressionSplit {
        let target = self.target_tokens();
        let (system, rest) = split_system(messages);
        let system_cost: usize = system.iter().map(|m| self.counter.count_message(m)).sum();
        let marker_cost = self.counter.count_message(&Message::user(ELISION_MARKER));
        let body_budget = target.saturating_sub(system_cost + marker_cost);

        let mut keep_from = rest.len();
        let mut used = 0usize;
        for (idx, msg) in rest.iter().enumerate().rev() {
            let cost = self.counter.count_message(msg);
            let kept_so_far = rest.len() - idx - 1;
            if used + cost > body_budget && kept_so_far >= self.config.min_recent_messages {
                break;
            }
            used += cost;
            keep_from = idx;
        }
        let keep_from = safe_boundary(rest, keep_from);

        CompressionSplit {
            system_len: system.len(),
            keep_from,
            system_cost,
        }
    }

    /// Build the compressed message list from a split point and optional summary.
    fn build_compressed(
        &self,
        system: &[Message],
        rest: &[Message],
        keep_from: usize,
        summary: Option<String>,
    ) -> Vec<Message> {
        let mut out = Vec::with_capacity(system.len() + 1 + (rest.len() - keep_from));
        out.extend(system.iter().cloned());
        if keep_from > 0 {
            if let Some(sum) = summary {
                out.push(Message::assistant(format!(
                    "[conversation summary]\n{sum}"
                )));
            } else {
                out.push(Message::user(ELISION_MARKER));
            }
        }
        out.extend(rest[keep_from..].iter().cloned());
        out
    }
}

/// Re-derive the cut once the summary's real token cost is known: the kept
/// tail must fit the body budget alongside the summary message. The same
/// walk as `compute_split`, with the summary cost deducted up front.
fn final_keep_from<C: TokenCounter>(
    rest: &[Message],
    body_budget: usize,
    summary_cost: usize,
    counter: &C,
    config: &ContextCompressionConfig,
) -> usize {
    let available = body_budget.saturating_sub(summary_cost);
    let mut keep_from = rest.len();
    let mut used = 0usize;
    for (idx, msg) in rest.iter().enumerate().rev() {
        let cost = counter.count_message(msg);
        let kept_so_far = rest.len() - idx - 1;
        if used + cost > available && kept_so_far >= config.min_recent_messages {
            break;
        }
        used += cost;
        keep_from = idx;
    }
    safe_boundary(rest, keep_from)
}

/// Call the model to produce a summary of the dropped portion.
fn summarize_dropped(
    dropped: &[Message],
    transport: &(dyn ProviderTransport + '_),
) -> Result<String> {
    let transcript = dropped
        .iter()
        .map(|m| {
            let mut line = format!("[{}]: {}", m.role.as_str(), m.content);
            // Without the tool calls the model loses *what it was doing* —
            // an in-flight ReAct turn reads as "assistant said a word" and
            // the summary drops the work that was actually underway.
            if !m.tool_calls.is_empty() {
                let calls = m
                    .tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.name(), truncate_utf8(c.arguments(), 200)))
                    .collect::<Vec<_>>()
                    .join(", ");
                line.push_str(&format!(" [tool_calls: {calls}]"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");

    let messages = vec![
        Message::system(SUMMARY_SYSTEM_PROMPT),
        Message::user(transcript),
    ];

    let resp = transport.complete(&messages, &[], "")?;
    Ok(resp.content)
}

/// Split off a leading system message, if present.
fn split_system(messages: &[Message]) -> (&[Message], &[Message]) {
    if messages.first().is_some_and(|m| m.role == Role::System) {
        messages.split_at(1)
    } else {
        (&[], messages)
    }
}

/// Move a cut point forward until it does not orphan a `tool` message.
///
/// A `tool` message without the assistant message that requested it is
/// rejected outright by OpenAI-compatible providers — cutting mid-pair turns a
/// context-size problem into an immediate hard 400.
fn safe_boundary(messages: &[Message], mut idx: usize) -> usize {
    while idx < messages.len() && messages[idx].role == Role::Tool {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn cfg() -> ContextCompressionConfig {
        ContextCompressionConfig {
            enabled: true,
            trigger_fraction: 0.75,
            target_fraction: 0.5,
            min_recent_messages: 2,
        }
    }

    /// Deterministic filler of roughly `tokens` tokens.
    fn filler(tokens: usize) -> String {
        vec!["word"; tokens].join(" ")
    }

    fn long_conversation(turns: usize, tokens_each: usize) -> Vec<Message> {
        let mut msgs = vec![Message::system("you are grace")];
        for i in 0..turns {
            msgs.push(Message::user(format!("q{i} {}", filler(tokens_each))));
            msgs.push(Message::assistant(format!("a{i} {}", filler(tokens_each))));
        }
        msgs
    }

    #[test]
    fn under_the_trigger_nothing_happens() {
        let c = Compressor::with_window(&cfg(), 32_000);
        let mut msgs = long_conversation(2, 10);
        let before = msgs.clone();
        assert!(c.compress_in_place(&mut msgs).is_none());
        assert_eq!(msgs.len(), before.len());
    }

    #[test]
    fn disabled_config_never_compresses() {
        let mut config = cfg();
        config.enabled = false;
        let c = Compressor::with_window(&config, 1_000);
        let mut msgs = long_conversation(200, 50);
        assert!(c.compress_in_place(&mut msgs).is_none());
    }

    #[test]
    fn over_the_trigger_compresses_below_the_trigger() {
        // The actual contract: after a pass, we are no longer over the line
        // that fired it. The old implementation could fail this outright.
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs = long_conversation(60, 40);
        assert!(c.should_compress(&msgs));
        let outcome = c.compress_in_place(&mut msgs).expect("should compress");
        assert!(outcome.changed());
        assert!(
            c.estimate(&msgs) <= c.trigger_tokens(),
            "still over trigger after compression: {} > {}",
            c.estimate(&msgs),
            c.trigger_tokens()
        );
    }

    #[test]
    fn compression_strictly_reduces_token_count() {
        // Regression: the old code inserted an explanatory summary that could
        // make the result *bigger* than the input, re-triggering every round.
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs = long_conversation(60, 40);
        let before = c.estimate(&msgs);
        let outcome = c.compress_in_place(&mut msgs).unwrap();
        assert!(outcome.after_tokens < outcome.before_tokens);
        assert!(c.estimate(&msgs) < before);
    }

    #[test]
    fn the_system_prompt_always_survives() {
        // It carries identity and durable facts; dropping it changes who the
        // agent is halfway through a conversation.
        let c = Compressor::with_window(&cfg(), 2_000);
        let mut msgs = long_conversation(80, 40);
        c.compress_in_place(&mut msgs).unwrap();
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content, "you are grace");
    }

    #[test]
    fn the_most_recent_exchange_is_preserved() {
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs = long_conversation(60, 40);
        let last = msgs.last().unwrap().content.clone();
        c.compress_in_place(&mut msgs).unwrap();
        assert_eq!(msgs.last().unwrap().content, last);
    }

    #[test]
    fn an_elision_marker_records_that_history_was_dropped() {
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs = long_conversation(60, 40);
        c.compress_in_place(&mut msgs).unwrap();
        assert!(
            msgs.iter().any(|m| m.content.contains("elided")),
            "the model must know history was cut, not silently rewritten"
        );
    }

    #[test]
    fn tool_results_are_never_orphaned_from_their_call() {
        // Providers hard-reject a `tool` message with no preceding assistant
        // tool_calls — cutting mid-pair turns a size problem into a 400.
        let mut msgs = vec![Message::system("sys")];
        for i in 0..40 {
            let mut a = Message::assistant(filler(30));
            a.tool_calls = vec![ToolCall::new(format!("c{i}"), "bash", "{}")];
            msgs.push(a);
            msgs.push(Message::tool(format!("c{i}"), "bash", filler(30)));
        }
        let c = Compressor::with_window(&cfg(), 4_000);
        c.compress_in_place(&mut msgs).unwrap();

        // Any surviving tool message must be preceded by an assistant message.
        for (i, m) in msgs.iter().enumerate() {
            if m.role == Role::Tool {
                let prev = msgs.get(i.wrapping_sub(1)).map(|p| p.role);
                assert_eq!(
                    prev,
                    Some(Role::Assistant),
                    "orphaned tool message at index {i}"
                );
            }
        }
    }

    #[test]
    fn min_recent_messages_is_honored_even_on_a_tiny_budget() {
        let mut config = cfg();
        config.min_recent_messages = 4;
        config.target_fraction = 0.05;
        config.trigger_fraction = 0.1;
        let c = Compressor::with_window(&config, 1_000);
        let mut msgs = long_conversation(50, 60);
        c.compress_in_place(&mut msgs).unwrap();
        // system + marker + at least the floor of recent messages.
        assert!(
            msgs.len() >= 4,
            "must keep a floor of recent context, got {}",
            msgs.len()
        );
    }

    #[test]
    fn a_conversation_with_no_system_message_still_compresses() {
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs: Vec<Message> = (0..200)
            .map(|i| Message::user(format!("{i} {}", filler(40))))
            .collect();
        let outcome = c.compress_in_place(&mut msgs);
        assert!(outcome.is_some());
        assert!(!msgs.is_empty());
    }

    #[test]
    fn empty_conversation_is_a_no_op() {
        let c = Compressor::with_window(&cfg(), 32_000);
        let mut msgs: Vec<Message> = Vec::new();
        assert!(c.compress_in_place(&mut msgs).is_none());
        assert!(msgs.is_empty());
    }

    #[test]
    fn config_normalization_forces_target_below_trigger() {
        // target > trigger would "compress" to above the threshold that fired
        // it, re-firing on every single iteration forever.
        let bad = ContextCompressionConfig {
            enabled: true,
            trigger_fraction: 0.5,
            target_fraction: 0.9,
            min_recent_messages: 0,
        };
        let n = bad.normalized();
        assert!(n.target_fraction <= n.trigger_fraction);
        assert!(n.min_recent_messages >= 1);
    }

    #[test]
    fn config_normalization_clamps_out_of_range_fractions() {
        let bad = ContextCompressionConfig {
            enabled: true,
            trigger_fraction: 5.0,
            target_fraction: -1.0,
            min_recent_messages: 2,
        };
        let n = bad.normalized();
        assert!(n.trigger_fraction > 0.0 && n.trigger_fraction < 1.0);
        assert!(n.target_fraction > 0.0 && n.target_fraction <= n.trigger_fraction);
    }

    #[test]
    fn window_comes_from_the_transport_not_a_hardcoded_constant() {
        // The old loop assumed 128k for every model, so an 8k model never
        // compressed until the request had already failed.
        struct Small;
        impl ProviderTransport for Small {
            fn name(&self) -> &str {
                "small"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[crate::transport::ToolSpec],
                _model: &str,
            ) -> crate::util::Result<crate::transport::ModelResponse> {
                unreachable!()
            }
            fn context_window(&self) -> Option<u32> {
                Some(8_000)
            }
        }
        let c = Compressor::for_transport(&cfg(), &Small);
        assert_eq!(c.context_window(), 8_000);
        assert_eq!(c.trigger_tokens(), 6_000);
        assert_eq!(c.target_tokens(), 4_000);
    }

    #[test]
    fn unknown_window_falls_back_conservatively() {
        struct Unknown;
        impl ProviderTransport for Unknown {
            fn name(&self) -> &str {
                "unknown"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[crate::transport::ToolSpec],
                _model: &str,
            ) -> crate::util::Result<crate::transport::ModelResponse> {
                unreachable!()
            }
        }
        let c = Compressor::for_transport(&cfg(), &Unknown);
        assert_eq!(c.context_window(), FALLBACK_CONTEXT_WINDOW);
    }

    #[test]
    fn repeated_compression_converges_instead_of_looping() {
        // Second pass on an already-compressed conversation must decline,
        // otherwise every iteration pays for a pointless rewrite.
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs = long_conversation(60, 40);
        assert!(c.compress_in_place(&mut msgs).is_some());
        assert!(
            c.compress_in_place(&mut msgs).is_none(),
            "a converged conversation must not re-compress"
        );
    }

    #[test]
    fn model_compression_replaces_marker_with_summary() {
        struct SummaryTransport;
        impl ProviderTransport for SummaryTransport {
            fn name(&self) -> &str {
                "summary"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[crate::transport::ToolSpec],
                _model: &str,
            ) -> crate::util::Result<crate::transport::ModelResponse> {
                Ok(crate::transport::ModelResponse {
                    content: "User asked about Rust lifetimes; assistant explained borrow checker rules.".into(),
                    tool_calls: vec![],
                    finish_reason: crate::transport::FinishReason::Stop,
                })
            }
            fn context_window(&self) -> Option<u32> {
                Some(4_000)
            }
        }
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs = long_conversation(60, 40);
        let result = c.compress_in_place_with_model(&mut msgs, &SummaryTransport).expect("should compress");
        assert!(result.outcome.changed());
        assert!(result.summary.is_some(), "model summary should be present");
        // The summary replaces the marker — no elided marker in the result
        assert!(
            !msgs.iter().any(|m| m.content.contains("elided")),
            "summary should replace the plain elision marker"
        );
        // The summary message should be an assistant message
        let summary_msg = msgs.iter().find(|m| m.content.starts_with("[conversation summary]"));
        assert!(summary_msg.is_some(), "summary message should be present");
        assert_eq!(summary_msg.map(|m| m.role), Some(Role::Assistant));
    }

    #[test]
    fn model_compression_falls_back_on_error() {
        struct FailingTransport;
        impl ProviderTransport for FailingTransport {
            fn name(&self) -> &str {
                "failing"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[crate::transport::ToolSpec],
                _model: &str,
            ) -> crate::util::Result<crate::transport::ModelResponse> {
                Err(crate::util::AgentError::Transport("summary call failed".into()))
            }
            fn context_window(&self) -> Option<u32> {
                Some(4_000)
            }
        }
        let c = Compressor::with_window(&cfg(), 4_000);
        let mut msgs = long_conversation(60, 40);
        let result = c.compress_in_place_with_model(&mut msgs, &FailingTransport).expect("should compress");
        assert!(result.outcome.changed());
        assert!(result.summary.is_none(), "should fall back to no summary");
        // Should still have the plain marker
        assert!(
            msgs.iter().any(|m| m.content.contains("elided")),
            "fallback should use elision marker"
        );
    }

    #[test]
    fn model_compression_preserves_tool_call_pairs() {
        struct SummaryTransport;
        impl ProviderTransport for SummaryTransport {
            fn name(&self) -> &str {
                "summary"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[crate::transport::ToolSpec],
                _model: &str,
            ) -> crate::util::Result<crate::transport::ModelResponse> {
                Ok(crate::transport::ModelResponse {
                    content: "Summary of tool interactions".into(),
                    tool_calls: vec![],
                    finish_reason: crate::transport::FinishReason::Stop,
                })
            }
            fn context_window(&self) -> Option<u32> {
                Some(4_000)
            }
        }
        let mut msgs = vec![Message::system("sys")];
        for i in 0..40 {
            let mut a = Message::assistant(filler(30));
            a.tool_calls = vec![ToolCall::new(format!("c{i}"), "bash", "{}")];
            msgs.push(a);
            msgs.push(Message::tool(format!("c{i}"), "bash", filler(30)));
        }
        let c = Compressor::with_window(&cfg(), 4_000);
        c.compress_in_place_with_model(&mut msgs, &SummaryTransport).unwrap();

        // Any surviving tool message must be preceded by an assistant message.
        for (i, m) in msgs.iter().enumerate() {
            if m.role == Role::Tool {
                let prev = msgs.get(i.wrapping_sub(1)).map(|p| p.role);
                assert_eq!(
                    prev,
                    Some(Role::Assistant),
                    "orphaned tool message at index {i}"
                );
            }
        }
    }

    /// A transport that records each summarize request so a test can inspect
    /// the transcript the dropped messages were rendered as.
    struct CaptureTransport<M: FnMut(&[Message]) + 'static> {
        capture: std::sync::Mutex<Option<Box<M>>>,
        answer: String,
    }

    fn capture_transport<M: FnMut(&[Message]) + 'static>(
        answer: &str,
        capture: M,
    ) -> CaptureTransport<M> {
        CaptureTransport {
            capture: std::sync::Mutex::new(Some(Box::new(capture))),
            answer: answer.to_string(),
        }
    }

    impl<M: FnMut(&[Message]) + 'static> ProviderTransport for CaptureTransport<M> {
        fn name(&self) -> &str {
            "capture"
        }
        fn complete(
            &self,
            m: &[Message],
            _t: &[crate::transport::ToolSpec],
            _model: &str,
        ) -> crate::util::Result<crate::transport::ModelResponse> {
            if let Some(c) = self.capture.lock().unwrap().as_mut() {
                c(m);
            }
            Ok(crate::transport::ModelResponse {
                content: self.answer.clone(),
                tool_calls: vec![],
                finish_reason: crate::transport::FinishReason::Stop,
            })
        }
        fn context_window(&self) -> Option<u32> {
            Some(4_000)
        }
    }

    fn tool_heavy_conversation(turns: usize, tokens_each: usize) -> Vec<Message> {
        let mut msgs = vec![Message::system("sys")];
        for i in 0..turns {
            msgs.push(Message::user(format!("q{i} {}", filler(tokens_each))));
            let mut a = Message::assistant(format!("a{i} {}", filler(tokens_each)));
            a.tool_calls = vec![ToolCall::new(
                format!("c{i}"),
                "bash",
                format!("{{\"command\":\"run_step_{i}\"}}"),
            )];
            msgs.push(a);
            msgs.push(Message::tool(format!("c{i}"), "bash", "ok"));
        }
        msgs
    }

    #[test]
    fn the_summary_transcript_keeps_tool_calls_of_the_dropped_span() {
        // If the dropped turns' tool calls are not in the transcript, the
        // summary can only say "the assistant said a word" and the model
        // loses the work that was underway.
        static CAPTURED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let transport = capture_transport("sum", |m: &[Message]| {
            if let Some(t) = m.last() {
                CAPTURED.lock().unwrap().push(t.content.clone());
            }
        });
        let mut msgs = tool_heavy_conversation(60, 40);
        let c = Compressor::with_window(&cfg(), 4_000);
        c.compress_in_place_with_model(&mut msgs, &transport)
            .expect("should compress");
        let captures = CAPTURED.lock().unwrap();
        assert!(!captures.is_empty(), "the summary call must have happened");
        let final_transcript = captures.last().unwrap();
        assert!(
            final_transcript.contains("run_step_"),
            "tool arguments must survive into the summary transcript"
        );
        assert!(
            final_transcript.contains("bash"),
            "tool names must survive into the summary transcript"
        );
    }

    #[test]
    fn the_real_summary_cost_is_reserved_not_a_placeholder() {
        // A summary much larger than the old fixed placeholder must shrink
        // the kept tail accordingly; otherwise the compressed result lands
        // over the target exactly when the summary is long.
        let transport = capture_transport(&filler(60), |_: &[Message]| {});
        let mut msgs = long_conversation(60, 40);
        let c = Compressor::with_window(&cfg(), 4_000);
        let result = c
            .compress_in_place_with_model(&mut msgs, &transport)
            .unwrap();
        assert!(result.outcome.changed());
        assert!(
            result.outcome.after_tokens <= c.target_tokens(),
            "after={} exceeds target {} — the summary cost was not reserved",
            result.outcome.after_tokens,
            c.target_tokens()
        );
    }

    #[test]
    fn the_summary_covers_exactly_the_final_dropped_span() {
        // If reserving the real summary cost moves the cut, the messages
        // between the first span and the final cut must be summarized too —
        // a summary of only the first span would leave a gap that is neither
        // kept nor described.
        static CAPTURED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let transport = capture_transport(&filler(60), |m: &[Message]| {
            if let Some(t) = m.last() {
                CAPTURED.lock().unwrap().push(t.content.clone());
            }
        });
        let mut msgs = long_conversation(60, 40);
        let before_len = msgs.len();
        let c = Compressor::with_window(&cfg(), 4_000);
        let result = c
            .compress_in_place_with_model(&mut msgs, &transport)
            .unwrap();
        assert!(result.summary.is_some());
        let kept_tail = result.messages.len() - 2; // system + summary
        let final_dropped = before_len - 1 - kept_tail;
        let guard = CAPTURED.lock().unwrap();
        let final_transcript = guard.last().unwrap();
        assert_eq!(
            final_transcript.lines().count(),
            final_dropped,
            "the final summary must describe every dropped message and no more"
        );
    }
}
