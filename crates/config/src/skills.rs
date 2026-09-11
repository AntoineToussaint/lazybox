//! Agent **Skills** — discovery and scaffolding.
//!
//! A snippet is human-triggered; a skill is model-triggered — the agent
//! reads each skill's `description` and decides *itself* whether to
//! invoke it, progressively loading the `SKILL.md` body and any bundled
//! files. This module owns two filesystem-facing halves of lazybox's
//! skill support (see `docs/snippets-vs-skills.md`):
//!
//! - **Discovery** (#797, #1671): scan the [Agent
//!   Skills](https://agentskills.io) roots the focused agent reads so
//!   the `]]` skills picker can surface them and let the user trigger
//!   one *explicitly*, gaining the deterministic-invocation + preview +
//!   Recent UX snippets enjoy.
//! - **Scaffolding** (#799): write a `.claude/skills/<name>/SKILL.md`
//!   folder from an "Ask Lazybox" request when the ask is genuinely
//!   multi-step (or wants bundled scripts/reference files).
//!
//! Layout: each skill is a folder holding a `SKILL.md` whose YAML
//! frontmatter carries `name` and `description`, optionally beside a
//! `scripts/` directory the agent may execute.

use std::io::BufRead;
use std::path::{Path, PathBuf};

// ── Discovery (#797) ────────────────────────────────────────────────

/// Where a discovered skill lives. Doubles as the picker's category so
/// repo skills group above user skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Repo,
    User,
}

impl SkillScope {
    /// Category / group label shown in the picker.
    pub fn label(self) -> &'static str {
        match self {
            SkillScope::Repo => "Repo",
            SkillScope::User => "User",
        }
    }
}

/// One skill available to the focused agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The skill's invocation name — its frontmatter `name`, falling back
    /// to the folder name. This is what an explicit trigger names.
    pub name: String,
    /// The frontmatter `description` (empty when absent). This is the
    /// text the model matches on, and what the picker previews.
    pub description: String,
    pub scope: SkillScope,
    /// The folder the winning root resolved this name to. Shown in the
    /// picker so the user can go read the `SKILL.md` they are about to
    /// invoke.
    pub folder: PathBuf,
    /// The same name's folders in the *other* roots this agent reads.
    /// lazybox picks a winner by its own root order, but the agent
    /// resolves the name itself and may not order roots the same way, so
    /// a disclosure naming only [`Self::folder`] could describe a file
    /// that never loads. These are the other candidates to review.
    pub also_at: Vec<PathBuf>,
    /// Whether a `scripts/` directory is bundled — a directory-exists
    /// test, no claim about what the scripts do. A skill loads as a
    /// system-prompt fragment carrying the agent's full permissions, so
    /// this is the line between instructions and instructions that can
    /// run things (#1671).
    ///
    /// ORed across [`Self::folder`] and every [`Self::also_at`] twin: a
    /// name the agent might resolve to a scripts-bundling copy must
    /// never be presented as instructions-only just because lazybox's
    /// own precedence happened to pick the inert one.
    pub bundles_scripts: bool,
}

/// Which agents read a given skill root. lazybox spawns three agents and
/// they do not share every root: `.agents/` is the standard's shared
/// path, `.claude/` and `.codex/` belong to one agent each. Listing a
/// root its agent never reads means the picker offers a skill that
/// cannot load, and attributes a path and a `runs code` verdict from a
/// folder that has no bearing on the session (#1671).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootOwner {
    /// The Agent Skills standard's shared root — every agent reads it.
    Shared,
    /// An agent-specific root, keyed by the agent id lazybox spawned.
    Agent(&'static str),
}

impl RootOwner {
    /// Whether an agent session reads this root. An unknown agent
    /// (`None`) reads only the shared root — an agent lazybox cannot
    /// name is not evidence that it reads Claude's or Codex's private
    /// directory.
    fn read_by(self, agent_id: Option<&str>) -> bool {
        match self {
            RootOwner::Shared => true,
            RootOwner::Agent(owner) => agent_id == Some(owner),
        }
    }
}

/// The frontmatter fields we read; every other key is ignored.
#[derive(serde::Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Discover the skills an `agent_id` session rooted at `repo_root` can
/// actually load, per the [Agent Skills](https://agentskills.io)
/// standard (#1671).
///
/// Scans the repo tier (`.claude/skills`, `.agents/skills`) then the
/// user tier under `$HOME` (`.claude`, `.agents`, `.codex`), keeping
/// only the roots that agent reads: `.agents/` is the standard's shared
/// root, while `.claude/` and `.codex/` belong to one agent each. The
/// first root to claim a name wins it, so a repo skill shadows a user
/// skill and, inside a tier, the agent-specific root shadows the shared
/// one — the losers stay reviewable in [`Skill::also_at`] rather than
/// being dropped. Sorted by name, so the picker's key-sorted-rows
/// invariant holds.
pub fn discover_skills(repo_root: Option<&Path>, agent_id: Option<&str>) -> Vec<Skill> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    discover_skills_in(&skill_roots(repo_root, agent_id, home.as_deref()))
}

/// The ordered roots [`discover_skills`] scans, filtered by
/// [`RootOwner::read_by`] to the ones `agent_id` reads. `~/.codex/skills`
/// is Codex's historical root.
///
/// `home` is a parameter rather than read from `$HOME` here so the order
/// and the ownership filter are testable without reaching into the
/// machine running the test.
fn skill_roots(
    repo_root: Option<&Path>,
    agent_id: Option<&str>,
    home: Option<&Path>,
) -> Vec<(PathBuf, SkillScope)> {
    const TIERS: [(&str, SkillScope); 5] = [
        (".claude", SkillScope::Repo),
        (".agents", SkillScope::Repo),
        (".claude", SkillScope::User),
        (".agents", SkillScope::User),
        (".codex", SkillScope::User),
    ];
    const OWNERS: [RootOwner; 5] = [
        RootOwner::Agent("claude"),
        RootOwner::Shared,
        RootOwner::Agent("claude"),
        RootOwner::Shared,
        RootOwner::Agent("codex"),
    ];

    TIERS
        .iter()
        .zip(OWNERS)
        .filter(|(_, owner)| owner.read_by(agent_id))
        .filter_map(|((dir, scope), _)| {
            let base = match scope {
                SkillScope::Repo => repo_root?,
                SkillScope::User => home?,
            };
            Some((base.join(dir).join("skills"), *scope))
        })
        .collect()
}

/// Core scan, taking the ordered skill roots directly so tests can drive
/// it with temp dirs. The first root to carry a name wins it; the losers
/// are folded into the winner's [`Skill::also_at`] and OR their
/// `scripts/` surface into it rather than being dropped, so a shadowed
/// twin can neither vanish from review nor soften the winner's tag.
fn discover_skills_in(roots: &[(PathBuf, SkillScope)]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for (dir, scope) in roots {
        let mut found = Vec::new();
        scan_dir(dir, *scope, &mut found);
        // `read_dir` order is filesystem-defined, so sort before the
        // first-wins dedup — two folders can declare the same
        // frontmatter `name`, and which one lists must not depend on it.
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in found {
            match skills
                .iter_mut()
                .find(|existing| existing.name == skill.name)
            {
                Some(winner) => {
                    winner.bundles_scripts |= skill.bundles_scripts;
                    winner.also_at.push(skill.folder);
                }
                None => skills.push(skill),
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Append every `<dir>/<skill>/SKILL.md` folder as a [`Skill`]. A missing
/// or unreadable directory yields nothing.
fn scan_dir(dir: &Path, scope: SkillScope, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let folder = entry.path();
        if !folder.is_dir() {
            continue;
        }
        if !folder.join("SKILL.md").is_file() {
            continue;
        }
        let folder_name = match folder.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        out.push(read_skill(folder, folder_name, scope));
    }
}

/// Build a [`Skill`] from one skill folder. The frontmatter `name` /
/// `description` win when present; a folder without parseable
/// frontmatter still lists (folder name, empty description) rather than
/// silently vanishing.
fn read_skill(folder: PathBuf, folder_name: String, scope: SkillScope) -> Skill {
    let front = read_frontmatter(&folder.join("SKILL.md"));
    let (name, description) = match front {
        Some(front) => (
            front
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(folder_name),
            front.description.unwrap_or_default(),
        ),
        None => (folder_name, String::new()),
    };
    Skill {
        name: name.trim().to_string(),
        description: one_line(&description),
        scope,
        bundles_scripts: folder.join("scripts").is_dir(),
        also_at: Vec::new(),
        folder,
    }
}

/// Collapse every run of whitespace — newlines included — to one space.
///
/// A `description` is third-party YAML, and a literal block (`|`) keeps
/// its newlines, so this is the system boundary where they have to go:
/// downstream the description is a one-line label, and the renderers
/// that draw it break on spaces and tabs only. An embedded `\n` reaches
/// a terminal cell, where it fuses the words around it and can corrupt
/// the frame. A folded block (`>`) already arrives space-joined; this
/// makes every form agree with it.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Read and parse a `SKILL.md`'s leading frontmatter, stopping at the
/// closing `---` fence so a large skill body is never loaded just to
/// read two header fields. Returns `None` when the file is unreadable,
/// doesn't open with a `---` fence, the fence is unterminated, or the
/// block isn't valid YAML.
fn read_frontmatter(manifest: &Path) -> Option<SkillFrontmatter> {
    let file = std::fs::File::open(manifest).ok()?;
    let mut lines = std::io::BufReader::new(file).lines();
    if lines.next()?.ok()?.trim_end() != "---" {
        return None;
    }
    let mut yaml = String::new();
    for line in lines {
        let line = line.ok()?;
        if line.trim_end() == "---" {
            return serde_yaml::from_str(&yaml).ok();
        }
        yaml.push_str(&line);
        yaml.push('\n');
    }
    None
}

// ── Scaffolding / authoring (#799) ──────────────────────────────────

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
fn skill_dir(repo_root: &Path, name: &str) -> PathBuf {
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
fn render_skill_md(name: &str, description: &str, body: &str) -> Result<String, SkillError> {
    let mut front = serde_yaml::Mapping::new();
    front.insert("name".into(), name.into());
    front.insert("description".into(), description.into());
    let yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(front))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", body.trim_end()))
}

/// Scaffold a `.claude/skills/<name>/SKILL.md` folder under `repo_root`
/// and return the `SKILL.md` path written. The `name` is trimmed, then
/// validated; description and body must be non-empty. Refuses to
/// overwrite an existing skill (a scaffold must never clobber
/// hand-authored bundled scripts sitting beside a `SKILL.md`). The
/// write is atomic (sibling tmp + rename) so a crash mid-write can't
/// leave a truncated `SKILL.md` behind.
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
    let name = name.trim();
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
mod discovery_tests {
    use super::*;

    fn write_skill(root: &Path, folder: &str, manifest: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), manifest).unwrap();
    }

    /// The ordered-roots argument for a repo-only / repo+user scan, so the
    /// existing cases read the way they did before roots became a list.
    fn roots(dirs: &[(&Path, SkillScope)]) -> Vec<(PathBuf, SkillScope)> {
        dirs.iter()
            .map(|(dir, scope)| (dir.to_path_buf(), *scope))
            .collect()
    }

    fn repo_only(dir: &Path) -> Vec<(PathBuf, SkillScope)> {
        roots(&[(dir, SkillScope::Repo)])
    }

    fn tmp_root(tag: &str) -> PathBuf {
        // A unique-enough dir without pulling in a temp-dir crate; the
        // pid + tag keeps parallel test cases from colliding.
        let dir =
            std::env::temp_dir().join(format!("lazybox-skills-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_name_and_description_from_frontmatter() {
        let repo = tmp_root("front");
        write_skill(
            &repo,
            "code-review",
            "---\nname: code-review\ndescription: Review a diff for bugs.\n---\nBody prose here.\n",
        );
        let skills = discover_skills_in(&repo_only(&repo));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "code-review");
        assert_eq!(skills[0].description, "Review a diff for bugs.");
        assert_eq!(skills[0].scope, SkillScope::Repo);
    }

    #[test]
    fn frontmatter_read_stops_at_the_first_closing_fence() {
        // The body is long and itself contains a `---` line (a markdown
        // horizontal rule / a second YAML doc). Parsing must read only the
        // header block — bounded at the first closing fence — and ignore
        // the rest, so `description` is the header's, not the body's.
        let repo = tmp_root("bounded");
        let body = format!(
            "---\nname: writer\ndescription: header desc\n---\n{}\n---\ndescription: body desc\n",
            "x".repeat(200_000),
        );
        write_skill(&repo, "writer", &body);
        let skills = discover_skills_in(&repo_only(&repo));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "writer");
        assert_eq!(skills[0].description, "header desc");
    }

    #[test]
    fn falls_back_to_folder_name_without_frontmatter() {
        let repo = tmp_root("nofront");
        write_skill(&repo, "deploy", "no frontmatter here, just prose\n");
        let skills = discover_skills_in(&repo_only(&repo));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "");
    }

    #[test]
    fn skips_folders_without_a_manifest() {
        let repo = tmp_root("nomanifest");
        std::fs::create_dir_all(repo.join("not-a-skill")).unwrap();
        write_skill(&repo, "real", "---\nname: real\ndescription: d\n---\n");
        let skills = discover_skills_in(&repo_only(&repo));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "real");
    }

    #[test]
    fn repo_skill_shadows_user_skill_of_same_name() {
        let repo = tmp_root("shadow-repo");
        let user = tmp_root("shadow-user");
        write_skill(
            &repo,
            "review",
            "---\nname: review\ndescription: repo version\n---\n",
        );
        write_skill(
            &user,
            "review",
            "---\nname: review\ndescription: user version\n---\n",
        );
        write_skill(
            &user,
            "notes",
            "---\nname: notes\ndescription: user only\n---\n",
        );
        let skills = discover_skills_in(&roots(&[
            (&repo, SkillScope::Repo),
            (&user, SkillScope::User),
        ]));
        // review (repo, wins), notes (user) — sorted by name.
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "notes");
        assert_eq!(skills[0].scope, SkillScope::User);
        assert_eq!(skills[1].name, "review");
        assert_eq!(skills[1].scope, SkillScope::Repo);
        assert_eq!(skills[1].description, "repo version");
    }

    #[test]
    fn results_are_sorted_by_name() {
        let repo = tmp_root("sorted");
        for folder in ["zebra", "alpha", "mango"] {
            write_skill(
                &repo,
                folder,
                &format!("---\nname: {folder}\ndescription: d\n---\n"),
            );
        }
        let names: Vec<_> = discover_skills_in(&repo_only(&repo))
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(names, vec!["alpha", "mango", "zebra"]);
    }

    /// #1671: the open standard's root. A repo that ships its skills
    /// under `.agents/skills` is discovered at repo scope, through the
    /// same root list the public entry point builds.
    #[test]
    fn agents_dir_skill_is_discovered_at_repo_scope() {
        let repo = tmp_root("agents-root");
        write_skill(
            &repo.join(".agents").join("skills"),
            "agents-only",
            "---\nname: agents-only\ndescription: From .agents.\n---\n",
        );
        let home = tmp_root("agents-root-home");
        let skills = discover_skills_in(&skill_roots(Some(&repo), Some("claude"), Some(&home)));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "agents-only");
        assert_eq!(skills[0].description, "From .agents.");
        assert_eq!(skills[0].scope, SkillScope::Repo);
    }

    /// The roots are per-agent: `.agents/` is shared, but `.claude/` and
    /// `.codex/` belong to one agent each. Offering a Claude session a
    /// `~/.codex/skills` skill means offering one it cannot load.
    #[test]
    fn roots_are_filtered_to_the_ones_that_agent_reads() {
        let repo = Path::new("/repo");
        let home = Path::new("/home/u");
        let dirs = |agent: Option<&str>| -> Vec<PathBuf> {
            skill_roots(Some(repo), agent, Some(home))
                .into_iter()
                .map(|(dir, _)| dir)
                .collect()
        };

        assert_eq!(
            dirs(Some("claude")),
            vec![
                PathBuf::from("/repo/.claude/skills"),
                PathBuf::from("/repo/.agents/skills"),
                PathBuf::from("/home/u/.claude/skills"),
                PathBuf::from("/home/u/.agents/skills"),
            ],
        );
        assert_eq!(
            dirs(Some("codex")),
            vec![
                PathBuf::from("/repo/.agents/skills"),
                PathBuf::from("/home/u/.agents/skills"),
                PathBuf::from("/home/u/.codex/skills"),
            ],
        );
        // Cursor reads the standard's shared root and nothing private.
        assert_eq!(
            dirs(Some("cursor")),
            vec![
                PathBuf::from("/repo/.agents/skills"),
                PathBuf::from("/home/u/.agents/skills"),
            ],
        );
        // An agent lazybox can't name reads only the shared root — not
        // evidence that it reads Claude's or Codex's private directory.
        assert_eq!(dirs(None), dirs(Some("cursor")));
    }

    /// A missing repo root or home drops that tier rather than joining
    /// onto nothing.
    #[test]
    fn absent_repo_root_or_home_drops_that_tier() {
        let home = Path::new("/home/u");
        assert_eq!(
            skill_roots(None, Some("claude"), Some(home))
                .into_iter()
                .map(|(dir, _)| dir)
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/home/u/.claude/skills"),
                PathBuf::from("/home/u/.agents/skills"),
            ],
        );
        assert_eq!(
            skill_roots(Some(Path::new("/repo")), Some("claude"), None)
                .into_iter()
                .map(|(dir, _)| dir)
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/repo/.claude/skills"),
                PathBuf::from("/repo/.agents/skills"),
            ],
        );
    }

    /// lazybox picks one root, but the agent resolves the name itself and
    /// may order roots differently. A shadowed twin therefore stays
    /// reviewable in `also_at`, and its `scripts/` surface ORs into the
    /// winner — the tag must never under-report the risk of a name.
    #[test]
    fn a_shadowed_twin_lends_its_scripts_surface_to_the_winner() {
        let repo = tmp_root("shadow-risk");
        let claude = repo.join(".claude").join("skills");
        let agents = repo.join(".agents").join("skills");
        write_skill(
            &claude,
            "deploy",
            "---\nname: deploy\ndescription: inert copy\n---\n",
        );
        write_skill(
            &agents,
            "deploy",
            "---\nname: deploy\ndescription: scripted copy\n---\n",
        );
        std::fs::create_dir_all(agents.join("deploy").join("scripts")).unwrap();

        let skills = discover_skills_in(&roots(&[
            (&claude, SkillScope::Repo),
            (&agents, SkillScope::Repo),
        ]));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "inert copy", "first root wins");
        assert_eq!(skills[0].folder, claude.join("deploy"));
        assert!(
            skills[0].bundles_scripts,
            "the shadowed twin bundles scripts, so the name does",
        );
        assert_eq!(skills[0].also_at, vec![agents.join("deploy")]);
    }

    /// A `description: |` literal block keeps its newlines. They must be
    /// collapsed here, at the third-party boundary: downstream renderers
    /// break on spaces and tabs only, so an embedded newline reaches a
    /// terminal cell and fuses the words around it.
    #[test]
    fn a_multi_line_description_collapses_to_one_line() {
        let repo = tmp_root("multiline-desc");
        write_skill(
            &repo,
            "review",
            "---\nname: review\ndescription: |\n  Line one.\n  Line two.\n---\n",
        );
        let skills = discover_skills_in(&repo_only(&repo));
        assert_eq!(skills[0].description, "Line one. Line two.");
    }

    /// The same name in both repo roots is one row: the first root
    /// scanned (`.claude/skills`) wins it.
    #[test]
    fn first_root_wins_within_a_tier() {
        let repo = tmp_root("tier-shadow");
        let claude = repo.join(".claude").join("skills");
        let agents = repo.join(".agents").join("skills");
        write_skill(
            &claude,
            "review",
            "---\nname: review\ndescription: claude root\n---\n",
        );
        write_skill(
            &agents,
            "review",
            "---\nname: review\ndescription: agents root\n---\n",
        );
        let skills = discover_skills_in(&roots(&[
            (&claude, SkillScope::Repo),
            (&agents, SkillScope::Repo),
        ]));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "claude root");
    }

    /// A bundled `scripts/` directory is the picker's execution-surface
    /// signal (#1671) — a directory-exists test, nothing more.
    #[test]
    fn bundled_scripts_are_flagged() {
        let repo = tmp_root("scripts");
        write_skill(&repo, "runs", "---\nname: runs\ndescription: d\n---\n");
        write_skill(&repo, "prose", "---\nname: prose\ndescription: d\n---\n");
        std::fs::create_dir_all(repo.join("runs").join("scripts")).unwrap();
        let skills = discover_skills_in(&repo_only(&repo));
        let by_name = |name: &str| {
            skills
                .iter()
                .find(|skill| skill.name == name)
                .unwrap()
                .clone()
        };
        assert!(by_name("runs").bundles_scripts);
        assert!(!by_name("prose").bundles_scripts);
        assert_eq!(by_name("runs").folder, repo.join("runs"));
    }

    #[test]
    fn missing_directories_yield_nothing() {
        let skills = discover_skills_in(&roots(&[
            (
                Path::new("/nonexistent/repo/.claude/skills"),
                SkillScope::Repo,
            ),
            (
                Path::new("/nonexistent/user/.claude/skills"),
                SkillScope::User,
            ),
        ]));
        assert!(skills.is_empty());
    }
}

#[cfg(test)]
mod scaffold_tests {
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

    /// A name with surrounding whitespace is trimmed rather than
    /// rejected, and lands at the trimmed path — the frontmatter and
    /// folder both use the clean id.
    #[test]
    fn scaffold_trims_the_name() {
        let root = tmp_root("x");
        let path = scaffold_skill(&root, "  code-review \n", "Review the diff", "Go.").unwrap();
        assert_eq!(path, skill_md_path(&root, "code-review"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("name: code-review"));
        assert!(!written.contains("name: '  code-review"));
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
