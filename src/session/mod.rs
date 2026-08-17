//! Session persistence — chat history that survives restarts, searchable.
//!
//! ```text
//! store.rs  SessionStore: SQLite + FTS5 (append, load, search, list, titles)
//! lock.rs   cross-terminal session locking
//! title.rs  model-generated session titles
//! ```
//!
//! History is stored, not just replayed: the FTS5 index is what makes
//! `--search-sessions` and the `session_search` tool possible, so the agent
//! can answer "what did we decide last time" instead of only resuming.

pub mod lock;
pub mod store;
pub mod title;

pub use lock::{pick_default_session, validate_session_id, SessionLock};
pub use store::SessionStore;
pub use title::generate_title;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    #[test]
    fn the_store_lock_and_title_apis_are_reachable_from_the_module_root() {
        let path = std::env::temp_dir().join(format!(
            "grace_session_mod_test_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        store.append("s", &Message::user("hi")).unwrap();

        // Split across three files, but one coherent API from the outside.
        assert_eq!(store.load("s").unwrap().len(), 1);
        assert_eq!(pick_default_session(&store).unwrap().as_deref(), Some("s"));
        let lock = SessionLock::acquire("s").unwrap();
        // Lock file exists and contains our PID — but `is_held` returns false
        // because it means "held by another process", not "owned by us".
        assert!(lock.path.exists());
        assert!(!SessionLock::is_held("s"));
        drop(lock);

        let _: fn(&dyn crate::transport::ProviderTransport, &str, &str) -> Option<String> =
            generate_title;
        let _ = std::fs::remove_file(&path);
    }
}
