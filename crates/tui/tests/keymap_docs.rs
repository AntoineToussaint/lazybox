//! Keymap-reference docs generator + drift guard (#12 of the
//! keybinding audit).
//!
//! `web/src/content/docs/docs/reference/keybindings.md` is GENERATED
//! from the runtime action catalog (default preset, built-in agent
//! trio, Claude's built-in tier menu) so the published reference can't
//! drift from the shipped keymap the way the hand-written page did
//! (it still advertised the removed `c`/`x`/`u` and `Shift-*` aliases).
//!
//! - Normal test run: regenerates in memory and asserts the checked-in
//!   file matches — drift fails the build with a regen hint.
//! - `LAZYBOX_REGEN_KEYMAP_DOCS=1 cargo test -p lazybox-tui --test
//!   keymap_docs`: rewrites the file in place.
//!
//! The page's Starlight frontmatter (title/description) is preserved
//! verbatim from the existing file. Non-catalog extras — pane
//! navigation, mouse, and pickers come from the static appendix below.
//! The terminal `]]` menu is generated from the runtime command table,
//! so dispatch, the which-key popup, and this page share one source.

use lazybox_tui_core::action::{
    ActionDef, ActionKind, CatalogEntry, Chord, Guard, KeyStroke, Section, leader_group_label,
};
use std::path::PathBuf;

/// Fallback frontmatter if the page ever goes missing entirely.
const DEFAULT_FRONTMATTER: &str = "---\n\
title: Keybindings reference\n\
description: The full keymap for every pane, leader chord, and picker.\n\
---\n";

fn docs_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src/content/docs/docs/reference/keybindings.md")
}

/// The `---`-fenced frontmatter block at the top of `existing`,
/// including both fences and the trailing newline.
fn extract_frontmatter(existing: &str) -> Option<String> {
    let rest = existing.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some(format!("---\n{}\n---\n", &rest[..end]))
}

/// The default-preset runtime catalog the page documents: the built-in
/// agent trio and Claude's built-in model-tier menu, no user overrides
/// (the in-app `?` help shows the user's live bindings; this page shows
/// the defaults).
fn default_catalog() -> Vec<CatalogEntry> {
    let agents: Vec<String> = ["claude", "codex", "cursor"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let tiers = lazybox_core::AgentModels::builtin("claude")
        .expect("claude ships a built-in tier menu")
        .tiers;
    ActionDef::catalog_with_tiers(&agents, &std::collections::BTreeMap::new(), &tiers)
}

/// Escape `|` for use inside a markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// A key rendered as a markdown code span. Keys containing a backtick
/// (the `` ` `` workspace picker) need double-backtick delimiters.
fn key_span(text: &str) -> String {
    let text = cell(text);
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

/// First sentence of a catalog `describe`, for the table's description
/// column — the full text is help-panel copy and too long for a row.
fn first_sentence(describe: &str) -> &str {
    match describe.split_once(". ") {
        Some((first, _)) => &describe[..first.len() + 1],
        None => describe,
    }
}

/// Guard annotation appended to an action cell.
fn guard_note(kind: ActionKind) -> &'static str {
    match ActionDef::for_kind(kind).guard() {
        Guard::Confirm { .. } => " *(confirmed first)*",
        Guard::DoublePress => " *(two-press chord)*",
        Guard::None => "",
    }
}

/// Whether the entry renders inside a leader-menu table rather than
/// the flat per-section grid — mirrors the `?` help panel's split: any
/// two-stroke `Seq` chord that is NOT the q-latch double-press.
fn is_leader_entry(entry: &CatalogEntry) -> bool {
    !matches!(ActionDef::for_kind(entry.kind).guard(), Guard::DoublePress)
        && entry
            .chords
            .iter()
            .any(|c| matches!(c, Chord::Seq(s) if s.len() == 2))
}

fn section_intro(section: Section) -> &'static str {
    match section {
        Section::Global => {
            "Work from any non-terminal pane. A focused terminal forwards keys to the PTY; \
             press `]]q` to return first. **Text selection (`F8`) is the deliberate exception** \
             and works inside a terminal too."
        }
        Section::Workspace => {
            "Act on the focused workspace. Available from the sidebar **and** the activity \
             pane (the sidebar selection stays the reference frame while reading activity)."
        }
        Section::Sidebar => "Manage the sidebar list itself — only while the sidebar has focus.",
        Section::Activity => "Only while the activity (right) pane has focus.",
        Section::Terminal => {
            "A focused terminal forwards every key to the PTY; only the chords below are \
             intercepted. `]]` (the escape char, doubled) opens the terminal command menu."
        }
    }
}

fn generate(frontmatter: &str) -> String {
    let catalog = default_catalog();
    let mut out = String::with_capacity(16 * 1024);
    out.push_str(frontmatter);
    out.push_str(
        "\n<!-- GENERATED FILE — do not edit by hand.\n     \
         Regenerate with: LAZYBOX_REGEN_KEYMAP_DOCS=1 cargo test -p lazybox-tui --test keymap_docs\n     \
         Source: crates/tui/tests/keymap_docs.rs (renders the runtime action catalog). -->\n",
    );
    out.push_str(
        "\nThe full default keymap, generated from the action catalog \
         (`crates/tui-core/src/action.rs`). Press `?` in the app to open **Ask Lazybox**: \
         typing searches your live bindings immediately, and Enter asks in plain language. \
         Press `?` again at its empty prompt for the compact shortcut index. If you've \
         remapped keys via `ui.action_keys` or selected a preset, those in-app surfaces show \
         your effective bindings; this page shows the defaults.\n",
    );
    out.push_str(
        "\nGrouped actions live behind **leader keys** (press the leader, a which-key menu \
         pops up, the next key picks the action). The default keymap is leaders-only: no \
         single-key aliases for grouped actions.\n",
    );
    out.push_str(
        "\n## The design\n\n\
         The keymap follows five rules:\n\n\
         1. **Ask, don't memorize.** `?` searches the effective keymap and answers workflow \
            questions; the static index is secondary.\n\
         2. **Common verbs stay direct.** `Enter` opens, `s` starts a shell, `e` opens the \
            editor, `r` replies, `z` snoozes, and `/` searches.\n\
         3. **Families use mnemonic leaders.** `w` work, `a` agent, `b` main branch, `g` \
            GitHub, and `x` workspace management. Every leader opens a visible menu.\n\
         4. **Risk requires intent.** Destructive/provider mutations are behind a leader and \
            confirmation; quit is `q q`.\n\
         5. **Terminal mode has one escape namespace.** `]]` opens every lazybox command that \
            must work while the embedded program owns the keyboard.\n",
    );

    // ── Flat per-section tables ─────────────────────────────────────
    for section in [
        Section::Global,
        Section::Workspace,
        Section::Sidebar,
        Section::Activity,
        Section::Terminal,
    ] {
        out.push_str(&format!("\n## {}\n\n", section.title()));
        out.push_str(section_intro(section));
        out.push_str("\n\n| Key | Action | What it does |\n| --- | --- | --- |\n");
        for entry in catalog
            .iter()
            .filter(|e| e.section == section && !e.keys_display.is_empty())
            .filter(|e| !is_leader_entry(e))
        {
            out.push_str(&format!(
                "| {} | {}{} | {} |\n",
                key_span(&entry.keys_display),
                cell(&entry.label),
                guard_note(entry.kind),
                cell(first_sentence(entry.describe)),
            ));
        }
        if section == Section::Sidebar {
            out.push_str(
                "\n`j` / `k` (or arrows) move the cursor; `Esc` clears a `v` multi-selection.\n",
            );
        }
        if section == Section::Activity {
            out.push_str(
                "\n`j` / `k` (or arrows) move the row cursor; `→`/`l` expand and `←`/`h` \
                 collapse the focused row; `w w` works on the selection.\n",
            );
        }
        if section == Section::Terminal {
            out.push_str(&terminal_appendix());
        }
    }

    // ── Leader menus ────────────────────────────────────────────────
    out.push_str(
        "\n## Leader menus\n\nPress the leader key, then the second key. Every menu shows \
         a which-key popup while it waits.\n",
    );
    let mut leaders: Vec<(KeyStroke, Vec<&CatalogEntry>)> = Vec::new();
    for entry in &catalog {
        if !is_leader_entry(entry) {
            continue;
        }
        for chord in &entry.chords {
            let Chord::Seq(strokes) = chord else { continue };
            if strokes.len() != 2 {
                continue;
            }
            match leaders.iter_mut().find(|(l, _)| *l == strokes[0]) {
                Some((_, members)) => members.push(entry),
                None => leaders.push((strokes[0], vec![entry])),
            }
        }
    }
    leaders.sort_by_key(|(_, members)| {
        members
            .first()
            .and_then(|entry| leader_group_label(entry.kind))
            .map(lazybox_tui_core::action::leader_group_rank)
            .unwrap_or(usize::MAX)
    });
    for (leader, members) in &leaders {
        let label = members
            .iter()
            .find_map(|e| leader_group_label(e.kind))
            .unwrap_or("leader");
        out.push_str(&format!(
            "\n### {} — {}\n\n",
            key_span(&leader.display()),
            label
        ));
        if *leader == KeyStroke::parse("w").expect("w parses") {
            out.push_str(
                "`w` opens a deterministic work menu: press `w w` for the default or \
                 already-running agent, or choose an agent / model tier below. Nothing waits \
                 on a timeout, so the second key acts immediately.\n\n",
            );
        }
        out.push_str("| Chord | Action |\n| --- | --- |\n");
        for entry in members {
            out.push_str(&format!(
                "| {} | {}{} |\n",
                key_span(&entry.keys_display),
                cell(&entry.label),
                guard_note(entry.kind),
            ));
        }
    }

    // ── Static appendix: mouse + pickers ────────────────────────────
    out.push_str(
        "\n## Mouse\n\n\
         - Click any pane to focus it; drag a splitter to resize.\n\
         - Right-click a sidebar row for the context menu; right-click a URL / path / `#N` \
           reference inside a terminal to open it.\n\
         - The wheel scrolls the pane under the cursor (terminal scrollback included).\n\
         - The mouse-capture toggle (`F8` / `Alt-s` / `Ctrl-Alt-s`) hands the mouse back to \
           the host terminal for native text selection.\n",
    );
    out.push_str(
        "\n## Pickers\n\n\
         The filter-as-you-type pickers — snippets (`]]s`), jump-to-workspace (`` ` ``), and \
         prompt history (`]]h`) — share one key protocol. Because typing filters the list, the \
         letter keys go to the filter and the arrows (not `j` / `k`) move the selection:\n\n\
         | Key | Action |\n| --- | --- |\n\
         | `↑` / `↓` | Move the selection |\n\
         | `Home` / `End` | Jump to the first / last match |\n\
         | `Enter` | Confirm the highlighted row |\n\
         | (type) / `Backspace` | Edit the filter |\n\
         | `Esc` / `Ctrl-c` | Dismiss |\n\n\
         The snippet picker adds two shortcuts on top of this protocol: typing a key that \
         uniquely identifies one snippet submits it immediately (so `]]srev` sends `rev` with \
         no `Enter`), and `Ctrl-F` skips straight to free text in the broadcast flow.\n\n\
         The reviewers / labels / assignees picker (`Choice`) is a different modal — a \
         multi-select with no filter, toggled with `Space` — so it does not follow the \
         protocol above.\n",
    );
    out
}

/// The Terminal section's non-catalog extras. The `]]` command rows come
/// from the same runtime table as dispatch and the which-key popup; only
/// the pane-owned scrollback keys remain static here.
fn terminal_appendix() -> String {
    let mut out = String::new();
    out.push_str(
        "\n### The `]]` terminal leader\n\n\
         `]]` is a non-timed leader: after the two presses it waits for the command key. \
         `Esc` or any unbound key cancels back into the terminal; a lone `]` followed by \
         any other key is sent to the program verbatim. The escape char is configurable \
         (`terminal.escape_char`; the legacy `ui.terminal_escape_char` alias is still accepted).\n\n\
         | Chord | Action |\n| --- | --- |\n",
    );
    for (suffix, description) in lazybox_tui::realm::model::terminal_leader_reference_rows() {
        out.push_str(&format!(
            "| {} | {} |\n",
            key_span(&format!("]]{suffix}")),
            cell(&description),
        ));
    }
    out.push_str(
        "\n### Scrollback\n\n\
         | Key | Action |\n| --- | --- |\n\
         | `Shift-PgUp` / `Shift-PgDn` | Scroll the scrollback |\n\
         | `Shift-Home` / `Shift-End` | Jump to the top / bottom |\n\
         | `Ctrl-c` | Forwarded to the program as an interrupt |\n",
    );
    out
}

#[test]
fn keymap_reference_page_is_current() {
    let path = docs_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let frontmatter =
        extract_frontmatter(&existing).unwrap_or_else(|| DEFAULT_FRONTMATTER.to_string());
    let generated = generate(&frontmatter);

    if std::env::var_os("LAZYBOX_REGEN_KEYMAP_DOCS").is_some() {
        std::fs::write(&path, &generated).expect("write keybindings.md");
        return;
    }
    assert!(
        existing == generated,
        "web/src/content/docs/docs/reference/keybindings.md is out of date with the \
         action catalog.\nRegenerate it with:\n\n    \
         LAZYBOX_REGEN_KEYMAP_DOCS=1 cargo test -p lazybox-tui --test keymap_docs\n\n\
         (The page is generated from crates/tui/tests/keymap_docs.rs — don't edit it by hand.)",
    );
}

/// The generator's own sanity: the page names the leaders-only chords
/// and none of the retired direct aliases.
#[test]
fn generated_page_reflects_the_leaders_only_default_keymap() {
    let page = generate(DEFAULT_FRONTMATTER);
    for expected in [
        "`g m`",
        "`g p`",
        "`a c`",
        "`w c`",
        "`b s`",
        "text selection",
        "]]r",
        "]]t",
        "]]x",
        "Shift-Home",
        "new project",
    ] {
        assert!(page.contains(expected), "page missing {expected:?}");
    }
    // Retired single-key agent aliases must not resurface as bindings.
    assert!(
        !page.contains("| `c` |") && !page.contains("| `x` |") && !page.contains("| `u` |"),
        "retired direct agent aliases leaked into the generated page",
    );
}
