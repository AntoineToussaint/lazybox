//! Help-assistant support: fuzzy search over the action catalog and
//! the generated context document for the "ask lazybox" agent (#302).
//!
//! Both surfaces read the *runtime* catalog — the post-override
//! effective bindings after `ui.keymap_preset` and `ui.action_keys`
//! are applied — so an answer always quotes the user's actual keys.
//! The prose reference is the in-tree docs, baked in at compile time
//! with `include_str!` so it ships inside the binary and re-embeds on
//! every build; nothing here is hand-maintained per release.

use crate::action::{ActionKind, CatalogEntry, Chord, KeyStroke};

/// Sentinel session key for the help-assistant agent run. Not a real
/// workspace: the daemon's `resolve_cwd` finds no workspace record for
/// it and the run needs no worktree. Clients use it to recognize the
/// matching `AgentRunStarted` on the shared event bus.
pub const HELP_SESSION_KEY: &str = "lazybox:help";

/// Provider fallback order for the help assistant. A compatible
/// configured default agent wins first; this order only applies when
/// that default cannot serve structured runs.
pub const HELP_AGENT_PREFERENCE: &[&str] = &["claude", "codex"];

/// Choose the first compatible configured agent for Ask Lazybox.
pub fn select_help_agent(enabled: &[String], default_agent: Option<&str>) -> Option<&'static str> {
    if let Some(default_agent) = default_agent
        && let Some(agent) = HELP_AGENT_PREFERENCE
            .iter()
            .copied()
            .find(|candidate| *candidate == default_agent)
        && enabled.iter().any(|enabled| enabled == agent)
    {
        return Some(agent);
    }
    HELP_AGENT_PREFERENCE
        .iter()
        .copied()
        .find(|candidate| enabled.iter().any(|agent| agent == candidate))
}

/// The fenced-code info string the help agent uses to emit a proposed
/// action (#353). Kept strict — only a `lazybox-action` fence is
/// parsed, so example JSON the agent shows in a plain ```json block
/// never misfires the confirm.
pub const ACTION_FENCE: &str = "lazybox-action";

/// A structured action the help agent proposes and lazybox applies
/// natively after a confirm-with-preview (#353). The set is a strict
/// allowlist: an intent whose `action` isn't a known variant fails to
/// deserialize and is ignored, so the agent can never drive an
/// un-vetted mutation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HelpActionIntent {
    /// Add (or replace) a snippet in the user's global
    /// `~/.lazybox/snippets.yaml`. `key` triggers it as `]]s<key>`.
    AddSnippet {
        key: String,
        #[serde(default)]
        category: String,
        #[serde(default)]
        description: String,
        body: String,
    },
    /// Set one allowlisted key in `~/.lazybox/config.yaml` (see
    /// [`EDITABLE_CONFIG_KEYS`]). `key` is a dotted path; `value` is
    /// the scalar to store. The client validates both against the
    /// allowlist before doing anything.
    EditConfig { key: String, value: String },
    /// Scaffold a `.claude/skills/<name>/SKILL.md` folder in the
    /// current repo (#799). Chosen over `add_snippet` when the request
    /// is genuinely multi-step or wants bundled scripts/reference
    /// files — a model-triggered skill, not a single human-fired
    /// prompt. `name` is the skill/folder id (kebab-case); `description`
    /// is what the agent matches on to decide when to use it; `body` is
    /// the SKILL.md instruction markdown (no frontmatter — lazybox
    /// writes it from `name` + `description`).
    ScaffoldSkill {
        name: String,
        description: String,
        body: String,
    },
}

/// The config keys the `edit_config` action may set, each with a
/// one-line description of its accepted values. This is the *only*
/// surface `edit_config` can touch — the client rejects any key not
/// listed here, and the help agent is told to stay within it.
///
/// Kept here (not client-side) so the generated prompt and the client
/// allowlist read from one list; the client still validates each
/// value against live state (a theme must exist, an agent must be
/// enabled) before applying.
pub const EDITABLE_CONFIG_KEYS: &[(&str, &str)] = &[
    (
        "ui.theme",
        "the color theme, by exact name (see the Themes doc for the list); applies live",
    ),
    (
        "setup.default_agent",
        "the agent id spawned by `w` / new workspace (must be one of the enabled agents); applies live",
    ),
    (
        "ui.keymap_preset",
        "the starter keymap: `default` or `vim`; takes effect after a restart",
    ),
];

/// Parse the first `lazybox-action` fenced block out of the help
/// agent's answer into an allowlisted [`HelpActionIntent`]. Returns
/// `None` when there's no such block, the block isn't valid JSON, or
/// its `action` isn't a known variant. The block body is JSON.
pub fn parse_action_intent(answer: &str) -> Option<HelpActionIntent> {
    let json = extract_action_block(answer)?;
    serde_json::from_str::<HelpActionIntent>(&json).ok()
}

/// Return `answer` with every `lazybox-action` fenced block removed, so
/// the transcript shows the agent's prose without any raw intent JSON.
/// Collapses the blank lines the removed blocks leave behind. When
/// there's no such block, returns the input trimmed of trailing
/// whitespace unchanged. Only the first block is ever *executed*, but a
/// stray second one must not leak raw JSON into the transcript either.
pub fn strip_action_block(answer: &str) -> String {
    let Some((start, end)) = action_block_span(answer) else {
        return answer.trim_end().to_string();
    };
    let head = answer[..start].trim_end();
    // Recurse on the remainder so a second block is stripped too.
    let tail = strip_action_block(answer[end..].trim_start_matches(['\n', '\r']));
    match (head.is_empty(), tail.trim().is_empty()) {
        (_, true) => head.to_string(),
        (true, false) => tail,
        (false, false) => format!("{head}\n\n{tail}"),
    }
}

/// Byte span `[start, end)` of a `lazybox-action` fenced block within
/// `answer`, fences included. `start` is the opening fence's first
/// byte; `end` is one past the closing fence's newline (or the end of
/// input if it's unterminated).
fn action_block_span(answer: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    let mut lines = answer.split_inclusive('\n');
    let mut opened_at: Option<usize> = None;
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if let Some(open) = opened_at {
            if is_fence(trimmed) {
                return Some((open, offset + line.len()));
            }
        } else if let Some(info) = trimmed.strip_prefix("```")
            && info.trim() == ACTION_FENCE
        {
            opened_at = Some(offset);
        }
        offset += line.len();
    }
    // Unterminated fence: treat the rest of the input as the block.
    opened_at.map(|open| (open, answer.len()))
}

/// The JSON body between a `lazybox-action` fence's delimiters.
fn extract_action_block(answer: &str) -> Option<String> {
    let (start, end) = action_block_span(answer)?;
    let inner = &answer[start..end];
    // Drop the opening fence line and, if present, the closing one.
    let body: String = inner
        .split_inclusive('\n')
        .skip(1)
        .filter(|line| !is_fence(line.trim()))
        .collect();
    if body.trim().is_empty() {
        None
    } else {
        Some(body)
    }
}

/// A line is a closing fence if, trimmed, it's only backticks.
fn is_fence(trimmed: &str) -> bool {
    trimmed.len() >= 3 && trimmed.chars().all(|c| c == '`')
}

/// In-tree docs embedded into the help agent's context. Titles are
/// section headers in the generated document.
const DOCS: &[(&str, &str)] = &[
    ("README", include_str!("../../../README.md")),
    (
        "Features overview",
        include_str!("../../../docs/features/README.md"),
    ),
    (
        "Inbox and sync",
        include_str!("../../../docs/features/inbox-and-sync.md"),
    ),
    (
        "Workspaces and worktrees",
        include_str!("../../../docs/features/workspaces-and-worktrees.md"),
    ),
    (
        "Terminals and agents",
        include_str!("../../../docs/features/terminals-and-agents.md"),
    ),
    (
        "TUI and UX",
        include_str!("../../../docs/features/tui-and-ux.md"),
    ),
    (
        "Providers",
        include_str!("../../../docs/features/providers.md"),
    ),
    (
        "Daemon and deployment",
        include_str!("../../../docs/features/daemon-and-deployment.md"),
    ),
    ("Snippets", include_str!("../../../docs/snippets.md")),
    ("Themes", include_str!("../../../docs/themes.md")),
    ("Slack setup", include_str!("../../../docs/slack-setup.md")),
];

/// Fuzzy search over the catalog: returns indices into `catalog`,
/// best matches first. Every whitespace-separated query token must
/// appear as a substring of the row's combined haystack (label, keys,
/// description, section title, case-insensitive). Ranked: all tokens
/// in the label first, then label+keys, then anywhere. When nothing
/// passes the token filter, falls back to an in-order subsequence
/// match over the haystack so near-misses ("mutliselect") still
/// surface rather than going straight to an empty list.
pub fn search(catalog: &[CatalogEntry], query: &str) -> Vec<usize> {
    let tokens: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut ranked: Vec<(u8, usize)> = Vec::new();
    for (idx, entry) in catalog.iter().enumerate() {
        let label = entry.label.to_lowercase();
        let keys = entry.keys_display.to_lowercase();
        let describe = entry.describe.to_lowercase();
        let section = entry.section.title().to_lowercase();
        let all_in = |hay: &dyn Fn(&str) -> bool| tokens.iter().all(|t| hay(t));
        let rank = if all_in(&|t| label.contains(t)) {
            0
        } else if all_in(&|t| label.contains(t) || keys.contains(t)) {
            1
        } else if all_in(&|t| {
            label.contains(t) || keys.contains(t) || describe.contains(t) || section.contains(t)
        }) {
            2
        } else {
            continue;
        };
        ranked.push((rank, idx));
    }
    if ranked.is_empty() {
        let needle: String = tokens.concat();
        for (idx, entry) in catalog.iter().enumerate() {
            let hay = format!("{} {} {}", entry.label, entry.keys_display, entry.describe);
            if subsequence_icase(&hay, &needle) {
                ranked.push((3, idx));
            }
        }
    }
    ranked.sort_by_key(|&(rank, idx)| (rank, idx));
    ranked.into_iter().map(|(_, idx)| idx).collect()
}

/// Case-insensitive subsequence test: all chars of `needle` appear in
/// `haystack` in order, gaps allowed.
fn subsequence_icase(haystack: &str, needle: &str) -> bool {
    let mut hs = haystack.chars().map(|c| c.to_ascii_lowercase());
    'outer: for nc in needle.chars().map(|c| c.to_ascii_lowercase()) {
        for hc in hs.by_ref() {
            if hc == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// Scope note per section, rendered next to each keybinding group so
/// the assistant can answer "…but only in the activity pane" — the
/// part of a binding static docs always under-specify.
fn section_scope(section: crate::action::Section) -> &'static str {
    use crate::action::Section;
    match section {
        Section::Global => {
            "active from non-terminal panes; a focused terminal forwards keys to its program \
             (use the terminal leader first), except for explicitly noted global exceptions"
        }
        Section::Workspace => {
            "acts on the focused workspace; active while the sidebar or activity pane has focus"
        }
        Section::Sidebar => "active while the sidebar (left pane) has focus",
        Section::Activity => "active while the activity pane (right pane) has focus",
        Section::Terminal => {
            "active while an embedded terminal has focus (all other keys are forwarded to the program inside)"
        }
    }
}

/// Append the generated marker/pill reference: the sidebar status
/// pills, the per-session agent-state glyph, and the passive row badges,
/// each from `crate::markers` so a new marker is documented the moment
/// it ships — the same "derived from code" guarantee the action catalog
/// gives keybindings (#883). This is why Ask Lazybox can explain a pill
/// like `CHANGES` without anyone hand-writing it into the prose docs.
fn push_markers(out: &mut String) {
    use crate::markers::{MarkerDoc, agent_state_docs, row_badge_docs, status_pill_docs};

    fn write_group(out: &mut String, docs: &[MarkerDoc]) {
        for doc in docs {
            out.push_str(&format!(
                "- `{}` — {} {}\n",
                doc.label, doc.meaning, doc.when
            ));
        }
    }

    out.push_str(
        "\n# Sidebar markers and pills\n\n\
Every marker a workspace row can carry, and what it means. These are generated from the code, \
so they always match what's on screen.\n\n\
## Status pills\n\nThe right-side pill(s) on a PR row — one review pill and one CI pill; a \
terminal/blocker state (merged, closed, conflict, …) overrides both. Most severe wins:\n",
    );
    write_group(out, &status_pill_docs());

    out.push_str(
        "\n## Agent state\n\nThe per-session glyph on a workspace row (and the terminal tab \
badge) showing what the workspace's agent is doing:\n",
    );
    write_group(out, &agent_state_docs());

    out.push_str(
        "\n## Row badges\n\nPassive badges packed to the right of the title, before the status \
pills:\n",
    );
    write_group(out, row_badge_docs());
}

/// Build the help agent's first message: instructions plus the full
/// generated reference — this user's effective keybindings (grouped
/// by scope, leader menus included) and the embedded docs. Sent once
/// per app lifetime as the opening user turn of the structured run;
/// follow-up questions ride the same conversation so the context is
/// prompt-cached.
///
/// `escape_char` is the configured `terminal.escape_char`; the
/// terminal leave/leader chord is rendered from it (doubled) exactly
/// like the `?` help panel does (#188).
pub fn agent_context(catalog: &[CatalogEntry], escape_char: char) -> String {
    let leader = format!("{escape_char}{escape_char}");
    let mut out = String::with_capacity(256 * 1024);
    out.push_str(
        "You are lazybox's built-in help assistant. lazybox is a reactive PR-inbox TUI: \
provider events (GitHub PRs/issues, Linear, Slack) flow into an inbox of workspaces, and \
each workspace can host embedded terminals running coding agents in isolated git worktrees.\n\
\n\
Answer the user's questions about how to use lazybox from the reference below.\n\
\n\
Rules:\n\
- The keybinding tables are THIS user's effective keymap — their `ui.keymap_preset` and \
`ui.action_keys` overrides are already applied. Quote keys exactly as written there.\n\
- A binding only works in its section's scope; the scope is noted on each section header. \
Global bindings generally do not intercept a focused terminal, so mention terminal scope when relevant.\n\
- Be brief: a few sentences, and when keys are the answer list them as `` `key` — action `` lines.\n\
- Do not use tools, do not read or write files, do not run commands yourself. Everything you need is below. \
The one way you can change anything is by proposing an action (see \"Performing actions\"); lazybox applies it \
after the user confirms.\n\
- If the reference doesn't cover something, say so plainly instead of guessing.\n",
    );

    out.push_str(&format!(
        "\n# Performing actions\n\n\
You can propose a small, allowlisted set of changes that lazybox applies natively after showing the user a \
confirm-with-preview — you never touch the filesystem or shell yourself. When the user asks for one of these, \
answer in one short sentence, then emit a single fenced code block tagged `{ACTION_FENCE}` containing JSON. \
Emit at most one block per reply, and only when the user actually asked you to perform the action (not when \
merely explaining it). If a required field is missing, ask for it instead of guessing.\n\n\
Supported actions:\n\n\
- **add_snippet** — save a reusable prompt the user can send to an agent with `]]s<key>`. Fields: `key` \
(short, no spaces — the trigger), `body` (the prompt text, required), `category` and `description` (optional, \
for the picker). Example:\n\n\
```{ACTION_FENCE}\n\
{{\"action\": \"add_snippet\", \"key\": \"integrate\", \"category\": \"Review\", \"description\": \"Integrate \
review feedback and commit\", \"body\": \"Address every review comment on this PR, then commit and push.\"}}\n\
```\n\n\
See the Snippets doc below for how snippet bodies should read.\n"
    ));

    out.push_str(&format!(
        "- **scaffold_skill** — create a `.claude/skills/<name>/SKILL.md` folder in the current repo. \
Fields: `name` (kebab-case id, e.g. `code-review`), `description` (one line the agent matches on to decide \
when to use the skill — required), `body` (the SKILL.md instructions in markdown, without frontmatter — required). \
Example:\n\n\
```{ACTION_FENCE}\n\
{{\"action\": \"scaffold_skill\", \"name\": \"release-notes\", \"description\": \"Draft release notes from the \
merged PRs since the last tag\", \"body\": \"1. Find the last tag with git.\\n2. List merged PRs since it.\\n3. \
Group them by type and write the notes.\"}}\n\
```\n\n\
**Choosing snippet vs skill:** a snippet is a single prompt the user fires deliberately with `]]s<key>` — pick it \
for a one-shot instruction. A skill is model-triggered and progressively disclosed: pick `scaffold_skill` when the \
ask is genuinely multi-step, or would benefit from bundled scripts/reference files the agent loads on demand. When \
in doubt, prefer a snippet — it's simpler and deterministic.\n"
    ));

    out.push_str(
        "- **edit_config** — set one allowlisted key in the user's config. Fields: `key` (one of the paths \
below, exactly) and `value` (the new value). You can ONLY set these keys — refuse anything else and never \
invent a key:\n",
    );
    for (key, describe) in EDITABLE_CONFIG_KEYS {
        out.push_str(&format!("  - `{key}` — {describe}\n"));
    }
    out.push_str(&format!(
        "\n  Use the exact spelling the reference gives (theme names come from the Themes doc; agent ids are \
the ones enabled above). lazybox validates the value and rejects anything unknown, so if you're unsure of the \
exact value, ask the user rather than guessing. Example:\n\n\
```{ACTION_FENCE}\n\
{{\"action\": \"edit_config\", \"key\": \"ui.theme\", \"value\": \"Lazybox Light\"}}\n\
```\n"
    ));

    out.push_str("\n# Key bindings (effective)\n");
    let mut current_section = None;
    for entry in catalog {
        if current_section != Some(entry.section) {
            current_section = Some(entry.section);
            out.push_str(&format!(
                "\n## {} — {}\n\n",
                entry.section.title(),
                section_scope(entry.section)
            ));
        }
        // The terminal leave/leader chord is dispatched by the escape-
        // char latch, not the catalog — render it from the live char
        // (#188), same as the `?` panel.
        let terminal_exit = format!("{leader}q");
        let keys: &str = if entry.kind == ActionKind::LeaveTerminal {
            &terminal_exit
        } else {
            entry.keys_display.as_ref()
        };
        if keys.is_empty() {
            out.push_str(&format!(
                "- (no key bound; remappable as `{}` via `ui.action_keys`) — {}: {}\n",
                entry.config_key, entry.label, entry.describe
            ));
        } else {
            out.push_str(&format!(
                "- `{keys}` — {}: {}\n",
                entry.label, entry.describe
            ));
        }
    }
    out.push_str(&format!(
        "- `{leader}s<snippet key>` — snippets: from a terminal, send a saved snippet \
(see the Snippets doc below).\n"
    ));

    out.push_str(
        "\n# Leader menus\n\nPressing a leader key opens a which-key menu; \
the next keystroke picks an action:\n",
    );
    let mut leaders: Vec<(KeyStroke, Vec<(KeyStroke, &CatalogEntry)>)> = Vec::new();
    for entry in catalog {
        for chord in &entry.chords {
            let Chord::Seq(strokes) = chord else { continue };
            if strokes.len() != 2 {
                continue;
            }
            match leaders.iter_mut().find(|(l, _)| *l == strokes[0]) {
                Some((_, members)) => members.push((strokes[1], entry)),
                None => leaders.push((strokes[0], vec![(strokes[1], entry)])),
            }
        }
    }
    leaders.sort_by_key(|(_, members)| {
        members
            .first()
            .and_then(|(_, entry)| crate::action::leader_group_label(entry.kind))
            .map(crate::action::leader_group_rank)
            .unwrap_or(usize::MAX)
    });
    for (leader_stroke, members) in &leaders {
        let picks = members
            .iter()
            .map(|(second, entry)| format!("`{}` {}", second.display(), entry.label))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "- press `{}`, then: {picks}\n",
            leader_stroke.display()
        ));
    }
    out.push_str(&format!(
        "\n# Terminal command menu\n\nInside a terminal, press `{leader}` (the terminal escape \
char `{escape_char}`, doubled), then choose:\n\
- `{leader}s<snippet key>` — send a saved snippet\n\
- `{leader}f` — toggle focus mode\n\
- `{leader}q` — exit to the sidebar\n\
- `{leader}\u{60}` — jump to any workspace\n\
- `{leader}1`…`{leader}9` — jump to an agent workspace by sidebar position\n\
- `{leader}|` / `{leader}-` — split right / down\n\
- `{leader}<arrow>` — move tile focus or switch tabs\n\
- `{leader}x` — close the focused terminal\n\
`Esc` or any unbound key cancels back to the terminal.\n"
    ));

    push_markers(&mut out);

    out.push_str("\n# Documentation\n");
    for (title, body) in DOCS {
        out.push_str(&format!("\n---\n\n## Doc: {title}\n\n{body}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ActionDef;

    fn catalog() -> Vec<CatalogEntry> {
        ActionDef::catalog(
            &["claude".to_string(), "codex".to_string()],
            &std::collections::BTreeMap::new(),
        )
    }

    /// The motivating query from #302: "multi-select" isn't a label or
    /// a key, only a description — search must still surface the
    /// activity-pane SelectRow binding for it.
    #[test]
    fn search_finds_multi_select_by_description() {
        let cat = catalog();
        let hits = search(&cat, "multi-select");
        assert!(
            hits.iter().any(|&i| cat[i].kind == ActionKind::SelectRow),
            "multi-select query should surface SelectRow",
        );
    }

    /// Label matches outrank description matches: "merge" must put
    /// the merge-PR row above rows that merely mention merging.
    #[test]
    fn search_ranks_label_matches_first() {
        let cat = catalog();
        let hits = search(&cat, "merge");
        let first = hits.first().map(|&i| cat[i].label.as_ref());
        assert_eq!(first, Some("merge PR"), "hits: {hits:?}");
    }

    /// Every query token must match somewhere — an unrelated token
    /// filters a row out, and an empty query returns nothing.
    #[test]
    fn search_requires_all_tokens_and_handles_empty() {
        let cat = catalog();
        assert!(search(&cat, "").is_empty());
        assert!(search(&cat, "   ").is_empty());
        let hits = search(&cat, "merge zzzznotaword");
        assert!(
            hits.is_empty(),
            "an unmatched token must filter everything out (subsequence \
fallback shouldn't resurrect it)",
        );
    }

    /// Near-miss queries fall back to subsequence matching instead of
    /// returning nothing.
    #[test]
    fn search_falls_back_to_subsequence() {
        let cat = catalog();
        let hits = search(&cat, "mrge pr");
        assert!(
            hits.iter().any(|&i| cat[i].label == "merge PR"),
            "subsequence fallback should catch near-misses",
        );
    }

    /// The generated context reflects the user's *effective* keymap:
    /// under the vim preset merge is `g m` (no `Shift-M` anywhere).
    #[test]
    fn agent_context_uses_post_override_bindings() {
        let overrides = crate::action::keymap_preset("vim").unwrap();
        let cat = ActionDef::catalog(&[], &overrides);
        let ctx = agent_context(&cat, ']');
        assert!(ctx.contains("`g m` — merge PR"), "vim preset chord missing");
    }

    /// Generated per-agent rows and section scope notes appear, and
    /// the terminal leader renders from the live escape char (#188).
    #[test]
    fn agent_context_includes_generated_rows_scopes_and_escape_char() {
        let ctx = agent_context(&catalog(), '}');
        assert!(ctx.contains("spawn claude"));
        assert!(ctx.contains("## Activity — active while the activity pane"));
        assert!(
            ctx.contains("`}}q` — exit to sidebar"),
            "leave-terminal binding should render the complete live exit chord"
        );
        assert!(
            !ctx.contains("`]]q` — exit to sidebar"),
            "no exit binding may render the default escape char"
        );
        assert!(ctx.contains("`}}s<snippet key>` — send a saved snippet"));
        assert!(ctx.contains("`}}|` / `}}-` — split right / down"));
    }

    /// The generated marker registry rides along: Ask Lazybox can now
    /// explain `CHANGES` (the #883 quick win) and every other pill,
    /// agent-state glyph, and row badge without any hand-written prose.
    #[test]
    fn agent_context_includes_generated_markers() {
        let ctx = agent_context(&catalog(), ']');
        assert!(ctx.contains("# Sidebar markers and pills"));
        // The motivating bug: CHANGES was undocumented, so the agent
        // couldn't explain it. It now comes straight from the code.
        assert!(
            ctx.contains("`CHANGES` — A reviewer requested changes"),
            "CHANGES pill must be explained from the generated registry"
        );
        // A sampling of the other generated sources.
        assert!(ctx.contains("## Status pills"));
        assert!(ctx.contains("`CI OK`"));
        assert!(ctx.contains("`CONFLICT`"));
        assert!(ctx.contains("## Agent state"));
        assert!(ctx.contains("Working"));
        assert!(ctx.contains("## Row badges"));
        assert!(ctx.contains("`ARM`"));
        assert!(ctx.contains("`FIX`"));
    }

    /// Every status pill and agent state reaches the generated context —
    /// the drift guard at the help-surface level, on top of the
    /// compile-time exhaustive matches in `crate::markers`.
    #[test]
    fn agent_context_documents_every_pill_and_state() {
        let ctx = agent_context(&catalog(), ']');
        for doc in crate::markers::status_pill_docs() {
            assert!(
                ctx.contains(doc.meaning),
                "status pill {} missing from context",
                doc.label
            );
        }
        for doc in crate::markers::agent_state_docs() {
            assert!(
                ctx.contains(doc.meaning),
                "agent state {} missing from context",
                doc.label
            );
        }
    }

    /// The embedded docs ride along — the snippets doc is the agent's
    /// only source for snippet YAML syntax.
    #[test]
    fn agent_context_embeds_docs() {
        let ctx = agent_context(&catalog(), ']');
        assert!(ctx.contains("## Doc: Snippets"));
        assert!(ctx.contains("## Doc: README"));
        assert!(ctx.contains("## Doc: Themes"));
    }

    /// The context teaches the agent the action vocabulary: both verbs,
    /// every editable config key, and the exact fence tag lazybox parses.
    #[test]
    fn agent_context_describes_actions() {
        let ctx = agent_context(&catalog(), ']');
        assert!(ctx.contains("# Performing actions"));
        assert!(ctx.contains("add_snippet"));
        assert!(ctx.contains("edit_config"));
        assert!(ctx.contains("scaffold_skill"));
        // The classification guidance the agent uses to pick the artifact.
        assert!(ctx.contains(".claude/skills/"));
        assert!(ctx.contains("Choosing snippet vs skill"));
        assert!(ctx.contains(&format!("```{ACTION_FENCE}")));
        for (key, _) in EDITABLE_CONFIG_KEYS {
            assert!(ctx.contains(key), "prompt must list editable key {key}");
        }
    }

    /// A `scaffold_skill` block parses into the intent verbatim; the
    /// client validates the name and writes the folder.
    #[test]
    fn parses_scaffold_skill_intent() {
        let answer = "Sure — I'll scaffold that skill.\n\n\
```lazybox-action\n\
{\"action\":\"scaffold_skill\",\"name\":\"release-notes\",\
\"description\":\"Draft release notes\",\"body\":\"Do the steps.\"}\n\
```\n";
        assert_eq!(
            parse_action_intent(answer),
            Some(HelpActionIntent::ScaffoldSkill {
                name: "release-notes".into(),
                description: "Draft release notes".into(),
                body: "Do the steps.".into(),
            })
        );
    }

    /// An `edit_config` block parses into the intent verbatim; the
    /// client is what enforces the key allowlist, not the parser.
    #[test]
    fn parses_edit_config_intent() {
        let answer = "```lazybox-action\n{\"action\":\"edit_config\",\"key\":\"ui.theme\",\"value\":\"Dracula\"}\n```";
        assert_eq!(
            parse_action_intent(answer),
            Some(HelpActionIntent::EditConfig {
                key: "ui.theme".into(),
                value: "Dracula".into(),
            })
        );
    }

    /// A well-formed `lazybox-action` block parses into the allowlisted
    /// intent with every field populated.
    #[test]
    fn parses_add_snippet_intent() {
        let answer = "Sure — I'll add that snippet.\n\n\
```lazybox-action\n\
{\"action\":\"add_snippet\",\"key\":\"integrate\",\"category\":\"Review\",\
\"description\":\"Integrate feedback\",\"body\":\"Do the thing.\"}\n\
```\n";
        let intent = parse_action_intent(answer).expect("intent parses");
        assert_eq!(
            intent,
            HelpActionIntent::AddSnippet {
                key: "integrate".into(),
                category: "Review".into(),
                description: "Integrate feedback".into(),
                body: "Do the thing.".into(),
            }
        );
    }

    /// Optional fields default to empty; only `key` + `body` are
    /// required.
    #[test]
    fn parses_add_snippet_with_only_required_fields() {
        let answer =
            "```lazybox-action\n{\"action\":\"add_snippet\",\"key\":\"go\",\"body\":\"Go.\"}\n```";
        let intent = parse_action_intent(answer).expect("intent parses");
        assert_eq!(
            intent,
            HelpActionIntent::AddSnippet {
                key: "go".into(),
                category: String::new(),
                description: String::new(),
                body: "Go.".into(),
            }
        );
    }

    /// A plain-prose answer has no intent.
    #[test]
    fn no_intent_without_action_fence() {
        assert!(parse_action_intent("Press `z` to snooze a workspace.").is_none());
    }

    /// Example JSON the agent shows in an ordinary ```json block must
    /// NOT fire an action — only the `lazybox-action` fence counts.
    #[test]
    fn json_fence_is_not_an_action() {
        let answer = "Here's the shape:\n```json\n{\"action\":\"add_snippet\",\"key\":\"x\",\"body\":\"y\"}\n```";
        assert!(parse_action_intent(answer).is_none());
    }

    /// An unknown `action` is outside the allowlist and yields nothing,
    /// even inside a valid fence.
    #[test]
    fn unknown_action_is_rejected() {
        let answer = "```lazybox-action\n{\"action\":\"rm_rf\",\"path\":\"/\"}\n```";
        assert!(parse_action_intent(answer).is_none());
    }

    /// Stripping removes the raw block but keeps the prose on both
    /// sides.
    #[test]
    fn strip_action_block_keeps_surrounding_prose() {
        let answer = "I'll add that snippet.\n\n\
```lazybox-action\n{\"action\":\"add_snippet\",\"key\":\"go\",\"body\":\"Go.\"}\n```\n\n\
Trigger it with `]]sgo`.";
        let stripped = strip_action_block(answer);
        assert!(!stripped.contains("lazybox-action"));
        assert!(!stripped.contains("add_snippet"));
        assert!(stripped.contains("I'll add that snippet."));
        assert!(stripped.contains("Trigger it with `]]sgo`."));
    }

    /// Only the first block is executed, but a stray second block must
    /// not survive in the transcript either — every `lazybox-action`
    /// fence is stripped, and the prose between them is kept.
    #[test]
    fn strip_removes_every_action_block() {
        let answer = "First.\n\n\
```lazybox-action\n{\"action\":\"add_snippet\",\"key\":\"a\",\"body\":\"A.\"}\n```\n\n\
Middle.\n\n\
```lazybox-action\n{\"action\":\"add_snippet\",\"key\":\"b\",\"body\":\"B.\"}\n```\n\n\
Last.";
        let stripped = strip_action_block(answer);
        assert!(!stripped.contains("lazybox-action"), "both blocks gone");
        assert!(!stripped.contains("add_snippet"));
        assert_eq!(stripped, "First.\n\nMiddle.\n\nLast.");
    }
}
