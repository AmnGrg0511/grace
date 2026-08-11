//! The `list_skills` and `load_skill` tools.
//!
//! Skills are discovered and loaded on demand rather than concatenated into
//! every system prompt. That is the entire design: a dozen skills injected up
//! front is a dozen skills' worth of tokens spent on every turn, most of them
//! irrelevant. The model sees names and one-line descriptions, and pulls in the
//! full procedure only when a task actually matches.

use super::store::SkillStore;
use crate::tools::r#trait::Tool;
use crate::util::{AgentError, Result};

/// Tool exposing skill discovery + loading to the model.
pub struct ListSkillsTool {
    pub store: std::sync::Arc<SkillStore>,
}

impl Tool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }
    fn description(&self) -> &str {
        "List available skill names that can be loaded with load_skill."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn run(&self, _args: &serde_json::Value) -> Result<String> {
        let names = self.store.list();
        if names.is_empty() {
            Ok("no skills available".to_string())
        } else {
            Ok(names.join("\n"))
        }
    }
}

/// Tool that loads one skill's content by name.
pub struct LoadSkillTool {
    pub store: std::sync::Arc<SkillStore>,
}

impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill"
    }
    fn description(&self) -> &str {
        "Load the full content of a named skill (see list_skills for available names)."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name (directory under skills/)."}
            },
            "required": ["name"],
        })
    }
    fn run(&self, args: &serde_json::Value) -> Result<String> {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AgentError::Tool("missing string argument 'name'".to_string()))?;
        self.store.load(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn scratch(tag: &str) -> (PathBuf, Arc<SkillStore>) {
        let dir = std::env::temp_dir().join(format!(
            "grace_skill_load_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("greet")).unwrap();
        std::fs::write(dir.join("greet").join("SKILL.md"), "# Greet\nSay hello.").unwrap();
        let store = Arc::new(SkillStore::new(&dir));
        (dir, store)
    }

    #[test]
    fn list_skills_returns_available_names() {
        let (dir, store) = scratch("list");
        let out = ListSkillsTool { store }
            .run(&serde_json::json!({}))
            .unwrap();
        assert_eq!(out, "greet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_skills_says_so_when_there_are_none() {
        // "no skills available" is an answer; an error would make the model
        // think the tool is broken and retry it.
        let store = Arc::new(SkillStore::new("/nonexistent/root"));
        let out = ListSkillsTool { store }
            .run(&serde_json::json!({}))
            .unwrap();
        assert_eq!(out, "no skills available");
    }

    #[test]
    fn load_skill_returns_the_full_procedure() {
        let (dir, store) = scratch("load");
        let out = LoadSkillTool { store }
            .run(&serde_json::json!({"name": "greet"}))
            .unwrap();
        assert!(out.contains("Say hello."));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_skill_requires_a_name() {
        let (dir, store) = scratch("noname");
        let err = LoadSkillTool { store }
            .run(&serde_json::json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("missing string argument 'name'"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_skill_refuses_path_traversal() {
        // A skill name reaching outside the skills root would turn a
        // discovery tool into an arbitrary file read.
        let (dir, store) = scratch("traversal");
        assert!(LoadSkillTool { store }
            .run(&serde_json::json!({"name": "../../etc/passwd"}))
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_tools_expose_object_schemas() {
        let (dir, store) = scratch("schema");
        assert_eq!(
            ListSkillsTool {
                store: Arc::clone(&store)
            }
            .parameters()["type"],
            "object"
        );
        let p = LoadSkillTool { store }.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "name");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
