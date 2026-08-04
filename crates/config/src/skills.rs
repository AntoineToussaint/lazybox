//! Agent-skill scaffolding: write a `.claude/skills/<name>/SKILL.md`
//! folder into a repo from a plain-language request.
//!
//! A snippet is a single human-triggered prompt; a skill is a
//! model-triggered, progressively-disclosed capability the agent picks
//! itself off its `description`. When an "Ask Lazybox" request is
//! genuinely multi-step (or wants bundled scripts/reference files), a
//! `SKILL.md` folder is the right artifact, not a snippet body (#799;
//! see `docs/snippets-vs-skills.md`).
//!
//! This module owns only the safe write: name validation, the
//! frontmatter render, and a refuse-don't-clobber scaffold. Whether a
//! request should become a skill vs a snippet is decided upstream (the
//! help agent classifies it and proposes the matching action).

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error(
        "invalid skill name {0:?} — use lowercase letters, digits and hyphens (e.g. `code-review`)"
    )]
    InvalidName(String),
    #[error("a skill named {0:?} already exists at {1}")]
    AlreadyExists(String, PathBuf),
    #[error("missing description — a skill needs one so the agent knows when to use it")]
    MissingDescription,
    #[error("missing body — the SKILL.md instructions cannot be empty")]
    MissingBody,
    #[error("failed to render skill frontmatter: {0}")]
    Frontmatter(#[from] serde_yaml::Error),
    #[error("failed to write skill: {0}")]
    Io(#[from] std::io::Error),
}

/// A skill's home directory: `<repo_root>/.claude/skills/<name>`.
pub fn skill_dir(repo_root: &Path, name: &str) -> PathBuf {
    repo_root.join(".claude").join("skills").join(name)
}

/// The `SKILL.md` path inside a skill's directory.
pub fn skill_md_path(repo_root: &Path, name: &str) -> PathBuf {
    skill_dir(repo_root, name).join("SKILL.md")
}

/// A skill name must be a clean, portable folder name: lowercase
/// ASCII letters, digits and interior hyphens only, no leading or
/// trailing hyphen. This keeps the on-disk folder predictable and
/// rules out any path traversal (`.`, `/`, `..`) before it reaches a
/// filesystem join.
pub fn validate_skill_name(name: &str) -> Result<(), SkillError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(SkillError::InvalidName(name.to_string()))
    }
}

/// Render a `SKILL.md`: YAML frontmatter (`name` + `description`,
/// serialized so any punctuation in the description is escaped
/// correctly) followed by the markdown instruction body.
pub fn render_skill_md(name: &str, description: &str, body: &str) -> Result<String, SkillError> {
    let mut front = serde_yaml::Mapping::new();
    front.insert("name".into(), name.into());
    front.insert("description".into(), description.into());
    let yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(front))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", body.trim_end()))
}

/// Scaffold a `.claude/skills/<name>/SKILL.md` folder under `repo_root`
/// and return the `SKILL.md` path written. Validates the name,
/// requires a non-empty description and body, and refuses to overwrite
/// an existing skill (a scaffold must never clobber hand-authored
/// bundled scripts sitting beside a `SKILL.md`). The write is atomic
/// (sibling tmp + rename) so a crash mid-write can't leave a truncated
/// `SKILL.md` behind.
///
/// Used by the Ask Lazybox help agent's `scaffold_skill` action (#799):
/// the agent proposes a skill, the TUI confirms it with a preview, and
/// this applies it natively.
pub fn scaffold_skill(
    repo_root: &Path,
    name: &str,
    description: &str,
    body: &str,
) -> Result<PathBuf, SkillError> {
    validate_skill_name(name)?;
    if description.trim().is_empty() {
        return Err(SkillError::MissingDescription);
    }
    if body.trim().is_empty() {
        return Err(SkillError::MissingBody);
    }
    let path = skill_md_path(repo_root, name);
    if path.exists() {
        return Err(SkillError::AlreadyExists(name.to_string(), path));
    }
    let contents = render_skill_md(name, description.trim(), body)?;
    let dir = skill_dir(repo_root, name);
    std::fs::create_dir_all(&dir)?;
    write_atomically(&path, contents.as_bytes())?;
    Ok(path)
}

/// Write `bytes` to `path` atomically: a per-call sibling `.tmp`, then
/// a rename. Mirrors the snippets writer so two writers can't clash on
/// a fixed tmp name.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SkillError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("md.tmp.{}.{seq}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique tmp repo root per call. Mirrors the snippets tests —
    /// no `tempfile` dependency just for tests.
    fn tmp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lazybox-skills-test-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn valid_names_pass_and_bad_ones_fail() {
        for good in ["code-review", "deploy", "run-ci-2"] {
            assert!(validate_skill_name(good).is_ok(), "{good} should pass");
        }
        for bad in [
            "",
            "-lead",
            "trail-",
            "Upper",
            "has space",
            "dot.name",
            "../escape",
            "a/b",
        ] {
            assert!(
                matches!(validate_skill_name(bad), Err(SkillError::InvalidName(_))),
                "{bad:?} should fail",
            );
        }
    }

    #[test]
    fn render_wraps_frontmatter_and_body() {
        let md = render_skill_md("code-review", "Review a diff: bugs, style", "Do it.").unwrap();
        assert!(md.starts_with("---\n"), "frontmatter fence: {md:?}");
        assert!(md.contains("name: code-review"));
        // A description with a colon must round-trip as valid YAML.
        assert!(md.contains("description: 'Review a diff: bugs, style'"));
        assert!(md.trim_end().ends_with("Do it."));
        // The frontmatter closes before the body begins.
        let close = md.find("---\n\n").expect("closing fence");
        assert!(close > 4, "closing fence must follow the opening one");
    }

    #[test]
    fn scaffold_writes_folder_and_skill_md() {
        let root = tmp_root("x");
        let path = scaffold_skill(&root, "code-review", "Review the diff", "Review it.").unwrap();
        assert_eq!(path, skill_md_path(&root, "code-review"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("name: code-review"));
        assert!(written.contains("Review it."));
    }

    #[test]
    fn scaffold_refuses_to_overwrite_existing_skill() {
        let root = tmp_root("x");
        scaffold_skill(&root, "dup", "first", "one").unwrap();
        let err = scaffold_skill(&root, "dup", "second", "two").unwrap_err();
        assert!(matches!(err, SkillError::AlreadyExists(_, _)));
        // The original body is untouched.
        let written = std::fs::read_to_string(skill_md_path(&root, "dup")).unwrap();
        assert!(written.contains("one"));
        assert!(!written.contains("two"));
    }

    #[test]
    fn scaffold_requires_description_and_body() {
        let root = tmp_root("x");
        assert!(matches!(
            scaffold_skill(&root, "x", "  ", "body"),
            Err(SkillError::MissingDescription)
        ));
        assert!(matches!(
            scaffold_skill(&root, "x", "desc", "   "),
            Err(SkillError::MissingBody)
        ));
        // Nothing was written for either rejected attempt.
        assert!(!skill_dir(&root, "x").exists());
    }

    #[test]
    fn scaffold_rejects_a_bad_name_before_touching_disk() {
        let root = tmp_root("x");
        assert!(matches!(
            scaffold_skill(&root, "../escape", "d", "b"),
            Err(SkillError::InvalidName(_))
        ));
        assert!(!root.join(".claude").exists());
    }
}
