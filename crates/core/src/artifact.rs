//! The agent artifact channel: a markdown document an agent hands lazybox
//! by writing a file, rendered in a surface lazybox owns (#1822).
//!
//! The terminal stays a terminal. `docs/agent-artifact-channel.md` (#1818)
//! settles why: the agent pane is a repainting full-screen TUI, so there is
//! no stable document in that byte stream to lift out, and the cell grid the
//! scrollback/replay machinery depends on cannot hold one. Instead the agent
//! writes a plain `.md` file into [`ARTIFACT_SPOOL_RELATIVE_PATH`] inside its
//! worktree; the daemon notices it, attaches it to the workspace, and the TUI
//! opens it in the markdown reader that already renders issue bodies.
//!
//! Nothing here requires an agent capability beyond writing a file, which is
//! why it works for `GenericCli` exactly as it does for Claude.
//!
//! ## The file's shape
//!
//! A bare markdown file, no front-matter and no sidecar. The reader needs a
//! title and a body ([`crate::Artifact::title`] / [`crate::Artifact::body`]),
//! and a markdown document already carries one: its first heading. So the
//! title is the leading `# …` line when the file opens with one — dropped
//! from the body, since the reader paints it in the frame — and otherwise the
//! file stem. An agent that knows nothing about this format still produces a
//! correctly-titled artifact by writing ordinary markdown.
//!
//! ## Which spool, and which artifact (#1855)
//!
//! A workspace with two sessions has two worktrees and so two spools. A file
//! name is unique within one spool and not within the workspace, so
//! [`Artifact::name`] alone cannot name an artifact and two `plan.md` files
//! render as two identical headings. Every artifact therefore carries the
//! [`Artifact::worktree`] it came from, `(worktree, name)` is the identity a
//! picker selects by, and [`qualified_titles`] adds a qualifier to exactly the
//! labels that would otherwise collide.
//!
//! The bounds below cap what one broadcast carries, which used to mean the
//! artifacts past them were named in a count and reachable by nothing. They
//! are now carried as [`ArtifactRef`]s — name and title, no body — so the
//! picker lists every artifact in the spool and the body is read on demand.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Spool directory inside a worktree, relative to its root. Excluded from
/// git alongside `.lazybox/task.json` — see
/// `lazybox_server::task_cache::exclude_lazybox_paths`.
pub const ARTIFACT_SPOOL_RELATIVE_PATH: &str = ".lazybox/artifacts";

/// The one extension the spool picks up. Markdown only in this slice —
/// mermaid, images and web views are later slices of #1818, and an
/// unrecognised file is left alone rather than guessed at.
pub const ARTIFACT_EXTENSION: &str = "md";

/// Largest artifact body the daemon carries, per file.
///
/// Past this the artifact is still attached, with its body replaced by a
/// notice naming the file and its size: the spool is a channel an agent
/// writes to unprompted, and a silently-dropped file reads to the user as
/// "lazybox didn't notice", which is the one thing a notification channel
/// must never do. 256 KiB is far past any document a person reads in a
/// modal and well under what a broadcast frame carries comfortably.
pub const ARTIFACT_MAX_BYTES: u64 = 256 * 1024;

/// Most artifacts attached to one workspace. Newest first; the remainder
/// stay on disk and are counted as hidden rather than forgotten, so the
/// reader can say so instead of quietly showing a partial set.
pub const ARTIFACT_MAX_PER_WORKSPACE: usize = 24;

/// Largest total body the attached set carries for one workspace.
///
/// [`ARTIFACT_MAX_BYTES`] and [`ARTIFACT_MAX_PER_WORKSPACE`] bound one file
/// and one count, but their *product* is what a single broadcast event
/// carries — 6 MiB, a number nobody chose, cloned per subscriber and held in
/// the bus ring. This is the bound that was actually decided: past it the
/// newest artifacts are kept and the rest counted as hidden, exactly as the
/// count cap does.
pub const ARTIFACT_MAX_TOTAL_BYTES: usize = 1024 * 1024;

/// Longest reader title carried, in characters.
///
/// A title is a markdown heading line, which nothing bounds: a file whose
/// first line is `# ` followed by 200 KiB of prose has a 200 KiB title. That
/// was harmless while the title only ever rode alongside its own body, and is
/// not once [`ArtifactRef`] carries titles *without* bodies — the index's size
/// would be set by the worst heading in the spool. Truncated with an ellipsis
/// rather than rejected: a long heading is still the best name the file has.
pub const ARTIFACT_MAX_TITLE_CHARS: usize = 120;

/// Most artifacts one workspace's index names — the bodies it carries
/// ([`ARTIFACT_MAX_PER_WORKSPACE`]) plus the [`ArtifactRef`]s past them.
///
/// The refs are what make a capped-out artifact reachable, so this is the
/// bound on reachability itself and is deliberately far above the body caps:
/// a ref is a few hundred bytes, so naming every artifact in a spool costs a
/// fraction of one carried document. Past it the remainder is still *counted*
/// (`unlisted`), because a channel an agent writes to unprompted must never
/// let a file vanish silently.
pub const ARTIFACT_MAX_INDEXED: usize = 256;

/// One markdown document an agent spooled for its workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct Artifact {
    /// Spool file name (`plan.md`) — unique within one spool, so rewriting
    /// the same file replaces rather than appends. Not unique within a
    /// *workspace*: see [`Self::worktree`].
    pub name: String,
    /// Directory name of the worktree whose spool this came from.
    ///
    /// A workspace with two sessions has two worktrees, so `(worktree, name)`
    /// rather than `name` is what identifies an artifact within a workspace —
    /// the key a picker selects by, and what tells two `plan.md` files apart
    /// in the reader (#1855).
    pub worktree: String,
    /// Reader title: the leading `# …` heading, else the humanised stem.
    pub title: String,
    /// Markdown body, with the title heading removed when it supplied the
    /// title (the reader paints that in the modal frame).
    pub body: String,
    /// The spool file's modification time, as the daemon last read it.
    pub written_at: DateTime<Utc>,
}

impl Artifact {
    /// Parse one spool file into the title/body pair the reader needs.
    pub fn from_markdown(
        worktree: impl Into<String>,
        name: impl Into<String>,
        contents: &str,
        written_at: DateTime<Utc>,
    ) -> Self {
        let (worktree, name) = (worktree.into(), name.into());
        match leading_heading(contents) {
            Some((title, rest)) => Self {
                worktree,
                name,
                title,
                body: rest,
                written_at,
            },
            None => Self {
                title: humanise_stem(&name),
                worktree,
                name,
                body: contents.trim_start_matches('\n').to_string(),
                written_at,
            },
        }
    }

    /// The `(worktree, name)` pair that identifies this artifact within its
    /// workspace, as [`ArtifactRef`] carries it.
    pub fn as_ref(&self) -> ArtifactRef {
        ArtifactRef {
            worktree: self.worktree.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            written_at: self.written_at,
        }
    }

    /// The stand-in for an artifact the daemon could not read at all — not
    /// UTF-8, permissions, a vanished file.
    ///
    /// Same rule as [`Self::oversized`]: the spool is a channel an agent
    /// writes to unprompted, so a file that is present but unreadable has to
    /// say so. Logging it and dropping it reads to the user as lazybox never
    /// having noticed, which is the one outcome this channel must not have.
    pub fn unreadable(
        worktree: impl Into<String>,
        name: impl Into<String>,
        reason: &str,
        written_at: DateTime<Utc>,
    ) -> Self {
        let name = name.into();
        let body = format!(
            "lazybox could not read this artifact: {reason}\n\nIt is in the worktree at \
`{ARTIFACT_SPOOL_RELATIVE_PATH}/{name}`. A spooled artifact must be UTF-8 markdown.",
        );
        Self {
            title: humanise_stem(&name),
            worktree: worktree.into(),
            name,
            body,
            written_at,
        }
    }

    /// The stand-in for an artifact too large to carry
    /// ([`ARTIFACT_MAX_BYTES`]). Attached in the file's place so the user
    /// learns the artifact exists and where to read it, rather than being
    /// shown nothing.
    pub fn oversized(
        worktree: impl Into<String>,
        name: impl Into<String>,
        bytes: u64,
        written_at: DateTime<Utc>,
    ) -> Self {
        let name = name.into();
        let body = format!(
            "This artifact is {bytes} bytes, past lazybox's {ARTIFACT_MAX_BYTES}-byte limit, so \
its contents are not shown here.\n\nRead it in the worktree at \
`{ARTIFACT_SPOOL_RELATIVE_PATH}/{name}`.",
        );
        Self {
            title: humanise_stem(&name),
            worktree: worktree.into(),
            name,
            body,
            written_at,
        }
    }
}

/// One artifact named without its body — what the picker lists (#1855).
///
/// The body caps bound what a broadcast carries; a ref is what makes the
/// artifacts past them reachable anyway. The reader gets the body by asking
/// the daemon for this `(worktree, name)`, which is also the pick's payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ArtifactRef {
    /// Directory name of the worktree whose spool holds the file.
    pub worktree: String,
    /// Spool file name (`plan.md`).
    pub name: String,
    /// Reader title, as [`Artifact::title`] derives it.
    pub title: String,
    /// The spool file's modification time, as the daemon last read it.
    pub written_at: DateTime<Utc>,
}

impl From<&Artifact> for ArtifactRef {
    fn from(artifact: &Artifact) -> Self {
        artifact.as_ref()
    }
}

/// What one workspace's spools currently hold, as the daemon last read them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceArtifacts {
    /// Carried with their bodies, newest first, within the body caps.
    pub artifacts: Vec<Artifact>,
    /// Every artifact past those caps, newest first, named but not carried —
    /// the picker's remaining rows, fetched on demand.
    pub hidden: Vec<ArtifactRef>,
    /// How many artifacts [`ARTIFACT_MAX_INDEXED`] left out of `hidden`
    /// entirely. Counted rather than forgotten, the same rule the body caps
    /// follow.
    pub unlisted: usize,
}

impl WorkspaceArtifacts {
    /// Every artifact the picker can offer, newest first: the carried ones
    /// named, then the rest.
    pub fn index(&self) -> Vec<ArtifactRef> {
        self.artifacts
            .iter()
            .map(ArtifactRef::from)
            .chain(self.hidden.iter().cloned())
            .collect()
    }

    /// How many artifacts the workspace has, including the ones no body and
    /// no ref was carried for — what the row badge counts.
    pub fn total(&self) -> usize {
        self.artifacts.len() + self.hidden.len() + self.unlisted
    }

    /// How many the combined reader document is not showing.
    pub fn not_shown(&self) -> usize {
        self.hidden.len() + self.unlisted
    }
}

/// Reader labels for `entries`, qualified only where they would collide.
///
/// A title is a markdown heading, and nothing stops two artifacts from
/// sharing one — two worktrees of a workspace each holding `plan.md`, or one
/// agent writing `plan.md` and `plan-v2.md` both opening `# The plan`. A
/// picker listing two identical rows, or a document with two identical
/// headings, cannot be acted on (#1855).
///
/// So each label escalates only as far as it must: the bare title, else the
/// title and the file that differs, else the title and the full
/// `worktree/name` identity. A workspace with one spool and distinct titles —
/// the ordinary case — is left exactly as it reads today.
pub fn qualified_titles(entries: &[ArtifactRef]) -> Vec<String> {
    use std::collections::HashMap;
    let mut by_title: HashMap<&str, usize> = HashMap::new();
    let mut by_file: HashMap<(&str, &str), usize> = HashMap::new();
    for entry in entries {
        *by_title.entry(entry.title.as_str()).or_default() += 1;
        *by_file
            .entry((entry.title.as_str(), entry.name.as_str()))
            .or_default() += 1;
    }
    entries
        .iter()
        .map(|entry| {
            let title = entry.title.as_str();
            if by_title.get(title) == Some(&1) {
                entry.title.clone()
            } else if by_file.get(&(title, entry.name.as_str())) == Some(&1) {
                format!("{title} · {}", entry.name)
            } else {
                format!("{title} · {}/{}", entry.worktree, entry.name)
            }
        })
        .collect()
}

/// The `(title, body)` pair the markdown reader opens for a workspace's
/// artifacts, or `None` when it has none.
///
/// One artifact opens under its own title. Several open as one document,
/// newest first, each under its own `#` heading — the reader already
/// scrolls, and a set of artifacts from one session is read together far
/// more often than one is picked out of it. Headings come from
/// [`qualified_titles`], so two artifacts that share a title are told apart
/// rather than repeated.
///
/// `not_shown` is how many artifacts the body caps left out of this document
/// ([`WorkspaceArtifacts::not_shown`]). They are named in a closing line
/// which points at the picker `picker_keys` opens — each of them is listed
/// there and can be read, so the line is a route rather than the dead end a
/// bare count was (#1855).
pub fn artifact_document(
    workspace_name: &str,
    artifacts: &[Artifact],
    not_shown: usize,
    picker_keys: &str,
) -> Option<(String, String)> {
    let (first, rest) = artifacts.split_first()?;
    let footer = (not_shown > 0).then(|| {
        format!(
            "\n\n---\n\n*{not_shown} older artifact(s) not shown here. `{picker_keys}` lists \
every artifact in this workspace and opens the one you pick; they are also in the worktree at \
`{ARTIFACT_SPOOL_RELATIVE_PATH}/`.*"
        )
    });
    let headings = qualified_titles(&artifacts.iter().map(ArtifactRef::from).collect::<Vec<_>>());
    if rest.is_empty() && footer.is_none() {
        return Some((headings[0].clone(), first.body.clone()));
    }
    let mut body = String::new();
    for (artifact, heading) in artifacts.iter().zip(&headings) {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str("# ");
        body.push_str(heading);
        body.push_str("\n\n");
        body.push_str(&close_unterminated_fence(artifact.body.trim()));
    }
    if let Some(footer) = footer {
        body.push_str(&footer);
    }
    let title = if rest.is_empty() {
        headings[0].clone()
    } else {
        format!("{} artifacts · {workspace_name}", artifacts.len())
    };
    Some((title, body))
}

/// Terminate a code fence the artifact left open.
///
/// Per CommonMark an unclosed fence runs to the end of the *document*, and
/// the combined reader document is several artifacts concatenated — so one
/// artifact ending mid-fence (a file caught mid-write, an agent that forgot
/// the closing line) would swallow every artifact after it and the hidden-
/// count footer with them. One artifact's mistake must not hide the others.
fn close_unterminated_fence(body: &str) -> String {
    let mut open: Option<(char, usize)> = None;
    for line in body.lines() {
        let trimmed = line.trim_start_matches(' ');
        // More than three leading spaces is indented code, not a fence.
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        let Some(marker) = trimmed.chars().next().filter(|c| *c == '`' || *c == '~') else {
            continue;
        };
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        if run < 3 {
            continue;
        }
        match open {
            // A closing fence is a bare run of at least the opener's length,
            // of the same character, with nothing but whitespace after it.
            Some((open_marker, open_run))
                if marker == open_marker && run >= open_run && trimmed[run..].trim().is_empty() =>
            {
                open = None;
            }
            Some(_) => {}
            None => open = Some((marker, run)),
        }
    }
    match open {
        Some((marker, run)) => {
            let mut closed = String::with_capacity(body.len() + run + 1);
            closed.push_str(body);
            closed.push('\n');
            closed.extend(std::iter::repeat_n(marker, run));
            closed
        }
        None => body.to_string(),
    }
}

/// Split a leading level-1 ATX heading off the front of a markdown file.
///
/// Only a heading that *opens* the document is a title: one further down is
/// a section of the body, and lifting it would retitle the artifact with its
/// second thought. A heading with no text (`#`) supplies no title either.
fn leading_heading(contents: &str) -> Option<(String, String)> {
    let mut lines = contents.lines();
    let first = lines.find(|line| !line.trim().is_empty())?;
    let title = first.strip_prefix("# ")?.trim();
    if title.is_empty() {
        return None;
    }
    let rest = contents
        .split_once(first)
        .map(|(_, after)| after.trim_start_matches('\n').to_string())
        .unwrap_or_default();
    Some((clamp_title(title), rest))
}

/// Hold a title to [`ARTIFACT_MAX_TITLE_CHARS`], counting characters rather
/// than bytes so the cut never lands inside one.
fn clamp_title(title: &str) -> String {
    match title.char_indices().nth(ARTIFACT_MAX_TITLE_CHARS) {
        Some((cut, _)) => format!("{}…", title[..cut].trim_end()),
        None => title.to_string(),
    }
}

/// `design-notes.md` → `design notes`. The fallback title when the file
/// carries no heading of its own.
fn humanise_stem(name: &str) -> String {
    let stem = name
        .strip_suffix(&format!(".{ARTIFACT_EXTENSION}"))
        .unwrap_or(name);
    let humanised = stem.replace(['-', '_'], " ");
    let humanised = humanised.trim();
    if humanised.is_empty() {
        clamp_title(name)
    } else {
        clamp_title(humanised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .expect("valid fixture timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn leading_heading_becomes_the_title_and_leaves_the_body() {
        let a = Artifact::from_markdown("wt", "plan.md", "# The plan\n\nStep one.\n", at());
        assert_eq!(a.title, "The plan");
        assert_eq!(a.body, "Step one.\n");
    }

    #[test]
    fn blank_lines_before_the_heading_do_not_hide_it() {
        let a = Artifact::from_markdown("wt", "plan.md", "\n\n# Title\n\nbody\n", at());
        assert_eq!(a.title, "Title");
        assert_eq!(a.body, "body\n");
    }

    #[test]
    fn a_heading_further_down_is_body_not_title() {
        // Retitling an artifact with its second section would be worse than
        // no title at all — the stem is at least honest about the file.
        let a =
            Artifact::from_markdown("wt", "review-notes.md", "Intro line.\n\n# Section\n", at());
        assert_eq!(a.title, "review notes");
        assert!(a.body.contains("# Section"));
        assert!(a.body.starts_with("Intro line."));
    }

    #[test]
    fn a_deeper_heading_is_not_a_title() {
        let a = Artifact::from_markdown("wt", "x.md", "## Sub\n\nbody\n", at());
        assert_eq!(a.title, "x");
        assert!(a.body.starts_with("## Sub"));
    }

    #[test]
    fn an_empty_heading_falls_back_to_the_stem() {
        let a = Artifact::from_markdown("wt", "notes.md", "#\n\nbody\n", at());
        assert_eq!(a.title, "notes");
    }

    #[test]
    fn oversized_names_the_file_and_the_limit() {
        let a = Artifact::oversized("wt", "huge.md", ARTIFACT_MAX_BYTES + 1, at());
        assert_eq!(a.title, "huge");
        assert!(a.body.contains(".lazybox/artifacts/huge.md"));
        assert!(a.body.contains(&ARTIFACT_MAX_BYTES.to_string()));
    }

    #[test]
    fn one_artifact_opens_under_its_own_title() {
        let a = Artifact::from_markdown("wt", "plan.md", "# The plan\n\nStep one.\n", at());
        let (title, body) =
            artifact_document("ws", std::slice::from_ref(&a), 0, "a P").expect("document");
        assert_eq!(title, "The plan");
        assert_eq!(body, "Step one.\n");
    }

    #[test]
    fn several_artifacts_open_as_one_titled_document() {
        let a = Artifact::from_markdown("wt", "plan.md", "# Plan\n\nStep one.\n", at());
        let b = Artifact::from_markdown("wt", "findings.md", "# Findings\n\nIt works.\n", at());
        let (title, body) = artifact_document("issue-7", &[a, b], 0, "a P").expect("document");
        assert_eq!(title, "2 artifacts · issue-7");
        assert!(body.starts_with("# Plan\n\nStep one."));
        assert!(body.contains("# Findings\n\nIt works."));
    }

    #[test]
    fn hidden_artifacts_are_named_rather_than_silently_dropped() {
        let a = Artifact::from_markdown("wt", "plan.md", "# Plan\n\nStep one.\n", at());
        let (_, body) = artifact_document("ws", &[a], 3, "x Y").expect("document");
        assert!(body.contains("3 older artifact(s) not shown"), "{body}");
        // The count used to be the end of the road. It now names the chord
        // that lists every one of them.
        // Quoted from the argument, not a chord core knows: the caller
        // resolves the user's own keymap.
        assert!(body.contains("`x Y` lists"), "{body}");
    }

    #[test]
    fn an_unterminated_fence_cannot_swallow_the_following_artifacts() {
        // Per CommonMark an unclosed fence runs to end-of-document, so
        // concatenating bodies let one artifact caught mid-write hide every
        // artifact after it — and the hidden-count footer with them.
        let broken =
            Artifact::from_markdown("wt", "broken.md", "# Broken\n\n```rust\nfn x() {}\n", at());
        let intact = Artifact::from_markdown("wt", "intact.md", "# Intact\n\nVisible.\n", at());
        let (_, body) = artifact_document("ws", &[broken, intact], 2, "a P").expect("document");
        let after_fence = body
            .split("# Intact")
            .nth(1)
            .expect("the second artifact survives");
        assert!(after_fence.contains("Visible."));
        assert!(
            body.contains("2 older artifact(s) not shown"),
            "the footer must not be inside the broken artifact's fence: {body}"
        );
        // The opener is closed exactly once — a tilde fence and a longer
        // backtick run must not be mistaken for each other.
        assert_eq!(body.matches("```").count(), 2, "{body}");
    }

    #[test]
    fn a_properly_closed_fence_is_left_alone() {
        let closed = Artifact::from_markdown("wt", "a.md", "# A\n\n```\ncode\n```\n", at());
        let other = Artifact::from_markdown("wt", "b.md", "# B\n\nx\n", at());
        let (_, body) = artifact_document("ws", &[closed, other], 0, "a P").expect("document");
        assert_eq!(body.matches("```").count(), 2, "no fence was added: {body}");
    }

    #[test]
    fn a_tilde_fence_is_closed_with_tildes() {
        let broken = Artifact::from_markdown("wt", "a.md", "# A\n\n~~~~\ncode\n", at());
        let other = Artifact::from_markdown("wt", "b.md", "# B\n\nx\n", at());
        let (_, body) = artifact_document("ws", &[broken, other], 0, "a P").expect("document");
        assert!(body.contains("~~~~\ncode\n~~~~"), "{body}");
    }

    #[test]
    fn unreadable_names_the_file_and_the_reason() {
        let a = Artifact::unreadable("wt", "junk.md", "stream did not contain valid UTF-8", at());
        assert_eq!(a.title, "junk");
        assert!(a.body.contains(".lazybox/artifacts/junk.md"));
        assert!(a.body.contains("valid UTF-8"));
    }

    #[test]
    fn no_artifacts_opens_nothing() {
        assert!(artifact_document("ws", &[], 0, "a P").is_none());
    }

    #[test]
    fn two_worktrees_holding_one_filename_are_told_apart() {
        // The #1855 case: a workspace with two sessions has two spools, and
        // `plan.md` in both used to render as two identical `# The plan`
        // headings with nothing saying which session wrote which.
        let mine = Artifact::from_markdown("issue-7", "plan.md", "# The plan\n\nMine.\n", at());
        let theirs = Artifact::from_markdown("main", "plan.md", "# The plan\n\nTheirs.\n", at());
        let (title, body) = artifact_document("ws", &[mine, theirs], 0, "a P").expect("document");
        assert_eq!(title, "2 artifacts · ws");
        assert!(body.contains("# The plan · issue-7/plan.md"), "{body}");
        assert!(body.contains("# The plan · main/plan.md"), "{body}");
        assert!(
            !body.contains("# The plan\n"),
            "no heading may be the bare colliding title: {body}"
        );
    }

    #[test]
    fn one_spool_with_two_files_under_one_title_is_told_apart_by_file() {
        // Same collision, one worktree: the worktree cannot disambiguate, so
        // the label escalates to the file name instead.
        let a = Artifact::from_markdown("wt", "plan.md", "# The plan\n\nA.\n", at());
        let b = Artifact::from_markdown("wt", "plan-v2.md", "# The plan\n\nB.\n", at());
        let (_, body) = artifact_document("ws", &[a, b], 0, "a P").expect("document");
        assert!(body.contains("# The plan · plan.md"), "{body}");
        assert!(body.contains("# The plan · plan-v2.md"), "{body}");
        assert!(
            !body.contains("wt/plan.md"),
            "no worktree needed here: {body}"
        );
    }

    #[test]
    fn distinct_titles_are_never_qualified() {
        // The ordinary case must read exactly as it did before #1855.
        let a = Artifact::from_markdown("wt", "plan.md", "# Plan\n\nA.\n", at());
        let b = Artifact::from_markdown("other", "findings.md", "# Findings\n\nB.\n", at());
        let (_, body) = artifact_document("ws", &[a, b], 0, "a P").expect("document");
        assert!(body.contains("# Plan\n\nA."), "{body}");
        assert!(body.contains("# Findings\n\nB."), "{body}");
        assert!(!body.contains(" · "), "{body}");
    }

    #[test]
    fn one_artifact_title_is_qualified_only_against_the_rest() {
        let only = Artifact::from_markdown("wt", "plan.md", "# The plan\n\nx.\n", at());
        let (title, _) = artifact_document("ws", &[only], 0, "a P").expect("document");
        assert_eq!(title, "The plan");
    }

    #[test]
    fn a_title_longer_than_the_cap_is_clamped() {
        // A heading is unbounded markdown, and `ArtifactRef` carries titles
        // with no body to dominate them — so the index's size would be set by
        // the worst heading in the spool.
        let long = "x".repeat(ARTIFACT_MAX_TITLE_CHARS * 3);
        let a = Artifact::from_markdown("wt", "plan.md", &format!("# {long}\n\nbody\n"), at());
        assert_eq!(a.title.chars().count(), ARTIFACT_MAX_TITLE_CHARS + 1);
        assert!(a.title.ends_with('…'));
        // The body is untouched — only the title is bounded.
        assert_eq!(a.body, "body\n");
    }

    #[test]
    fn clamping_a_title_never_splits_a_character() {
        let long = "é".repeat(ARTIFACT_MAX_TITLE_CHARS + 5);
        let a = Artifact::from_markdown("wt", "p.md", &format!("# {long}\n"), at());
        assert_eq!(a.title.chars().count(), ARTIFACT_MAX_TITLE_CHARS + 1);
    }

    #[test]
    fn a_ref_carries_the_identity_and_drops_the_body() {
        let a = Artifact::from_markdown("issue-7", "plan.md", "# Plan\n\nStep one.\n", at());
        let r = ArtifactRef::from(&a);
        assert_eq!(
            (r.worktree.as_str(), r.name.as_str()),
            ("issue-7", "plan.md")
        );
        assert_eq!(r.title, "Plan");
        assert_eq!(r.written_at, a.written_at);
    }

    #[test]
    fn the_index_names_carried_and_hidden_artifacts_alike() {
        // The picker's whole point: one list over both halves, so an artifact
        // past the body caps is reachable rather than merely counted.
        let carried = Artifact::from_markdown("wt", "new.md", "# New\n\nx\n", at());
        let found = WorkspaceArtifacts {
            artifacts: vec![carried],
            hidden: vec![ArtifactRef {
                worktree: "wt".to_string(),
                name: "old.md".to_string(),
                title: "Old".to_string(),
                written_at: at(),
            }],
            unlisted: 2,
        };
        let index = found.index();
        let names: Vec<&str> = index.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["new.md", "old.md"]);
        assert_eq!(found.total(), 4, "the badge counts what it cannot name too");
        assert_eq!(found.not_shown(), 3, "the reader's footer counts both");
    }
}
