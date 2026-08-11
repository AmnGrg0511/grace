//! End-to-end integration: the crate's public API surface, and the paths a
//! real invocation actually takes.
//!
//! Where `agent_tests`, `tool_tests`, and `session_tests` each go deep on one
//! subsystem, this file goes wide — it asserts the modules *compose*: that the
//! CLI can parse into a config, that a config builds the registry the agent
//! expects, and that a full turn runs against that assembly with delegation,
//! tools, and compression all wired together.

use grace::config::{Config, RegistryOptions};
use grace::core::{
    run_turn_with_options, AgentEvent, ContextCompressionConfig, DelegationDepth, TurnOptions,
};
use grace::message::{Message, Role, ToolCall};
use grace::session::SessionStore;
use grace::transport::{FinishReason, ModelResponse, ProviderTransport, ToolSpec};
use grace::ui::cli::{Action, CliArgs};
use grace::util::Result;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

// ---- test double ------------------------------------------------------------

/// Replays a scripted sequence of responses, then repeats the last one.
struct Scripted {
    script: RefCell<Vec<ModelResponse>>,
    calls: Cell<usize>,
    window: Option<u32>,
}

impl Scripted {
    fn new(script: Vec<ModelResponse>) -> Self {
        Self {
            script: RefCell::new(script),
            calls: Cell::new(0),
            window: None,
        }
    }
    fn with_window(mut self, w: u32) -> Self {
        self.window = Some(w);
        self
    }
}

impl ProviderTransport for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn complete(&self, _m: &[Message], _t: &[ToolSpec], _model: &str) -> Result<ModelResponse> {
        let n = self.calls.get();
        self.calls.set(n + 1);
        let script = self.script.borrow();
        Ok(script[n.min(script.len() - 1)].clone())
    }
    fn context_window(&self) -> Option<u32> {
        self.window
    }
}

fn stop(text: &str) -> ModelResponse {
    ModelResponse {
        content: text.into(),
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> ModelResponse {
    ModelResponse {
        content: String::new(),
        tool_calls: vec![ToolCall::new(id, name, args)],
        finish_reason: FinishReason::ToolCalls,
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "grace_integration_{}_{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---- the public API surface -------------------------------------------------

#[test]
fn the_crate_root_re_exports_everything_an_embedder_needs() {
    // Compiling these paths *is* the assertion. A rename that quietly drops a
    // re-export breaks downstream users, not any test inside the crate.
    let _m: grace::Message = grace::Message::user("hi");
    let _r: grace::Role = grace::Role::User;
    let _c: grace::ToolCall = grace::ToolCall::new("i", "n", "{}");
    let _reg: grace::ToolRegistry = grace::ToolRegistry::new();
    let _fr: grace::FinishReason = grace::FinishReason::Stop;
    let _resp: grace::ModelResponse = grace::ModelResponse::default();
    let _spec = grace::ToolSpec {
        name: "n".into(),
        description: "d".into(),
        parameters: serde_json::json!({"type": "object"}),
    };
    let _err: grace::AgentError = grace::AgentError::Interrupted;
    let _opts: grace::TurnOptions = grace::TurnOptions::new();
    let _task: grace::SubTask = grace::SubTask::new("t");
    let _cc: grace::ContextCompressionConfig = grace::ContextCompressionConfig::default();
    let _res: grace::Result<u8> = Ok(1);
}

// ---- CLI parsing feeding real configuration ---------------------------------

#[test]
fn parsed_cli_flags_produce_a_working_config_and_transport() {
    let args = CliArgs::parse([
        "--base-url",
        "https://api.openai.com/v1",
        "--api-key",
        "sk-test",
        "--model",
        "gpt-4o-mini",
        "--max-iterations",
        "12",
        "--prompt",
        "hello",
    ]);
    assert!(!args.wants_chat());

    let config = Config::from_args(
        args.base_url.clone(),
        args.api_key.clone(),
        args.model.clone(),
        args.max_iterations.unwrap(),
        None,
    )
    .unwrap();

    assert_eq!(config.model(), "gpt-4o-mini");
    assert_eq!(config.max_iterations, 12);
    assert_eq!(config.build_transport().unwrap().name(), "openai-http");
}

#[test]
fn a_terminal_flag_short_circuits_before_any_provider_is_needed() {
    // `--help` and friends must never require a configured model.
    for (argv, expected) in [
        (vec!["--help"], Action::Help),
        (vec!["--list-skins"], Action::ListSkins),
        (vec!["--list-sessions"], Action::ListSessions),
    ] {
        let args = CliArgs::parse(argv);
        assert_eq!(args.action, Some(expected));
    }
}

// ---- assembled registry -----------------------------------------------------

#[test]
fn the_full_registry_contains_builtins_skills_sessions_and_delegation() {
    // Delegation registration used to live in main.rs, so any other entry
    // point silently built a Grace that could not delegate.
    let dir = scratch("registry");
    let db = dir.join("sessions.db");
    #[allow(clippy::arc_with_non_send_sync)]
    let sessions = Arc::new(SessionStore::open(&db).unwrap());

    let opts = RegistryOptions::new(dir.join("skills"), dir.join("tools"))
        .with_sessions(Arc::clone(&sessions))
        .with_transport(Rc::new(Scripted::new(vec![stop("ok")])));
    let reg = Config::build_registry_full(&opts, DelegationDepth::ROOT);

    for expected in [
        "bash",
        "read",
        "write",
        "edit",
        "list_skills",
        "load_skill",
        "session_search",
        "delegate",
    ] {
        assert!(reg.get(expected).is_some(), "{expected} is missing");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_tool_in_the_full_registry_advertises_a_provider_valid_schema() {
    let dir = scratch("schemas");
    let opts = RegistryOptions::new(dir.join("skills"), dir.join("tools"))
        .with_transport(Rc::new(Scripted::new(vec![stop("ok")])));
    let reg = Config::build_registry_full(&opts, DelegationDepth::ROOT);

    for spec in reg.specs() {
        assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
        assert!(!spec.description.is_empty(), "{}", spec.name);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a full turn over the assembled system ----------------------------------

#[test]
fn a_full_turn_runs_tools_from_the_assembled_registry() {
    let dir = scratch("fullturn");
    let target = dir.join("out.txt");

    let transport = Scripted::new(vec![
        tool_call(
            "c1",
            "write",
            &serde_json::json!({"path": target.to_str().unwrap(), "content": "written by grace"})
                .to_string(),
        ),
        stop("I wrote the file."),
    ]);
    let opts = RegistryOptions::new(dir.join("skills"), dir.join("tools"));
    let reg = Config::build_registry_full(&opts, DelegationDepth::ROOT);

    let mut messages = vec![
        Message::system("you are grace"),
        Message::user("write a file"),
    ];
    let outcome = run_turn_with_options(&transport, &reg, &mut messages, 8, TurnOptions::new())
        .unwrap();

    assert_eq!(outcome.answer, "I wrote the file.");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "written by grace"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delegation_works_end_to_end_through_the_tool_registry() {
    // The parent calls `delegate`; a sub-agent runs a fresh loop against the
    // same transport and its answer comes back as the tool result.
    let dir = scratch("delegate");
    let transport = Rc::new(Scripted::new(vec![
        // Parent asks to delegate.
        tool_call(
            "c1",
            "delegate",
            r#"{"task":"count to three","max_iterations":5}"#,
        ),
        // The sub-agent's single response.
        stop("one two three"),
        // Parent's final answer.
        stop("the sub-agent counted: one two three"),
    ]));

    let opts = RegistryOptions::new(dir.join("skills"), dir.join("tools"))
        .with_transport(Rc::clone(&transport) as Rc<dyn ProviderTransport>);
    let reg = Config::build_registry_full(&opts, DelegationDepth::ROOT);

    let mut messages = vec![Message::user("delegate a counting task")];
    let outcome = run_turn_with_options(
        transport.as_ref(),
        &reg,
        &mut messages,
        8,
        TurnOptions::new(),
    )
    .unwrap();

    assert_eq!(outcome.answer, "the sub-agent counted: one two three");
    let tool_result = messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("the delegate result must be recorded");
    assert_eq!(tool_result.content, "one two three");
    assert_eq!(tool_result.name.as_deref(), Some("delegate"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_sub_agent_is_not_handed_the_delegate_tool_past_the_depth_cap() {
    // Structural termination: at the cap the tool simply is not registered,
    // so recursion cannot continue even if the model keeps asking.
    let dir = scratch("depthcap");
    let opts = RegistryOptions::new(dir.join("skills"), dir.join("tools"))
        .with_transport(Rc::new(Scripted::new(vec![stop("ok")])));

    let root = Config::build_registry_full(&opts, DelegationDepth::ROOT);
    assert!(root.get("delegate").is_some());

    let capped = Config::build_registry_full(
        &opts,
        DelegationDepth(grace::core::MAX_DELEGATION_DEPTH),
    );
    assert!(capped.get("delegate").is_none());
    assert!(
        capped.get("read").is_some(),
        "only delegation is withdrawn, not the whole toolset"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compression_and_tool_calls_coexist_in_one_turn() {
    // The two features touch the same message vector; compressing mid-turn
    // must not orphan a tool result from the call that produced it.
    let dir = scratch("compress_tools");
    let transport = Scripted::new(vec![
        tool_call("c1", "bash", r#"{"command":"echo hi"}"#),
        stop("done"),
    ])
    .with_window(3_000);

    let opts = RegistryOptions::new(dir.join("skills"), dir.join("tools"));
    let reg = Config::build_registry_full(&opts, DelegationDepth::ROOT);

    let mut messages = vec![Message::system("sys")];
    for i in 0..80 {
        messages.push(Message::user(format!("{i} {}", vec!["word"; 40].join(" "))));
    }

    let cfg = ContextCompressionConfig::default();
    let compressed = Cell::new(false);
    let mut sink = |e: AgentEvent<'_>| {
        if matches!(e, AgentEvent::ContextCompressed { .. }) {
            compressed.set(true);
        }
    };

    let outcome = run_turn_with_options(
        &transport,
        &reg,
        &mut messages,
        8,
        TurnOptions::new()
            .with_events(&mut sink)
            .with_compression(&cfg),
    )
    .unwrap();

    assert_eq!(outcome.answer, "done");
    assert!(compressed.get(), "compression should have fired");
    // No tool message may be left without its preceding assistant request.
    for (i, m) in messages.iter().enumerate() {
        if m.role == Role::Tool {
            assert_eq!(
                messages.get(i.wrapping_sub(1)).map(|p| p.role),
                Some(Role::Assistant),
                "orphaned tool message at {i}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_conversation_can_be_persisted_and_resumed_across_store_instances() {
    // The full round trip a `--session` run performs.
    let dir = scratch("resume");
    let db = dir.join("sessions.db");
    let transport = Scripted::new(vec![stop("first answer"), stop("second answer")]);
    let reg = grace::ToolRegistry::new();

    {
        let store = SessionStore::open(&db).unwrap();
        let mut messages = vec![Message::system("sys"), Message::user("first question")];
        store.append("s", &Message::user("first question")).unwrap();
        let outcome =
            run_turn_with_options(&transport, &reg, &mut messages, 4, TurnOptions::new()).unwrap();
        store.append("s", &Message::assistant(outcome.answer)).unwrap();
    }

    // A fresh process would reopen the store and replay history.
    let store = SessionStore::open(&db).unwrap();
    let mut messages = vec![Message::system("sys")];
    messages.extend(store.load("s").unwrap());
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2].content, "first answer");

    messages.push(Message::user("second question"));
    let outcome =
        run_turn_with_options(&transport, &reg, &mut messages, 4, TurnOptions::new()).unwrap();
    assert_eq!(outcome.answer, "second answer");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- shipped skills ---------------------------------------------------------

#[test]
fn the_checked_in_skills_directory_matches_the_in_binary_defaults() {
    // `skills/` at the repo root exists so the defaults are reviewable in a
    // diff rather than buried in a Rust string literal. Two copies of the
    // same text is exactly the kind of thing that silently drifts, so pin it.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills");
    for (name, expected) in grace::skill::defaults::DEFAULT_SKILLS {
        let path = root.join(name).join("SKILL.md");
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is missing: {e}", path.display()));
        assert_eq!(
            on_disk,
            *expected,
            "skills/{name}/SKILL.md has drifted from src/skill/defaults.rs"
        );
    }
}

#[test]
fn the_shipped_skills_are_loadable_through_the_skill_tools() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills");
    let reg = Config::build_registry_with_skills(&root);
    let listed = reg.execute("list_skills", "{}").unwrap();
    assert!(listed.contains("grace-agent"));
    assert!(listed.contains("memory-update"));
    assert!(listed.contains("skill-author"));

    let loaded = reg.execute("load_skill", r#"{"name":"grace-agent"}"#).unwrap();
    assert!(loaded.contains("Architecture"));
}

#[test]
fn the_grace_agent_skill_documents_the_current_module_layout() {
    // This skill is what Grace loads to reason about its own codebase. If it
    // still describes the pre-refactor flat layout, it actively misleads.
    let (_, body) = grace::skill::defaults::DEFAULT_SKILLS
        .iter()
        .find(|(n, _)| *n == "grace-agent")
        .unwrap();
    for path in [
        "core/",
        "transport/",
        "tools/",
        "src/tools/trait.rs",
        "src/ui/skin.rs",
    ] {
        assert!(body.contains(path), "grace-agent should mention {path}");
    }
    assert!(
        !body.contains("src/tool.rs"),
        "grace-agent still references the pre-refactor flat layout"
    );
    assert!(body.contains("delegate"), "delegation should be documented");
}
