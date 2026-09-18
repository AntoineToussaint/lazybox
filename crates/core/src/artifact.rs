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

/// One markdown document an agent spooled for its workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct Artifact {
    /// Spool file name (`plan.md`) — the artifact's identity within a
    /// workspace, so rewriting the same file replaces rather than appends.
    pub name: String,
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
        name: impl Into<String>,
        contents: &str,
        written_at: DateTime<Utc>,
    ) -> Self {
        let name = name.into();
        match leading_heading(contents) {
            Some((title, rest)) => Self {
                name,
                title,
                body: rest,
                written_at,
            },
            None => Self {
                title: humanise_stem(&name),
                name,
                body: contents.trim_start_matches('\n').to_string(),
                written_at,
            },
        }
    }

    /// The stand-in for an artifact too large to carry
    /// ([`ARTIFACT_MAX_BYTES`]). Attached in the file's place so the user
    /// learns the artifact exists and where to read it, rather than being
    /// shown nothing.
    pub fn oversized(name: impl Into<String>, bytes: u64, written_at: DateTime<Utc>) -> Self {
        let name = name.into();
        let body = format!(
            "This artifact is {bytes} bytes, past lazybox's {ARTIFACT_MAX_BYTES}-byte limit, so \
its contents are not shown here.\n\nRead it in the worktree at \
`{ARTIFACT_SPOOL_RELATIVE_PATH}/{name}`.",
        );
        Self {
            title: humanise_stem(&name),
            name,
            body,
            written_at,
        }
    }
}

/// The `(title, body)` pair the markdown reader opens for a workspace's
/// artifacts, or `None` when it has none.
///
/// One artifact opens under its own title. Several open as one document,
/// newest first, each under its own `#` heading — the reader already
/// scrolls, and a set of artifacts from one session is read together far
/// more often than one is picked out of it. `hidden` is the count past
/// [`ARTIFACT_MAX_PER_WORKSPACE`], named in a closing line so a truncated
/// set never reads as the whole set.
pub fn artifact_document(
    workspace_name: &str,
    artifacts: &[Artifact],
    hidden: usize,
) -> Option<(String, String)> {
    let (first, rest) = artifacts.split_first()?;
    let footer = (hidden > 0).then(|| {
        format!(
            "\n\n---\n\n*{hidden} older artifact(s) not shown — the newest \
{ARTIFACT_MAX_PER_WORKSPACE} are listed. All of them are in the worktree at \
`{ARTIFACT_SPOOL_RELATIVE_PATH}/`.*"
        )
    });
    if rest.is_empty() && footer.is_none() {
        return Some((first.title.clone(), first.body.clone()));
    }
    let mut body = String::new();
    for artifact in artifacts {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str("# ");
        body.push_str(&artifact.title);
        body.push_str("\n\n");
        body.push_str(artifact.body.trim());
    }
    if let Some(footer) = footer {
        body.push_str(&footer);
    }
    let title = if rest.is_empty() {
        first.title.clone()
    } else {
        format!("{} artifacts · {workspace_name}", artifacts.len())
    };
    Some((title, body))
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
    Some((title.to_string(), rest))
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
        name.to_string()
    } else {
        humanised.to_string()
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
        let a = Artifact::from_markdown("plan.md", "# The plan\n\nStep one.\n", at());
        assert_eq!(a.title, "The plan");
        assert_eq!(a.body, "Step one.\n");
    }

    #[test]
    fn blank_lines_before_the_heading_do_not_hide_it() {
        let a = Artifact::from_markdown("plan.md", "\n\n# Title\n\nbody\n", at());
        assert_eq!(a.title, "Title");
        assert_eq!(a.body, "body\n");
    }

    #[test]
    fn a_heading_further_down_is_body_not_title() {
        // Retitling an artifact with its second section would be worse than
        // no title at all — the stem is at least honest about the file.
        let a = Artifact::from_markdown("review-notes.md", "Intro line.\n\n# Section\n", at());
        assert_eq!(a.title, "review notes");
        assert!(a.body.contains("# Section"));
        assert!(a.body.starts_with("Intro line."));
    }

    #[test]
    fn a_deeper_heading_is_not_a_title() {
        let a = Artifact::from_markdown("x.md", "## Sub\n\nbody\n", at());
        assert_eq!(a.title, "x");
        assert!(a.body.starts_with("## Sub"));
    }

    #[test]
    fn an_empty_heading_falls_back_to_the_stem() {
        let a = Artifact::from_markdown("notes.md", "#\n\nbody\n", at());
        assert_eq!(a.title, "notes");
    }

    #[test]
    fn oversized_names_the_file_and_the_limit() {
        let a = Artifact::oversized("huge.md", ARTIFACT_MAX_BYTES + 1, at());
        assert_eq!(a.title, "huge");
        assert!(a.body.contains(".lazybox/artifacts/huge.md"));
        assert!(a.body.contains(&ARTIFACT_MAX_BYTES.to_string()));
    }

    #[test]
    fn one_artifact_opens_under_its_own_title() {
        let a = Artifact::from_markdown("plan.md", "# The plan\n\nStep one.\n", at());
        let (title, body) = artifact_document("ws", std::slice::from_ref(&a), 0).expect("document");
        assert_eq!(title, "The plan");
        assert_eq!(body, "Step one.\n");
    }

    #[test]
    fn several_artifacts_open_as_one_titled_document() {
        let a = Artifact::from_markdown("plan.md", "# Plan\n\nStep one.\n", at());
        let b = Artifact::from_markdown("findings.md", "# Findings\n\nIt works.\n", at());
        let (title, body) = artifact_document("issue-7", &[a, b], 0).expect("document");
        assert_eq!(title, "2 artifacts · issue-7");
        assert!(body.starts_with("# Plan\n\nStep one."));
        assert!(body.contains("# Findings\n\nIt works."));
    }

    #[test]
    fn hidden_artifacts_are_named_rather_than_silently_dropped() {
        let a = Artifact::from_markdown("plan.md", "# Plan\n\nStep one.\n", at());
        let (_, body) = artifact_document("ws", &[a], 3).expect("document");
        assert!(body.contains("3 older artifact(s) not shown"), "{body}");
    }

    #[test]
    fn no_artifacts_opens_nothing() {
        assert!(artifact_document("ws", &[], 0).is_none());
    }
}
