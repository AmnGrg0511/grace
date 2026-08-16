//! Integration tests for persistence: sessions, durable memory, and the
//! pre-flight recall that stitches them into a prompt.
//!
//! Every test uses its own scratch database. The real store lives at
//! `~/.grace/`, and a test that touched it would both pollute a user's data
//! and become order-dependent.

use grace::memory::Memory;
use grace::message::Message;
use grace::session::SessionStore;
use grace::skill::SkillStore;
use std::path::PathBuf;
use std::sync::Arc;

fn scratch_db(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "grace_session_it_{}_{tag}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "grace_session_it_dir_{}_{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---- session history --------------------------------------------------------

#[test]
fn history_survives_reopening_the_store() {
    // The concrete fix for "--chat forgets everything on exit": a new
    // SessionStore over the same file must see the prior turns.
    let path = scratch_db("persist");
    {
        let store = SessionStore::open(&path).unwrap();
        store.append("work", &Message::user("what is grace")).unwrap();
        store
            .append("work", &Message::assistant("a ReAct agent"))
            .unwrap();
    }
    let reopened = SessionStore::open(&path).unwrap();
    let history = reopened.load("work").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].content, "what is grace");
    assert_eq!(history[1].content, "a ReAct agent");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn history_is_returned_oldest_first_so_it_replays_in_order() {
    let path = scratch_db("order");
    let store = SessionStore::open(&path).unwrap();
    for i in 0..5 {
        store.append("s", &Message::user(format!("m{i}"))).unwrap();
    }
    let loaded = store.load("s").unwrap();
    let texts: Vec<&str> = loaded.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(texts, vec!["m0", "m1", "m2", "m3", "m4"]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn two_sessions_never_leak_into_each_other() {
    let path = scratch_db("isolation");
    let store = SessionStore::open(&path).unwrap();
    store.append("alpha", &Message::user("alpha secret")).unwrap();
    store.append("beta", &Message::user("beta secret")).unwrap();

    let alpha = store.load("alpha").unwrap();
    assert_eq!(alpha.len(), 1);
    assert!(alpha[0].content.contains("alpha"));
    assert!(store
        .load("beta")
        .unwrap()
        .iter()
        .all(|m| !m.content.contains("alpha")));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn empty_messages_are_not_persisted_as_noise() {
    let path = scratch_db("empty");
    let store = SessionStore::open(&path).unwrap();
    store.append("s", &Message::assistant("")).unwrap();
    assert!(store.load("s").unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn full_text_search_finds_turns_across_sessions() {
    let path = scratch_db("fts");
    let store = SessionStore::open(&path).unwrap();
    store
        .append("s1", &Message::user("we chose rustls for TLS"))
        .unwrap();
    store
        .append("s2", &Message::user("unrelated discussion of pastry"))
        .unwrap();

    let hits = store.search("rustls", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "s1");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_wildcard_or_empty_query_returns_everything_instead_of_an_fts_syntax_error() {
    // `MATCH "*"` is invalid FTS5 and used to surface as a raw SQL error.
    let path = scratch_db("wildcard");
    let store = SessionStore::open(&path).unwrap();
    store.append("s1", &Message::user("one")).unwrap();
    store.append("s2", &Message::user("two")).unwrap();
    assert_eq!(store.search("*", 10).unwrap().len(), 2);
    assert_eq!(store.search("", 10).unwrap().len(), 2);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn search_respects_its_limit() {
    let path = scratch_db("limit");
    let store = SessionStore::open(&path).unwrap();
    for i in 0..10 {
        store
            .append("s", &Message::user(format!("marker {i}")))
            .unwrap();
    }
    assert_eq!(store.search("marker", 3).unwrap().len(), 3);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn titles_are_stored_updated_and_fetched_in_bulk() {
    let path = scratch_db("titles");
    let store = SessionStore::open(&path).unwrap();
    store.append("s1", &Message::user("hi")).unwrap();
    store.append("s2", &Message::user("hi")).unwrap();

    store.set_title("s1", "debugging the stdin race").unwrap();
    store.set_title("s2", "planning the refactor").unwrap();
    // Overwriting must update, not duplicate.
    store.set_title("s1", "fixing the stdin race").unwrap();

    assert_eq!(
        store.get_title("s1").unwrap().as_deref(),
        Some("fixing the stdin race")
    );
    let bulk = store
        .get_titles(&["s1".to_string(), "s2".to_string()])
        .unwrap();
    assert_eq!(bulk.len(), 2);
    assert_eq!(store.get_title("never-titled").unwrap(), None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sessions_are_listed_most_recently_active_first() {
    let path = scratch_db("list");
    let store = SessionStore::open(&path).unwrap();
    store.append("older", &Message::user("hi")).unwrap();
    // Timestamps have one-second resolution, so a real gap is required.
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    store.append("newer", &Message::user("hi")).unwrap();

    let ids = store.list_sessions().unwrap();
    assert_eq!(ids[0], "newer");
    assert_eq!(ids.len(), 2);
    let _ = std::fs::remove_file(&path);
}

// ---- locking ----------------------------------------------------------------

#[test]
fn a_locked_session_is_skipped_when_picking_a_default() {
    // Two terminals must never silently interleave into the same history.
    let path = scratch_db("locking");
    let store = SessionStore::open(&path).unwrap();
    let held = format!("it-held-{}", std::process::id());
    let free = format!("it-free-{}", std::process::id());

    store.append(&held, &Message::user("hi")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    store.append(&free, &Message::user("hi")).unwrap();

    // `free` is most recent; simulate another process locking it
    // (PID 1 is init/systemd — always alive and never our process).
    let lock_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("locks");
    std::fs::create_dir_all(&lock_dir).unwrap();
    std::fs::write(lock_dir.join(format!("{free}.lock")), "1").unwrap();

    assert_eq!(
        grace::session::pick_default_session(&store).unwrap(),
        Some(held.clone()),
        "free is locked by PID 1, so picker should fall back to held"
    );

    let _ = std::fs::remove_file(lock_dir.join(format!("{free}.lock")));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn releasing_a_lock_makes_the_session_pickable_again() {
    let path = scratch_db("unlock");
    let store = SessionStore::open(&path).unwrap();
    let id = format!("it-unlock-{}", std::process::id());
    store.append(&id, &Message::user("hi")).unwrap();

    let lock_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("locks");
    std::fs::create_dir_all(&lock_dir).unwrap();

    // Simulate another process holding the lock.
    std::fs::write(lock_dir.join(format!("{id}.lock")), "1").unwrap();
    assert_eq!(
        grace::session::pick_default_session(&store).unwrap(),
        None,
        "locked by PID 1, so nothing is pickable"
    );

    // Release the lock.
    std::fs::remove_file(lock_dir.join(format!("{id}.lock"))).unwrap();
    assert_eq!(
        grace::session::pick_default_session(&store).unwrap(),
        Some(id),
        "after release, session should be pickable again"
    );
    let _ = std::fs::remove_file(&path);
}

// ---- durable memory ---------------------------------------------------------

#[test]
fn facts_survive_reopening_and_reach_the_prompt() {
    let dir = scratch_dir("memory");
    let db = dir.join("memory.db");
    let id = {
        let mem = Memory::open(&db).unwrap();
        mem.remember("user prefers concise answers").unwrap()
    };
    let mem = Memory::open(&db).unwrap();
    let block = mem.as_prompt_block().unwrap().unwrap();
    assert!(block.contains("user prefers concise answers"));

    assert!(mem.forget(id).unwrap());
    assert!(mem.as_prompt_block().unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forgetting_an_unknown_fact_reports_false_rather_than_erroring() {
    let dir = scratch_dir("forget");
    let mem = Memory::open(dir.join("memory.db")).unwrap();
    assert!(!mem.forget(9_999).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wikilinks_resolve_one_hop_between_facts() {
    let dir = scratch_dir("links");
    let mem = Memory::open(dir.join("memory.db")).unwrap();
    mem.remember("acme is the build tool we use")
        .unwrap();
    mem.remember("today we debug [[acme]] sequential moves")
        .unwrap();

    let resolved = mem
        .resolve_links("today we debug [[acme]] sequential moves")
        .unwrap();
    assert!(resolved.iter().any(|f| f.content.contains("build tool")));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- recall -----------------------------------------------------------------

#[test]
fn recall_surfaces_a_relevant_fact_without_being_asked() {
    // The user should not have to say "remember what we said about X".
    let dir = scratch_dir("recall");
    let mem = Memory::open(dir.join("memory.db")).unwrap();
    mem.remember("the deployment pipeline runs on jenkins")
        .unwrap();
    let skills = SkillStore::new(dir.join("skills"));

    let hits = grace::recall::recall("how does the jenkins pipeline work", &mem, &skills, None, 5);
    assert!(
        hits.iter().any(|h| h.snippet.contains("jenkins")),
        "expected a jenkins hit, got {hits:?}"
    );
    let block = grace::recall::as_prompt_block(&hits).unwrap();
    assert!(block.contains("jenkins"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recall_stays_silent_when_nothing_is_relevant() {
    // Injecting unrelated context wastes tokens and misleads the model.
    let dir = scratch_dir("recall_quiet");
    let mem = Memory::open(dir.join("memory.db")).unwrap();
    mem.remember("the deployment pipeline runs on jenkins")
        .unwrap();
    let skills = SkillStore::new(dir.join("skills"));

    let hits = grace::recall::recall("what is the capital of Peru", &mem, &skills, None, 5);
    assert!(
        grace::recall::as_prompt_block(&hits).is_none(),
        "unrelated prompt should inject nothing, got {hits:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recall_can_draw_on_past_sessions_too() {
    let dir = scratch_dir("recall_sessions");
    let mem = Memory::open(dir.join("memory.db")).unwrap();
    let skills = SkillStore::new(dir.join("skills"));
    let path = scratch_db("recall_sessions");
    #[allow(clippy::arc_with_non_send_sync)]
    let sessions = Arc::new(SessionStore::open(&path).unwrap());
    sessions
        .append(
            "prior",
            &Message::user("we decided the compressor keeps the system prompt"),
        )
        .unwrap();

    let hits = grace::recall::recall(
        "what did we decide about the compressor",
        &mem,
        &skills,
        Some(&sessions),
        5,
    );
    assert!(
        !hits.is_empty(),
        "prior session content should be recallable"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recall_surfaces_a_matching_skill_by_its_description() {
    // This is what fixes "it didn't know which skill to look for" without
    // making the model speculatively load every SKILL.md.
    let dir = scratch_dir("recall_skill");
    let mem = Memory::open(dir.join("memory.db")).unwrap();
    let skills_root = dir.join("skills");
    std::fs::create_dir_all(skills_root.join("perforce-review")).unwrap();
    std::fs::write(
        skills_root.join("perforce-review").join("SKILL.md"),
        "---\ndescription: Reviews a pending perforce changelist for correctness.\n---\n# Review\n",
    )
    .unwrap();
    let skills = SkillStore::new(&skills_root);

    let hits = grace::recall::recall("review my perforce changelist", &mem, &skills, None, 5);
    assert!(
        hits.iter().any(|h| h.snippet.contains("perforce") || h.label.contains("perforce")),
        "expected the perforce skill to surface, got {hits:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
