//! Agent **Skills** — discovery and scaffolding.
//!
//! A snippet is human-triggered; a skill is model-triggered — the agent
//! reads each skill's `description` and decides *itself* whether to
//! invoke it, progressively loading the `SKILL.md` body and any bundled
//! files. This module owns the filesystem-facing halves of lazybox's
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
//! - **Export** (#1672): write one of lazybox's curated snippets out as
//!   a `SKILL.md`, so a vetted workflow is portable to any agent that
//!   reads the format — and stays checkable against its source snippet.
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
    /// The lazybox snippet this skill's frontmatter *claims* it was
    /// exported from (#1672), read from `metadata.lazybox.snippet`.
    /// `None` for every skill carrying no such marker — which is most of
    /// them.
    ///
    /// A claim, not a verdict: like every other field here it is
    /// third-party YAML, and nothing stops a hand-authored skill from
    /// writing the marker. Discovery has no snippet catalog to check it
    /// against, so the picker reports it as marked and leaves the
    /// verifying to `lazybox snippet export --check`. In particular it
    /// must not retract the "not vetted by lazybox" disclosure (#1671).
    pub from_snippet: Option<String>,
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
///
/// `metadata` stays an untyped [`serde_yaml::Value`] on purpose: a
/// third-party skill may put anything under that key, and a typed field
/// would fail the *whole* frontmatter parse on a shape we don't expect —
/// dropping that skill's name and description from discovery. Digging the
/// one nested path we care about ([`lazybox_export_meta`]) keeps an odd
/// `metadata:` harmless.
#[derive(serde::Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    metadata: serde_yaml::Value,
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
    let (name, description, from_snippet) = match front {
        Some(front) => {
            let from_snippet = lazybox_export_meta(&front).map(|meta| meta.snippet);
            (
                front
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(folder_name),
                front.description.unwrap_or_default(),
                from_snippet,
            )
        }
        None => (folder_name, String::new(), None),
    };
    Skill {
        name: name.trim().to_string(),
        description: one_line(&description),
        scope,
        bundles_scripts: folder.join("scripts").is_dir(),
        also_at: Vec::new(),
        folder,
        from_snippet,
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
    #[error("{0} was edited in place — re-export with --force to discard that edit")]
    ExportEdited(PathBuf),
    #[error("{0} is not a lazybox snippet export — re-export with --force to replace it")]
    ExportForeign(PathBuf),
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

// ── Export: a snippet becomes a portable skill (#1672) ───────────────
//
// The inverse of the `skill:` bridge (#798, where a snippet *dispatches*
// a skill): here a curated lazybox snippet is written out AS a `SKILL.md`
// so the same vetted workflow reaches any agent that reads the format,
// with or without lazybox. Nothing ever flows the other way — a skill is
// never read back into a snippet, so the snippet body stays the single
// authored copy of a workflow (the #1145 rule).

/// Which skill root an export is written into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTarget {
    /// `<repo_root>/<dir>/skills` — travels with the repo.
    Repo,
    /// `$HOME/<dir>/skills` — follows the user across repos.
    User,
}

/// Which directory convention the target agent reads. `.claude` is Claude
/// Code's; `.agents` is the open Agent Skills spec's, which Codex and
/// others read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillAgentDir {
    Claude,
    Agents,
}

impl SkillAgentDir {
    fn segment(self) -> &'static str {
        match self {
            SkillAgentDir::Claude => ".claude",
            SkillAgentDir::Agents => ".agents",
        }
    }

    /// Parse the `--agent-dir` value. `None` for anything else, so the
    /// caller reports the typo instead of silently picking a default.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "claude" => Some(SkillAgentDir::Claude),
            "agents" => Some(SkillAgentDir::Agents),
            _ => None,
        }
    }
}

impl SkillTarget {
    /// Parse the `--to` value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "repo" => Some(SkillTarget::Repo),
            "user" => Some(SkillTarget::User),
            _ => None,
        }
    }
}

/// The skills root an export lands in — `<repo_root>/.claude/skills`,
/// `$HOME/.agents/skills`, and so on. `None` when the target's base is
/// unavailable: `Repo` without a repo root, or `User` without a `$HOME`.
pub fn skills_root(
    target: SkillTarget,
    agent_dir: SkillAgentDir,
    repo_root: Option<&Path>,
) -> Option<PathBuf> {
    let base = match target {
        SkillTarget::Repo => repo_root?.to_path_buf(),
        SkillTarget::User => {
            PathBuf::from(std::env::var_os("HOME").filter(|home| !home.is_empty())?)
        }
    };
    Some(base.join(agent_dir.segment()).join("skills"))
}

/// The `SKILL.md` an exported snippet is written to.
pub fn exported_skill_path(root: &Path, key: &str) -> PathBuf {
    root.join(key).join("SKILL.md")
}

/// How an exported `SKILL.md` relates to the snippet it came from.
///
/// Both drift directions are distinguished because they call for
/// opposite responses: a *stale* export is regenerated freely (the file
/// is generated, nothing is lost), while an *edited* one holds a change
/// that only `--force` may discard — and whose right home is the snippet
/// body, since a skill never writes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportState {
    /// Nothing exported under this key yet.
    Missing,
    /// The exported body still matches the snippet's.
    InSync,
    /// The snippet changed after the export — regenerate.
    Stale,
    /// The `SKILL.md` body was edited in place, away from the version it
    /// recorded.
    Edited,
    /// A `SKILL.md` is there but carries no lazybox export marker — a
    /// hand-authored or third-party skill that happens to share the name.
    Foreign,
}

impl ExportState {
    /// Whether this state is drift worth reporting — what `--check` and
    /// the startup notice flag.
    pub fn is_drift(self) -> bool {
        matches!(self, ExportState::Stale | ExportState::Edited)
    }

    /// One-word label for CLI / notice output.
    pub fn label(self) -> &'static str {
        match self {
            ExportState::Missing => "not exported",
            ExportState::InSync => "up to date",
            ExportState::Stale => "stale",
            ExportState::Edited => "edited",
            ExportState::Foreign => "not a lazybox export",
        }
    }
}

/// One snippet's export status under a given skills root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportStatus {
    pub key: String,
    pub path: PathBuf,
    pub state: ExportState,
}

/// What an export call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportOutcome {
    /// The `SKILL.md` was written (created or regenerated).
    Written(PathBuf),
    /// Already byte-current — nothing written.
    Unchanged(PathBuf),
}

impl ExportOutcome {
    pub fn path(&self) -> &Path {
        match self {
            ExportOutcome::Written(p) | ExportOutcome::Unchanged(p) => p,
        }
    }
}

/// The lazybox provenance an exported skill records under
/// `metadata.lazybox`.
struct ExportMeta {
    /// The source snippet key.
    snippet: String,
    /// Hash of the body as exported — the drift anchor.
    version: String,
}

/// Dig `metadata.lazybox.{snippet,version}` out of a parsed frontmatter.
/// `None` for any skill that isn't a lazybox export.
fn lazybox_export_meta(front: &SkillFrontmatter) -> Option<ExportMeta> {
    let lazybox = front.metadata.get("lazybox")?;
    Some(ExportMeta {
        snippet: lazybox.get("snippet")?.as_str()?.to_string(),
        version: lazybox.get("version")?.as_str()?.to_string(),
    })
}

/// Split a `SKILL.md` into its frontmatter YAML and the body that
/// follows the closing fence. `None` when there is no complete
/// frontmatter block.
fn split_skill_md(contents: &str) -> Option<(String, String)> {
    // Both line endings, because the sibling `read_frontmatter` accepts
    // both (its `trim_end` absorbs the `\r`): an LF-only opening fence
    // here made a CRLF `SKILL.md` — a checkout under `eol=crlf`, or any
    // editor configured that way — parse as no frontmatter at all, so it
    // classified as `Foreign` and `drifted_exports` filtered it out of
    // the sweep entirely. An export lazybox could no longer read is the
    // last thing `--check` should report as clean.
    let rest = contents
        .strip_prefix("---\n")
        .or_else(|| contents.strip_prefix("---\r\n"))?;
    let mut consumed = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let body = &rest[consumed + line.len()..];
            return Some((
                rest[..consumed].to_string(),
                body.trim_start_matches('\n').to_string(),
            ));
        }
        consumed += line.len();
    }
    None
}

/// Human-readable provider label for the scope note an export carries.
fn provider_label(provider: &str) -> String {
    match provider {
        "github" => "GitHub".to_string(),
        "linear" => "Linear".to_string(),
        "slack" => "Slack".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The exported skill's `description`. A `provider:`-scoped snippet
/// states the scope in prose: a skill has no provider filter, so the
/// only place that constraint can survive the export is the text the
/// model reads when deciding whether the skill applies.
fn export_description(snippet: &crate::Snippet) -> String {
    let description = snippet.description.trim();
    match snippet.provider.as_deref() {
        None => description.to_string(),
        Some(provider) => format!(
            "{description} ({} workspaces only)",
            provider_label(provider)
        ),
    }
}

/// Render the `SKILL.md` a snippet exports to: frontmatter carrying the
/// skill `name` / `description` plus lazybox provenance, then the
/// snippet's delivery body verbatim.
///
/// No `scripts/` and no bundled files — a snippet is text, so the
/// exported skill has no execution surface.
pub fn render_snippet_skill(key: &str, snippet: &crate::Snippet) -> Result<String, SkillError> {
    validate_skill_name(key)?;
    // Checked before the scope suffix is appended: a scoped snippet with
    // no description of its own would otherwise export as the bare
    // "(GitHub workspaces only)", which tells the model nothing.
    if snippet.description.trim().is_empty() {
        return Err(SkillError::MissingDescription);
    }
    let description = export_description(snippet);
    let body = snippet.dispatch_body();
    if body.trim().is_empty() {
        return Err(SkillError::MissingBody);
    }

    let mut lazybox = serde_yaml::Mapping::new();
    lazybox.insert("snippet".into(), key.into());
    let category = snippet.category.trim();
    if !category.is_empty() {
        lazybox.insert("category".into(), category.into());
    }
    lazybox.insert("version".into(), crate::export_body_hash(&body).into());
    let mut metadata = serde_yaml::Mapping::new();
    metadata.insert("lazybox".into(), serde_yaml::Value::Mapping(lazybox));

    let mut front = serde_yaml::Mapping::new();
    front.insert("name".into(), key.into());
    front.insert("description".into(), description.into());
    front.insert("metadata".into(), serde_yaml::Value::Mapping(metadata));
    let yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(front))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", body.trim_end()))
}

/// Classify the export of `key` under `root` against the live snippet.
pub fn export_status(root: &Path, key: &str, snippet: &crate::Snippet) -> ExportStatus {
    let path = exported_skill_path(root, key);
    let state = match std::fs::read_to_string(&path) {
        Err(_) => ExportState::Missing,
        Ok(contents) => classify_export(&contents, key, snippet),
    };
    ExportStatus {
        key: key.to_string(),
        path,
        state,
    }
}

/// The pure core of [`export_status`], against an already-read file.
///
/// The recorded `version` is the anchor for *both* comparisons: the
/// file's own body against it says whether the file was edited, and the
/// snippet's current body against it says whether the snippet moved. An
/// edit wins the tie — it is the one that would be destroyed by a
/// regenerate, so it is the one the user must be told about.
fn classify_export(contents: &str, key: &str, snippet: &crate::Snippet) -> ExportState {
    let Some((yaml, body)) = split_skill_md(contents) else {
        return ExportState::Foreign;
    };
    let Ok(front) = serde_yaml::from_str::<SkillFrontmatter>(&yaml) else {
        return ExportState::Foreign;
    };
    let Some(meta) = lazybox_export_meta(&front) else {
        return ExportState::Foreign;
    };
    if meta.snippet != key {
        return ExportState::Foreign;
    }
    if crate::export_body_hash(&body) != meta.version {
        return ExportState::Edited;
    }
    if snippet.dispatch_hash() != meta.version {
        return ExportState::Stale;
    }
    ExportState::InSync
}

/// Export `snippet` as `<root>/<key>/SKILL.md` and report what happened.
///
/// An existing export is regenerated freely when it is this snippet's own
/// and unedited (`InSync` — a no-op — or `Stale`). An `Edited` or
/// `Foreign` file is refused unless `force`, so a regenerate can never
/// silently destroy a hand-written change or someone else's skill.
pub fn export_snippet_skill(
    root: &Path,
    key: &str,
    snippet: &crate::Snippet,
    force: bool,
) -> Result<ExportOutcome, SkillError> {
    let contents = render_snippet_skill(key, snippet)?;
    let status = export_status(root, key, snippet);
    if !force {
        match status.state {
            ExportState::Edited => return Err(SkillError::ExportEdited(status.path)),
            ExportState::Foreign => return Err(SkillError::ExportForeign(status.path)),
            ExportState::InSync => return Ok(ExportOutcome::Unchanged(status.path)),
            ExportState::Missing | ExportState::Stale => {}
        }
    }
    std::fs::create_dir_all(root.join(key))?;
    write_atomically(&status.path, contents.as_bytes())?;
    Ok(ExportOutcome::Written(status.path))
}

/// Every snippet whose export under `root` has drifted — the `--check`
/// report and the startup notice. Ordered by key (the catalog's order);
/// a missing root yields nothing, so the common "never exported
/// anything" case costs one failed `read_dir`.
pub fn drifted_exports(root: &Path, snippets: &crate::Snippets) -> Vec<ExportStatus> {
    if !root.is_dir() {
        return Vec::new();
    }
    snippets
        .all()
        .map(|(key, snippet)| export_status(root, key, snippet))
        .filter(|status| status.state.is_drift())
        .collect()
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

    pub(super) fn repo_only(dir: &Path) -> Vec<(PathBuf, SkillScope)> {
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

#[cfg(test)]
mod export_tests {
    use super::*;
    use crate::{Snippet, SnippetOrigin, Snippets};

    fn tmp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lazybox-export-test-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn snippet(body: &str) -> Snippet {
        Snippet {
            description: "Review the current diff".into(),
            category: "Review".into(),
            body: body.into(),
            skill: None,
            provider: None,
            next: Vec::new(),
            origin: SnippetOrigin::BuiltIn,
        }
    }

    /// The acceptance case: exporting `rev` yields a `SKILL.md` whose
    /// frontmatter validates and whose body is the snippet's delivery
    /// body byte for byte.
    #[test]
    fn exports_a_valid_skill_md_with_the_delivery_body() {
        let root = tmp_root("valid");
        let rev = Snippets::builtin()
            .get("rev")
            .cloned()
            .expect("built-in rev");
        let outcome = export_snippet_skill(&root, "rev", &rev, false).unwrap();
        assert_eq!(
            outcome,
            ExportOutcome::Written(exported_skill_path(&root, "rev"))
        );

        let written = std::fs::read_to_string(outcome.path()).unwrap();
        let (yaml, body) = split_skill_md(&written).expect("frontmatter block");
        let front: SkillFrontmatter = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(front.name.as_deref(), Some("rev"));
        assert_eq!(front.description.as_deref(), Some(rev.description.as_str()));
        assert_eq!(body.trim_end(), rev.dispatch_body().trim_end());

        let meta = lazybox_export_meta(&front).expect("lazybox metadata");
        assert_eq!(meta.snippet, "rev");
        assert_eq!(meta.version, rev.dispatch_hash());
        assert_eq!(
            front
                .metadata
                .get("lazybox")
                .and_then(|m| m.get("category"))
                .and_then(|c| c.as_str()),
            Some("Review"),
        );
    }

    /// Round-trip: what `export_snippet_skill` writes is what
    /// `discover_skills_in` finds, name and description intact.
    #[test]
    fn an_exported_skill_is_discovered_by_the_picker_scan() {
        let repo = tmp_root("roundtrip");
        let root = repo.join(".claude").join("skills");
        export_snippet_skill(&root, "rev", &snippet("Review it."), false).unwrap();
        let skills = discover_skills_in(&super::discovery_tests::repo_only(&root));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "rev");
        assert_eq!(skills[0].description, "Review the current diff");
        assert_eq!(skills[0].from_snippet.as_deref(), Some("rev"));
    }

    /// A skill lazybox did not write carries no snippet provenance — the
    /// picker must not imply lazybox vouches for a third-party skill.
    #[test]
    fn a_hand_authored_skill_carries_no_snippet_provenance() {
        let repo = tmp_root("provenance");
        std::fs::create_dir_all(repo.join("deploy")).unwrap();
        std::fs::write(
            repo.join("deploy").join("SKILL.md"),
            "---\nname: deploy\ndescription: ship it\nmetadata: not-a-mapping\n---\nGo.\n",
        )
        .unwrap();
        let skills = discover_skills_in(&super::discovery_tests::repo_only(&repo));
        assert_eq!(skills.len(), 1);
        // An odd `metadata:` must not cost the skill its name/description.
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "ship it");
        assert_eq!(skills[0].from_snippet, None);
    }

    /// A snippet body that moves after the export reads as stale, and a
    /// plain re-export (no `--force`) is what fixes it.
    #[test]
    fn a_changed_snippet_makes_the_export_stale() {
        let root = tmp_root("stale");
        let before = snippet("Review it.");
        export_snippet_skill(&root, "rev", &before, false).unwrap();
        assert_eq!(
            export_status(&root, "rev", &before).state,
            ExportState::InSync
        );

        let after = snippet("Review it, adversarially.");
        assert_eq!(
            export_status(&root, "rev", &after).state,
            ExportState::Stale
        );
        export_snippet_skill(&root, "rev", &after, false).unwrap();
        assert_eq!(
            export_status(&root, "rev", &after).state,
            ExportState::InSync
        );
    }

    /// Editing the `SKILL.md` reads as edited — a different drift from
    /// stale, and one a re-export refuses to discard without `--force`.
    #[test]
    fn an_edited_skill_md_is_reported_and_not_silently_overwritten() {
        let root = tmp_root("edited");
        let rev = snippet("Review it.");
        let path = exported_skill_path(&root, "rev");
        export_snippet_skill(&root, "rev", &rev, false).unwrap();

        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("Review it.", "Review it, but softly.");
        std::fs::write(&path, &edited).unwrap();
        assert_eq!(export_status(&root, "rev", &rev).state, ExportState::Edited);

        let err = export_snippet_skill(&root, "rev", &rev, false).unwrap_err();
        assert!(matches!(err, SkillError::ExportEdited(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);

        export_snippet_skill(&root, "rev", &rev, true).unwrap();
        assert_eq!(export_status(&root, "rev", &rev).state, ExportState::InSync);
    }

    /// The drift anchor must be byte-exact, not whitespace-normalized.
    /// Reflowing a body into paragraphs is the commonest prompt edit
    /// there is, and it changes prose a model reads — under the #1312
    /// whitespace-insensitive `body_hash` the export reported "up to
    /// date" forever, the exact silent divergence export exists to catch.
    #[test]
    fn reflowing_a_snippet_body_makes_the_export_stale() {
        let root = tmp_root("reflow");
        let flat = snippet("Pass 1: design. Pass 2: stress. Pass 3: blast radius.");
        export_snippet_skill(&root, "rev", &flat, false).unwrap();
        assert_eq!(
            export_status(&root, "rev", &flat).state,
            ExportState::InSync,
        );

        // Same words, new shape: whitespace-only, and still a real change
        // to the exported prompt.
        let reflowed = snippet("Pass 1: design.\n\nPass 2: stress.\n\nPass 3: blast radius.");
        assert_eq!(
            export_status(&root, "rev", &reflowed).state,
            ExportState::Stale,
            "a whitespace-only body change still changes the exported prompt",
        );
        export_snippet_skill(&root, "rev", &reflowed, false).unwrap();
        assert_eq!(
            export_status(&root, "rev", &reflowed).state,
            ExportState::InSync,
        );
    }

    /// The trailing newline the file format appends is the one whitespace
    /// difference that is NOT drift — otherwise every export would report
    /// itself edited the moment it was written.
    #[test]
    fn the_formats_trailing_newline_is_not_drift() {
        let root = tmp_root("trailing");
        let s = snippet("Review it.");
        export_snippet_skill(&root, "rev", &s, false).unwrap();
        let path = exported_skill_path(&root, "rev");

        // An editor that strips the final newline, and one that adds
        // several, both leave the body itself untouched.
        let written = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, written.trim_end()).unwrap();
        assert_eq!(export_status(&root, "rev", &s).state, ExportState::InSync);
        std::fs::write(&path, format!("{}\n\n\n", written.trim_end())).unwrap();
        assert_eq!(export_status(&root, "rev", &s).state, ExportState::InSync);
    }

    /// A `SKILL.md` saved with CRLF endings still parses as the lazybox
    /// export it is. It reads as `Edited` (the bytes really did change,
    /// so a regenerate would discard someone's save) rather than
    /// `Foreign`, which `drifted_exports` filtered out of the sweep —
    /// leaving `--check` reporting "no drifted exports" for a file
    /// lazybox could no longer read.
    #[test]
    fn a_crlf_export_is_still_recognized_and_reported() {
        let root = tmp_root("crlf");
        let s = snippet("Review it.");
        export_snippet_skill(&root, "rev", &s, false).unwrap();
        let path = exported_skill_path(&root, "rev");
        let crlf = std::fs::read_to_string(&path)
            .unwrap()
            .replace('\n', "\r\n");
        std::fs::write(&path, crlf).unwrap();

        assert_eq!(export_status(&root, "rev", &s).state, ExportState::Edited);
        let yaml = root.join("snippets.yaml");
        std::fs::write(
            &yaml,
            "snippets:\n  rev:\n    description: d\n    body: Review it.\n",
        )
        .unwrap();
        let catalog = Snippets::load_from(&yaml, SnippetOrigin::Global).unwrap();
        let drifted = drifted_exports(&root, &catalog);
        assert_eq!(drifted.len(), 1, "the sweep must not go blind: {drifted:?}");
        assert_eq!(drifted[0].state, ExportState::Edited);
    }

    /// A hand-authored skill that happens to share a snippet's key is
    /// never a lazybox export, so it is never regenerated over.
    #[test]
    fn a_foreign_skill_is_refused_without_force() {
        let root = tmp_root("foreign");
        let path = exported_skill_path(&root, "rev");
        std::fs::create_dir_all(root.join("rev")).unwrap();
        std::fs::write(
            &path,
            "---\nname: rev\ndescription: mine\n---\nHand written.\n",
        )
        .unwrap();

        let rev = snippet("Review it.");
        assert_eq!(
            export_status(&root, "rev", &rev).state,
            ExportState::Foreign
        );
        let err = export_snippet_skill(&root, "rev", &rev, false).unwrap_err();
        assert!(matches!(err, SkillError::ExportForeign(_)), "{err}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Hand written.")
        );
    }

    /// A `SKILL.md` whose lazybox marker names a *different* snippet is
    /// foreign too — two snippets can't share one exported folder.
    #[test]
    fn an_export_of_another_snippet_is_foreign() {
        let root = tmp_root("mismatch");
        let rev = snippet("Review it.");
        let contents = render_snippet_skill("rev", &rev).unwrap();
        std::fs::create_dir_all(root.join("nit")).unwrap();
        std::fs::write(exported_skill_path(&root, "nit"), contents).unwrap();
        assert_eq!(
            export_status(&root, "nit", &rev).state,
            ExportState::Foreign
        );
    }

    /// Re-exporting an unchanged snippet writes nothing.
    #[test]
    fn an_up_to_date_export_is_a_no_op() {
        let root = tmp_root("noop");
        let rev = snippet("Review it.");
        export_snippet_skill(&root, "rev", &rev, false).unwrap();
        let outcome = export_snippet_skill(&root, "rev", &rev, false).unwrap();
        assert_eq!(
            outcome,
            ExportOutcome::Unchanged(exported_skill_path(&root, "rev"))
        );
    }

    /// A skill has no provider filter, so a `provider:`-scoped snippet
    /// has to state the scope where the model will read it.
    #[test]
    fn a_provider_scoped_snippet_states_its_scope_in_the_description() {
        let root = tmp_root("scoped");
        let mut scoped = snippet("Reply to the review threads.");
        scoped.description = "Address the PR review comments".into();
        scoped.provider = Some("github".into());
        export_snippet_skill(&root, "respond", &scoped, false).unwrap();
        let written = std::fs::read_to_string(exported_skill_path(&root, "respond")).unwrap();
        assert!(
            written.contains("Address the PR review comments (GitHub workspaces only)"),
            "{written}",
        );
    }

    /// A skill-dispatching snippet (#798) exports the invocation it
    /// actually delivers, not the raw body.
    #[test]
    fn a_skill_dispatching_snippet_exports_its_dispatch_body() {
        let root = tmp_root("dispatch");
        let mut bridge = snippet("Rank findings by severity.");
        bridge.skill = Some("code-review".into());
        export_snippet_skill(&root, "rev", &bridge, false).unwrap();
        let written = std::fs::read_to_string(exported_skill_path(&root, "rev")).unwrap();
        assert!(written.contains("Use the `code-review` skill"), "{written}");
        assert_eq!(
            export_status(&root, "rev", &bridge).state,
            ExportState::InSync
        );
    }

    /// An exported skill is *model-selectable mid-task*
    /// (`docs/snippets-vs-skills.md`) — unlike a snippet, which is always
    /// the whole turn. So the export carries the authored body and never
    /// the output contract, whose "nothing after it" would truncate the
    /// host turn a model invoked this skill from.
    #[test]
    fn an_exported_skill_carries_no_output_contract() {
        let root = tmp_root("no-contract");
        let catalog = crate::Snippets::builtin();
        for key in ["rev", "commit", "triage"] {
            let snippet = catalog.get(key).expect(key);
            export_snippet_skill(&root, key, snippet, false).unwrap();
            let written = std::fs::read_to_string(exported_skill_path(&root, key)).unwrap();
            assert!(!written.contains("OUTPUT CONTRACT"), "{key}: {written}");
            assert!(!written.contains("nothing after it"), "{key}: {written}");
            // The authored instructions still make it across intact.
            assert!(written.contains("The verdict names"), "{key}: {written}");
        }
    }

    /// A description-less snippet can't be model-selected, so the export
    /// refuses rather than writing a skill the agent can never pick.
    #[test]
    fn export_requires_a_description_and_a_body() {
        let root = tmp_root("empty");
        let mut blank = snippet("Review it.");
        blank.description = "  ".into();
        assert!(matches!(
            export_snippet_skill(&root, "rev", &blank, false),
            Err(SkillError::MissingDescription),
        ));
        // Still refused with a provider scope, whose suffix would
        // otherwise stand in for the missing description.
        blank.provider = Some("github".into());
        assert!(matches!(
            export_snippet_skill(&root, "rev", &blank, false),
            Err(SkillError::MissingDescription),
        ));
        let mut bodyless = snippet("   ");
        bodyless.skill = None;
        assert!(matches!(
            export_snippet_skill(&root, "rev", &bodyless, false),
            Err(SkillError::MissingBody),
        ));
        assert!(!root.join("rev").exists());
    }

    /// A key that isn't a portable folder name is rejected before any
    /// filesystem join — the same guard scaffolding uses.
    #[test]
    fn export_rejects_a_key_that_is_not_a_valid_skill_name() {
        let root = tmp_root("badkey");
        assert!(matches!(
            export_snippet_skill(&root, "../escape", &snippet("b"), false),
            Err(SkillError::InvalidName(_)),
        ));
    }

    /// The `--check` / startup-notice report: only drifted exports, in
    /// key order, and nothing at all when nothing was ever exported.
    #[test]
    fn drift_report_lists_only_drifted_exports() {
        let root = tmp_root("drift");
        assert!(drifted_exports(&root, &Snippets::builtin()).is_empty());

        let stale_before = snippet("Review it.");
        export_snippet_skill(&root, "rev", &stale_before, false).unwrap();
        export_snippet_skill(&root, "nit", &snippet("Nitpick it."), false).unwrap();

        let yaml = root.join("snippets.yaml");
        std::fs::write(
            &yaml,
            "snippets:\n  rev:\n    description: d\n    body: Review it, harder.\n\
             \x20 nit:\n    description: d\n    body: Nitpick it.\n",
        )
        .unwrap();
        let catalog = Snippets::load_from(&yaml, SnippetOrigin::Global).unwrap();

        let drifted = drifted_exports(&root, &catalog);
        assert_eq!(drifted.len(), 1, "{drifted:?}");
        assert_eq!(drifted[0].key, "rev");
        assert_eq!(drifted[0].state, ExportState::Stale);
    }

    #[test]
    fn skills_root_follows_the_target_and_agent_dir() {
        let repo = Path::new("/repo");
        assert_eq!(
            skills_root(SkillTarget::Repo, SkillAgentDir::Claude, Some(repo)),
            Some(PathBuf::from("/repo/.claude/skills")),
        );
        assert_eq!(
            skills_root(SkillTarget::Repo, SkillAgentDir::Agents, Some(repo)),
            Some(PathBuf::from("/repo/.agents/skills")),
        );
        // A repo target without a repo root has no home.
        assert_eq!(
            skills_root(SkillTarget::Repo, SkillAgentDir::Claude, None),
            None
        );
    }

    #[test]
    fn target_and_agent_dir_parse_their_cli_values() {
        assert_eq!(SkillTarget::parse("repo"), Some(SkillTarget::Repo));
        assert_eq!(SkillTarget::parse("user"), Some(SkillTarget::User));
        assert_eq!(SkillTarget::parse("global"), None);
        assert_eq!(SkillAgentDir::parse("claude"), Some(SkillAgentDir::Claude));
        assert_eq!(SkillAgentDir::parse("agents"), Some(SkillAgentDir::Agents));
        assert_eq!(SkillAgentDir::parse("codex"), None);
    }

    /// The body split must stop at the *first* closing fence: a rendered
    /// review body is full of `---`-looking prose and markdown rules.
    #[test]
    fn body_split_stops_at_the_first_closing_fence() {
        let (yaml, body) = split_skill_md("---\nname: x\n---\n\nbody\n---\nmore\n").unwrap();
        assert_eq!(yaml, "name: x\n");
        assert_eq!(body, "body\n---\nmore\n");
        assert!(split_skill_md("no frontmatter\n").is_none());
        assert!(split_skill_md("---\nname: x\nunterminated\n").is_none());
    }
}
