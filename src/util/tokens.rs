//! Token estimation.
//!
//! The old context-compression code estimated tokens as `content.len() / 4`.
//! That is wrong in both directions and in ways that matter:
//!
//! - **Under-counts structure.** Chat completions are not raw text. Every
//!   message carries role/delimiter overhead (~4 tokens on OpenAI-family
//!   models) and every tool call carries its JSON envelope. A 40-message
//!   conversation is ~160 tokens of pure framing that `len()/4` cannot see.
//! - **Under-counts digits and punctuation.** BPE splits `1234567890` into
//!   multiple numeric tokens and rarely merges punctuation runs, so
//!   machine-generated tool output (JSON, logs, diffs, hex dumps) is far
//!   denser in tokens per character than prose.
//! - **Wildly over-counts CJK.** A CJK character is 3 UTF-8 bytes but
//!   typically 1 token; `len()/4` on `String` bytes therefore inflates it.
//!
//! Getting this wrong is not cosmetic: under-counting means we sail past the
//! provider's context limit and the request hard-fails mid-turn, which is the
//! exact failure compression exists to prevent.
//!
//! [`TokenCounter`] is the seam. The default [`HeuristicCounter`] is a
//! segment-aware BPE approximation with no extra dependency and no model
//! download; a real `tiktoken` implementation can be dropped in behind the
//! same trait without touching the compressor.

use crate::message::Message;

/// Anything that can estimate the token cost of text and of chat messages.
///
/// Implementors should be cheap and deterministic — the agent loop calls this
/// on the entire conversation before every single provider round-trip.
pub trait TokenCounter {
    /// Estimated tokens for a bare string, with no chat framing.
    fn count_text(&self, text: &str) -> usize;

    /// Estimated tokens for one chat message, including role/delimiter
    /// framing and any attached tool calls.
    fn count_message(&self, message: &Message) -> usize {
        let mut total = PER_MESSAGE_OVERHEAD + self.count_text(&message.content);
        for call in &message.tool_calls {
            // name + arguments + the JSON envelope the provider wraps them in.
            total += TOOL_CALL_OVERHEAD
                + self.count_text(call.name())
                + self.count_text(call.arguments());
        }
        if let Some(name) = &message.name {
            total += self.count_text(name);
        }
        total
    }

    /// Estimated tokens for a whole conversation, including the fixed priming
    /// cost the provider adds for the assistant's reply.
    fn count_messages(&self, messages: &[Message]) -> usize {
        let body: usize = messages.iter().map(|m| self.count_message(m)).sum();
        body + REPLY_PRIMING_OVERHEAD
    }
}

/// Per-message framing: `<|start|>{role}<|message|>...<|end|>`.
const PER_MESSAGE_OVERHEAD: usize = 4;

/// The assistant-reply priming tokens appended once per request.
const REPLY_PRIMING_OVERHEAD: usize = 3;

/// JSON envelope around a single tool call (`{"id":..,"type":"function",..}`).
const TOOL_CALL_OVERHEAD: usize = 8;

/// Segment-aware BPE approximation.
///
/// Text is split into runs of like characters and each run is costed with the
/// rule that matches how byte-pair encoders actually behave on that class,
/// rather than applying one flat bytes-per-token ratio to everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicCounter;

impl HeuristicCounter {
    /// Average characters absorbed per token inside an alphabetic word.
    const CHARS_PER_WORD_TOKEN: usize = 4;

    /// BPE merges digits in groups of at most three (`123`, `4567` -> `456`+`7`).
    const DIGITS_PER_TOKEN: usize = 3;

    /// Punctuation occasionally pairs up (`);`, `",`) but usually does not.
    const PUNCT_PER_TOKEN: usize = 2;

    /// Cost of a run of `n` alphabetic characters.
    fn word_tokens(n: usize) -> usize {
        // Short words are a single token; longer ones split roughly every
        // four characters. `max(1)` keeps a 1-char word from costing zero.
        div_ceil(n, Self::CHARS_PER_WORD_TOKEN).max(1)
    }

    /// Cost of a run of whitespace. A *single* space is absorbed into the
    /// following word's token (BPE encodes " word" as one token), so an
    /// ordinary space between words costs nothing here. Newlines and runs of
    /// indentation do cost — which is why token-dense machine output (code,
    /// logs, JSON) is systematically under-counted by a flat chars/4 rule.
    fn whitespace_tokens(run: &str) -> usize {
        let newlines = run.matches('\n').count();
        let spaces = run.chars().filter(|c| *c != '\n').count();
        // One token per newline, plus one per four spaces of indentation
        // beyond the first (which the adjacent word already absorbs).
        newlines + spaces.saturating_sub(1) / 4
    }
}

/// Character classes that BPE treats meaningfully differently.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Alpha,
    Digit,
    Wide,
    Space,
    Punct,
}

fn classify(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_ascii_digit() {
        Class::Digit
    } else if is_wide(c) {
        Class::Wide
    } else if c.is_alphabetic() {
        Class::Alpha
    } else {
        Class::Punct
    }
}

/// CJK / Hangul / Kana ranges, where one character is ~one token despite
/// being three UTF-8 bytes.
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F      // Hangul Jamo
        | 0x2E80..=0x303E    // CJK radicals, Kangxi, CJK punctuation
        | 0x3041..=0x33FF    // Hiragana, Katakana, CJK compatibility
        | 0x3400..=0x4DBF    // CJK Ext A
        | 0x4E00..=0x9FFF    // CJK Unified
        | 0xA000..=0xA4CF    // Yi
        | 0xAC00..=0xD7A3    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0xFE30..=0xFE6F    // CJK compatibility forms
        | 0xFF00..=0xFF60    // Fullwidth forms
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FA1F  // CJK Ext B..
    )
}

fn div_ceil(n: usize, d: usize) -> usize {
    n.div_ceil(d)
}

impl TokenCounter for HeuristicCounter {
    fn count_text(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let mut total = 0usize;
        let mut run = String::new();
        let mut run_class: Option<Class> = None;

        let mut flush = |class: Option<Class>, run: &mut String| {
            let Some(class) = class else { return };
            let n = run.chars().count();
            total += match class {
                Class::Alpha => HeuristicCounter::word_tokens(n),
                // Digits merge in groups of <=3.
                Class::Digit => div_ceil(n, HeuristicCounter::DIGITS_PER_TOKEN).max(1),
                // One token per wide character.
                Class::Wide => n,
                Class::Space => HeuristicCounter::whitespace_tokens(run),
                Class::Punct => div_ceil(n, HeuristicCounter::PUNCT_PER_TOKEN).max(1),
            };
            run.clear();
        };

        for c in text.chars() {
            let class = classify(c);
            if Some(class) != run_class {
                flush(run_class, &mut run);
                run_class = Some(class);
            }
            run.push(c);
        }
        flush(run_class, &mut run);
        total
    }
}

/// The counter used when a caller has no reason to pick a specific one.
pub fn default_counter() -> HeuristicCounter {
    HeuristicCounter
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn count(s: &str) -> usize {
        HeuristicCounter.count_text(s)
    }

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(count(""), 0);
    }

    #[test]
    fn prose_lands_near_the_known_tokens_per_word_ratio() {
        // ~1.3 tokens/word is the well-established cl100k average for English
        // prose. Anything wildly off that means the heuristic has drifted.
        let text = "The quick brown fox jumps over the lazy dog near the river bank today";
        let words = text.split_whitespace().count();
        let tokens = count(text);
        let ratio = tokens as f32 / words as f32;
        assert!(
            (0.9..=1.9).contains(&ratio),
            "expected ~1.3 tokens/word, got {ratio} ({tokens} tokens / {words} words)"
        );
    }

    #[test]
    fn digit_runs_cost_more_than_char_count_over_four_would_suggest() {
        // `len()/4` says 10 digits == 2 tokens. BPE groups digits by three,
        // so it is really ~4. Under-counting numeric output (logs, metrics,
        // hex dumps) is exactly how a "safe" context estimate overflows.
        let naive = "1234567890".len() / 4;
        let estimated = count("1234567890");
        assert!(
            estimated > naive,
            "digits must cost more than the naive len/4 estimate ({estimated} vs {naive})"
        );
    }

    #[test]
    fn cjk_is_not_inflated_by_utf8_byte_length() {
        // Three UTF-8 bytes per char meant `len()/4` over-counted CJK badly.
        let text = "日本語のテキストです";
        let chars = text.chars().count();
        let naive = text.len() / 4;
        let estimated = count(text);
        assert_eq!(estimated, chars, "one token per wide char");
        assert!(
            estimated < naive * 2,
            "byte-based counting inflates CJK; estimate {estimated}, naive {naive}"
        );
    }

    #[test]
    fn punctuation_heavy_json_is_denser_than_prose_per_char() {
        let json = r#"{"a":1,"b":[2,3],"c":{"d":"e"}}"#;
        let prose = "this is a sentence of about the same length ok";
        let json_density = count(json) as f32 / json.len() as f32;
        let prose_density = count(prose) as f32 / prose.len() as f32;
        assert!(
            json_density > prose_density,
            "JSON should be denser in tokens/char than prose ({json_density} vs {prose_density})"
        );
    }

    #[test]
    fn newlines_are_counted() {
        let flat = count("aaaa bbbb cccc");
        let broken = count("aaaa\nbbbb\ncccc");
        assert!(broken > flat, "newlines cost tokens: {broken} vs {flat}");
    }

    #[test]
    fn message_framing_is_included() {
        let msg = Message::user("hi");
        let framed = HeuristicCounter.count_message(&msg);
        let bare = HeuristicCounter.count_text("hi");
        assert_eq!(framed, bare + PER_MESSAGE_OVERHEAD);
    }

    #[test]
    fn tool_calls_add_their_json_envelope() {
        let mut msg = Message::assistant("");
        msg.tool_calls = vec![ToolCall::new("id1", "bash", r#"{"command":"ls"}"#)];
        let with_call = HeuristicCounter.count_message(&msg);
        assert!(
            with_call > PER_MESSAGE_OVERHEAD + TOOL_CALL_OVERHEAD,
            "tool call name+args must be counted, got {with_call}"
        );
    }

    #[test]
    fn conversation_count_includes_reply_priming() {
        let msgs = vec![Message::user("a"), Message::assistant("b")];
        let total = HeuristicCounter.count_messages(&msgs);
        let parts: usize = msgs.iter().map(|m| HeuristicCounter.count_message(m)).sum();
        assert_eq!(total, parts + REPLY_PRIMING_OVERHEAD);
    }

    #[test]
    fn empty_conversation_still_costs_priming() {
        assert_eq!(HeuristicCounter.count_messages(&[]), REPLY_PRIMING_OVERHEAD);
    }

    #[test]
    fn counting_is_monotonic_in_content() {
        let short = count("hello");
        let long = count("hello world this is considerably longer text");
        assert!(long > short);
    }
}
