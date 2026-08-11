//! `grace` binary — a thin entry point.
//!
//! All behaviour lives in the library ([`grace::ui::cli`]), so it is reachable
//! from a test rather than only from a real process invocation. This file
//! exists to satisfy `fn main` and nothing else.
//!
//! Usage:
//! ```text
//! # Interactive chat (persists across turns and restarts):
//! grace --chat --session work
//!
//! # One-shot against any OpenAI-compatible endpoint:
//! grace --base-url https://api.openai.com/v1 --api-key "$KEY" \
//!       --model gpt-4o-mini --prompt "list files"
//!
//! # OpenRouter:
//! export OPENROUTER_API_KEY=sk-or-...
//! grace --openrouter --model tencent/hy3:free --prompt "list files"
//!
//! # Durable memory (injected into every later prompt):
//! grace --remember "user prefers concise answers"
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    grace::ui::cli::main()
}
