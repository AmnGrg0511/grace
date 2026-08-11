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

    /// Produce the compressed conversation without mutating the input.
    fn compress(&self, messages: &[Message]) -> Vec<Message> {
        let target = self.target_tokens();

        // The leading system message is identity + durable facts. Dropping it
        // changes who the agent is mid-conversation, so it is never a
        // candidate — it is subtracted from the budget instead.
        let (system, rest) = split_system(messages);
        let system_cost: usize = system.iter().map(|m| self.counter.count_message(m)).sum();
        let marker = Message::system(ELISION_MARKER);
        let marker_cost = self.counter.count_message(&marker);
        let body_budget = target.saturating_sub(system_cost + marker_cost);

        // Walk backwards accumulating the most recent messages that fit, then
        // widen the cut to the nearest safe boundary so no tool-call pair is
        // split.
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

        let mut out = Vec::with_capacity(system.len() + 1 + (rest.len() - keep_from));
        out.extend(system.iter().cloned());
        if keep_from > 0 {
            out.push(marker);
        }
        out.extend(rest[keep_from..].iter().cloned());
        out
    }
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
}
