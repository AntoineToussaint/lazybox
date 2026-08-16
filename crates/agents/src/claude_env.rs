//! Prepare Claude Code's `~/.claude.json` so an unattended
//! (`--dangerously-skip-permissions`) launch doesn't stall on an
//! interactive startup gate no headless spawn can clear.
//!
//! `--dangerously-skip-permissions` bypasses tool-permission checks but
//! NOT the one-time consent screens Claude paints on startup, and lazybox
//! runs the agent in a PTY (not `-p`), so those screens render and an
//! autonomous spawn hangs on them with no human to answer (issue #256):
//!
//! - **Workspace trust** — "Do you trust the files in this folder?",
//!   shown the first time Claude runs in a directory. State lives
//!   per-project under `projects.<absolute-path>.hasTrustDialogAccepted`.
//! - **Onboarding** — the first-run setup flow, gated by the top-level
//!   `hasCompletedOnboarding` flag.
//!
//! Seeding both before launch makes Claude treat the worktree as trusted
//! and onboarding as done, so it drops straight to a ready composer. MCP
//! auth prompts and promotional interstitials are the same category of
//! blocker but aren't config-seedable (an MCP gate is suppressed at spawn
//! with `--strict-mcp-config`; a promo that still slips through is caught
//! by [`crate::detect`] surfacing it as `InputNeeded`).
//!
//! - **Bypass-permissions consent** — `--dangerously-skip-permissions`
//!   itself paints a one-time "Bypass Permissions mode" warning on every
//!   start; the flag does NOT suppress it. Claude clears it only when
//!   `skipDangerousModePermissionPrompt` is `true` in a settings source it
//!   reads (`~/.claude/settings.json` is `userSettings`), so
//!   [`seed_skip_dangerous_mode_prompt`] persists it there
//!   (anthropics/claude-code#25503). The per-session `--settings` file
//!   (`flagSettings`) carries the same key via
//!   [`crate::hook_settings::build_settings`], covering spawns whether or
//!   not that file is generated.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Settings key that suppresses Claude's one-time bypass-permissions
/// consent screen. Shared with [`crate::hook_settings`] so the two
/// seeding paths (user settings file here, per-session `--settings` file
/// there) never spell it differently.
pub(crate) const SKIP_DANGEROUS_MODE_KEY: &str = "skipDangerousModePermissionPrompt";

/// Prepare `~/.claude.json` for an unattended launch in `worktree`:
/// trust the worktree and mark onboarding complete. Best-effort: a
/// missing `$HOME` is treated as "nothing to do" (Ok), so callers only
/// log genuine IO/parse failures.
pub fn seed_unattended_env(worktree: &Path) -> io::Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let config_path = PathBuf::from(home).join(".claude.json");
    seed_unattended_env_in(&config_path, worktree)
}

/// Read-modify-write `config_path`, setting
/// `projects[worktree].hasTrustDialogAccepted = true` and the top-level
/// `hasCompletedOnboarding = true` while preserving every other field.
/// Writes atomically (temp + rename) so a concurrent Claude reader never
/// observes a torn file, and skips the write entirely when both are
/// already set (no needless rewrite, no clobbering a concurrent Claude
/// update). A missing config is created with a minimal structure; an
/// unparseable one is left untouched (returns `Err`).
fn seed_unattended_env_in(config_path: &Path, worktree: &Path) -> io::Result<()> {
    let mut root: Value = match std::fs::read_to_string(config_path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e),
    };

    let obj = root.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "claude.json root is not an object",
        )
    })?;

    let onboarding_done = obj.get("hasCompletedOnboarding") == Some(&Value::Bool(true));

    let projects = obj
        .entry("projects")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "claude.json `projects` is not an object",
            )
        })?;

    let key = worktree.to_string_lossy().into_owned();
    let entry = projects
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "claude.json project entry is not an object",
            )
        })?;

    let trust_done = entry.get("hasTrustDialogAccepted") == Some(&Value::Bool(true));
    if trust_done && onboarding_done {
        return Ok(());
    }
    entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    obj.insert("hasCompletedOnboarding".into(), Value::Bool(true));

    let serialized = serde_json::to_vec_pretty(&root)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = tmp_sibling(config_path)?;
    std::fs::write(&tmp, serialized)?;
    std::fs::rename(&tmp, config_path)
}

/// Persist the one-time bypass-permissions consent into
/// `~/.claude/settings.json` so an unattended
/// `--dangerously-skip-permissions` launch doesn't stall on the
/// "Bypass Permissions mode" warning. Best-effort like
/// [`seed_unattended_env`]: a missing `$HOME` is a no-op (Ok).
pub fn seed_skip_dangerous_mode_prompt() -> io::Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let settings_path = PathBuf::from(home).join(".claude").join("settings.json");
    seed_skip_dangerous_mode_prompt_in(&settings_path)
}

/// Read-modify-write `settings_path`, setting
/// `skipDangerousModePermissionPrompt = true` while preserving every
/// other key. Writes atomically (temp + rename) so a concurrent Claude
/// reader never sees a torn file, and skips the write when it is already
/// set. A missing file (and its `~/.claude` parent) is created; an
/// unparseable one is left untouched (returns `Err`).
fn seed_skip_dangerous_mode_prompt_in(settings_path: &Path) -> io::Result<()> {
    let mut root: Value = match std::fs::read_to_string(settings_path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e),
    };

    let obj = root.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "claude settings.json root is not an object",
        )
    })?;

    if obj.get(SKIP_DANGEROUS_MODE_KEY) == Some(&Value::Bool(true)) {
        return Ok(());
    }
    obj.insert(SKIP_DANGEROUS_MODE_KEY.into(), Value::Bool(true));

    let serialized = serde_json::to_vec_pretty(&root)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_sibling(settings_path)?;
    std::fs::write(&tmp, serialized)?;
    std::fs::rename(&tmp, settings_path)
}

/// `<config>.lazybox-tmp` next to `config_path`, for the atomic write.
fn tmp_sibling(config_path: &Path) -> io::Result<PathBuf> {
    let name = config_path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "config path has no file name")
    })?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".lazybox-tmp");
    Ok(config_path.with_file_name(tmp_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir under the OS temp dir. Process-id keyed so
    /// parallel test binaries don't collide; no randomness needed.
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lazybox-claude-trust-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn trusted(config: &Path, worktree: &str) -> bool {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
        v["projects"][worktree]["hasTrustDialogAccepted"] == Value::Bool(true)
    }

    fn onboarded(config: &Path) -> bool {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
        v["hasCompletedOnboarding"] == Value::Bool(true)
    }

    #[test]
    fn creates_config_and_seeds_trust_and_onboarding_when_missing() {
        let dir = scratch("missing");
        let config = dir.join(".claude.json");
        let _ = std::fs::remove_file(&config);
        let worktree = Path::new("/tmp/wt-a");

        seed_unattended_env_in(&config, worktree).unwrap();

        assert!(trusted(&config, "/tmp/wt-a"));
        assert!(onboarded(&config));
    }

    #[test]
    fn preserves_existing_content_and_other_projects() {
        let dir = scratch("preserve");
        let config = dir.join(".claude.json");
        std::fs::write(
            &config,
            r#"{"numStartups":7,"projects":{"/tmp/other":{"hasTrustDialogAccepted":true,"allowedTools":["Bash"]}}}"#,
        )
        .unwrap();

        seed_unattended_env_in(&config, Path::new("/tmp/wt-b")).unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        // Unrelated top-level + sibling project survive untouched.
        assert_eq!(v["numStartups"], Value::from(7));
        assert_eq!(
            v["projects"]["/tmp/other"]["allowedTools"][0],
            Value::from("Bash")
        );
        assert!(trusted(&config, "/tmp/other"));
        assert!(trusted(&config, "/tmp/wt-b"));
        assert!(onboarded(&config));
    }

    #[test]
    fn seeds_onboarding_even_when_worktree_already_trusted() {
        // Trust alone already set, but onboarding still pending — the
        // write must still fire to clear the onboarding gate.
        let dir = scratch("onboarding-only");
        let config = dir.join(".claude.json");
        std::fs::write(
            &config,
            r#"{"projects":{"/tmp/wt-e":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();

        seed_unattended_env_in(&config, Path::new("/tmp/wt-e")).unwrap();

        assert!(trusted(&config, "/tmp/wt-e"));
        assert!(onboarded(&config));
    }

    #[test]
    fn idempotent_when_already_trusted_and_onboarded() {
        let dir = scratch("idempotent");
        let config = dir.join(".claude.json");
        std::fs::write(
            &config,
            r#"{"hasCompletedOnboarding":true,"projects":{"/tmp/wt-c":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        let before = std::fs::read_to_string(&config).unwrap();

        seed_unattended_env_in(&config, Path::new("/tmp/wt-c")).unwrap();

        // No rewrite — byte-for-byte unchanged.
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    }

    fn skip_dangerous_set(settings: &Path) -> bool {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
        v[SKIP_DANGEROUS_MODE_KEY] == Value::Bool(true)
    }

    #[test]
    fn seeds_skip_dangerous_mode_creating_missing_settings() {
        let dir = scratch("skip-missing").join(".claude");
        let settings = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);

        seed_skip_dangerous_mode_prompt_in(&settings).unwrap();

        assert!(skip_dangerous_set(&settings));
    }

    #[test]
    fn seeds_skip_dangerous_mode_preserving_other_settings() {
        let dir = scratch("skip-preserve");
        let settings = dir.join("settings.json");
        std::fs::write(
            &settings,
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[]}]}}"#,
        )
        .unwrap();

        seed_skip_dangerous_mode_prompt_in(&settings).unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["model"], Value::from("opus"));
        assert!(v["hooks"]["Stop"].is_array());
        assert!(skip_dangerous_set(&settings));
    }

    #[test]
    fn skip_dangerous_mode_idempotent_when_already_set() {
        let dir = scratch("skip-idempotent");
        let settings = dir.join("settings.json");
        std::fs::write(&settings, format!("{{\"{SKIP_DANGEROUS_MODE_KEY}\":true}}")).unwrap();
        let before = std::fs::read_to_string(&settings).unwrap();

        seed_skip_dangerous_mode_prompt_in(&settings).unwrap();

        // No rewrite — byte-for-byte unchanged.
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), before);
    }

    #[test]
    fn skip_dangerous_mode_leaves_unparseable_settings_alone() {
        let dir = scratch("skip-malformed");
        let settings = dir.join("settings.json");
        std::fs::write(&settings, "not json {").unwrap();

        let err = seed_skip_dangerous_mode_prompt_in(&settings).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "not json {");
    }

    #[test]
    fn leaves_unparseable_config_alone() {
        let dir = scratch("malformed");
        let config = dir.join(".claude.json");
        std::fs::write(&config, "not json {").unwrap();

        let err = seed_unattended_env_in(&config, Path::new("/tmp/wt-d")).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Original content untouched.
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "not json {");
    }
}
