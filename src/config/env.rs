//! The `~/.grace/.env` key store — the single owner of that file.
//!
//! API keys are persisted here (one `KEY=value` per line) so they survive
//! across invocations without ever touching shell rc files. Startup loads
//! it into the process environment; the wizard and `/model` upsert into it.
//! It holds secrets, so the file is created and left mode 0600.

/// Where the onboarding wizard and `/model` persist API keys.
pub fn env_file_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join(".env")
}

/// Parse one `.env` line into `(key, value)`, tolerating an `export `
/// prefix and matched single or double quotes around the value. `None` for
/// blank and comment lines, for lines without `=`, and for empty values —
/// empty means absent, not "set to nothing".
pub fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let (key, value) = line.split_once('=')?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return None;
    }
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .map(str::to_string)
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .map(str::to_string)
        })
        .unwrap_or_else(|| value.to_string());
    if value.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Load the .env into the process environment, only where not already set
/// — the real environment always wins.
pub fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(env_file_path()) else {
        return;
    };
    for line in text.lines() {
        if let Some((key, value)) = parse_env_line(line) {
            if std::env::var(&key).is_err() {
                std::env::set_var(key, value);
            }
        }
    }
}

/// Replace-or-append one key in a .env file, touching only that key's line
/// so the other keys (other providers' tokens) survive a rewrite. A line
/// counts as the same key with or without an `export ` prefix; the new line
/// is written plain, which the reader handles.
///
/// The file is created, and left, mode 0600: it holds API keys, and a
/// group/world-readable file is a leak.
pub fn upsert_env_file_at(path: &std::path::Path, key: &str, value: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        let key_of_line = line
            .trim()
            .strip_prefix("export ")
            .map(str::trim)
            .unwrap_or(line.trim())
            .split('=')
            .next()
            .unwrap_or("")
            .trim();
        let is_comment = line.trim_start().starts_with('#');
        if !replaced
            && !line.trim().is_empty()
            && !is_comment
            && key_of_line == key
        {
            out.push(format!("{key}={value}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(format!("{key}={value}"));
    }
    let mut text = out.join("\n");
    text.push('\n');
    std::fs::write(path, text)?;
    // A key file is 0600 whether it pre-existed or not.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Upsert into the standard `~/.grace/.env` (see [`upsert_env_file_at`]).
pub fn upsert_env_file(key: &str, value: &str) -> std::io::Result<()> {
    upsert_env_file_at(&env_file_path(), key, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_line_parsing_tolerates_export_prefix_and_quotes() {
        assert_eq!(
            parse_env_line("KEY=value"),
            Some(("KEY".to_string(), "value".to_string()))
        );
        assert_eq!(
            parse_env_line("  export KEY=value  "),
            Some(("KEY".to_string(), "value".to_string()))
        );
        assert_eq!(
            parse_env_line("KEY=\"double quoted\""),
            Some(("KEY".to_string(), "double quoted".to_string()))
        );
        assert_eq!(
            parse_env_line("KEY='single quoted'"),
            Some(("KEY".to_string(), "single quoted".to_string()))
        );
        assert_eq!(parse_env_line("KEY=keep  inner  spaces"), Some(("KEY".to_string(), "keep  inner  spaces".to_string())));
        // Empty values are absent, not "set to nothing".
        assert_eq!(parse_env_line("KEY="), None);
        assert_eq!(parse_env_line("KEY=''"), None);
        // Comments, blanks, and valueless lines are skipped.
        assert_eq!(parse_env_line("# KEY=commented"), None);
        assert_eq!(parse_env_line("   "), None);
        assert_eq!(parse_env_line("NOEQUALS"), None);
        assert_eq!(parse_env_line("=orphan value"), None);
    }

    fn scratch_env(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "grace_env_test_{}_{tag}.env",
            std::process::id()
        ))
    }

    #[test]
    fn upsert_touches_only_the_target_key_and_forces_0600() {
        use std::path::Path;
        let path = scratch_env("upsert");
        let _ = std::fs::remove_file(&path);
        // Existing file with another provider's key in the export form,
        // plus a comment and a blank line that must all survive.
        std::fs::write(
            &path,
            "# comment\nexport GITHUB_COPILOT_TOKEN=old-token\n\nOPENROUTER_API_KEY=or-key\n",
        )
        .unwrap();
        // Pre-existing world-readable mode must be tightened, not kept.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        upsert_env_file_at(Path::new(&path), "OPENROUTER_API_KEY", "or-key-v2").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after,
            "# comment\nexport GITHUB_COPILOT_TOKEN=old-token\n\nOPENROUTER_API_KEY=or-key-v2\n"
        );
        assert!(
            after.contains("GITHUB_COPILOT_TOKEN=old-token"),
            "an unrelated key must survive an upsert of a different key"
        );

        // Upserting a key in its `export ` form rewrites that very line.
        upsert_env_file_at(Path::new(&path), "GITHUB_COPILOT_TOKEN", "gh-key-v2").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("GITHUB_COPILOT_TOKEN=gh-key-v2"), "{after}");
        assert!(!after.contains("old-token"), "{after}");
        assert_eq!(after.lines().count(), 4, "no line may be duplicated: {after}");

        // The file must be 0600 (keys inside).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the key file must be 0600, got {mode:o}");
        }

        // A brand-new key is appended.
        upsert_env_file_at(Path::new(&path), "GRACE_API_KEY", "gk").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("GRACE_API_KEY=gk"));
        let _ = std::fs::remove_file(&path);
    }
}
