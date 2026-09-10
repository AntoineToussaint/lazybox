//! The tracker-record rule in the work preamble (#1586).
//!
//! Lives in `tests/` rather than beside the other `prompts.rs` unit tests
//! on purpose: `crates/ipc/build.rs` hashes the *contents* of every file
//! under `crates/core/src` into `LAZYBOX_PROTOCOL_FINGERPRINT`, and
//! `ipc::socket::{client,server}_handshake` hard-fail a connection whose
//! peer fingerprint differs. A test added under `src/` therefore breaks
//! every out-of-process client (`--connect`, the SSH-forwarded remote
//! path) against a daemon built one commit earlier, and forces a
//! `make desktop-contract` regen — a real protocol break bought by a
//! prose guard. `crates/core/tests/` is not hashed, and `pub mod prompts`
//! makes the constant reachable, so the same guard costs nothing here.

use lazybox_core::prompts::AGENT_WORK_PREAMBLE;

/// Strip the shell-prompt decoration and whitespace variance a command
/// line can pick up, so the "is this line an invocation?" check below
/// can't be sidestepped by `$ `, indentation, or a doubled space.
fn as_command_line(line: &str) -> String {
    let line = line.trim();
    let line = line
        .strip_prefix("$ ")
        .or_else(|| line.strip_prefix("$"))
        .unwrap_or(line);
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The preamble with every run of whitespace collapsed to one space.
/// The markdown is hard-wrapped, so a phrase these guards care about can
/// land with a newline through the middle of it (`**off\nby default**`);
/// matching the raw text would make every guard hostage to the reflow of
/// an unrelated edit above it.
fn flowed() -> String {
    AGENT_WORK_PREAMBLE
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn preamble_keeps_the_tracker_record_section() {
    // The section header is guarded here rather than in the `src`-side
    // `preamble_keeps_every_principle_section` for the fingerprint reason
    // in the module docs: adding a needle there would move the wire
    // fingerprint. Dropping the section silently restores the pre-#1586
    // steering, so it needs a guard either way.
    assert!(
        AGENT_WORK_PREAMBLE.contains("## The tracker record is the workspace"),
        "the preamble must keep the tracker-record section"
    );
}

#[test]
fn preamble_starts_new_work_from_the_tracker_record() {
    // #1586: the preamble used to tell every agent to run `lazybox
    // workspace create --name …` "instead of filing an issue", which is
    // exactly the split the rule forbids — the branch, activity, cost,
    // claim, and epic graph end up on a second row beside the issue's own.
    // It rides in *every* work prompt, so it is the loudest voice an agent
    // hears on the subject and must name the issue-first path.
    assert!(
        AGENT_WORK_PREAMBLE.contains("gh issue create"),
        "preamble must name the issue-first path for new work"
    );
    // The create command may still be *named* — the rule spells out what
    // not to do — but never handed over as a copy-pasteable command. A
    // line that is itself the invocation is the fenced block this fix
    // removed, so guard the shape rather than the removed prose.
    assert!(
        !AGENT_WORK_PREAMBLE
            .lines()
            .any(|line| as_command_line(line).starts_with("lazybox workspace create")),
        "preamble must not offer `lazybox workspace create` as a command to run"
    );
}

#[test]
fn preamble_does_not_promise_a_workspace_for_a_filed_issue() {
    // #1586's first pass replaced one false instruction with another:
    // "Lazybox opens that issue's workspace on the next poll". It does
    // not, on a default install. `ProviderConfig::default_for("github")`
    // enables only `pr.*` keys, so `issue_enabled()` is false, which
    // skips the issue half of the repo sweep (`want_issues`) AND the
    // `involves:USER is:issue` discovery probe, and
    // `filter_github_tasks_with_watches` drops the row on the type/role
    // gate regardless. A repo outside the configured scopes is dropped by
    // the same function's scope gate. So the agent files an issue, reports
    // that lazybox will pick it up, and the work is stranded with no error
    // and no row — the exact silent-loss shape the rule exists to prevent.
    let text = flowed();
    for needle in [
        "The filed issue is your deliverable",
        "Do not assume a workspace appeared",
        "off by default",
        "a repo outside the configured scopes is filtered out",
    ] {
        assert!(
            text.contains(needle),
            "preamble must not promise a workspace it cannot deliver; missing {needle:?}"
        );
    }
}

#[test]
fn preamble_teaches_the_cross_repo_parent_form() {
    // A bare `--parent <n>` resolves inside `--repo`, so a sub-issue filed
    // for a cross-repo epic (#1517's whole shape) lands under an unrelated
    // same-repo issue or errors out. `gh issue create --help`: "the
    // specified parent number or URL". The preamble must teach the URL
    // form and say why, or the epic graph is silently wrong.
    let text = flowed();
    assert!(
        text.contains("--parent <parent-issue-url>"),
        "preamble must teach the URL parent form"
    );
    assert!(
        text.contains("a number resolves inside `--repo`"),
        "preamble must say why a bare parent number fails across repos"
    );
}

#[test]
fn preamble_fences_no_runnable_placeholder_text() {
    // The `gh issue create` block replaced a fenced `lazybox workspace
    // create` command. Its first form used `--body "…"` — a literal U+2026
    // inside shell quotes, which runs happily and files an issue whose
    // body is one ellipsis. Placeholders in this file are angle-bracketed
    // (`<n>`, `<owner/repo>`); a quoted-ellipsis argument is a runnable
    // command wearing a placeholder's clothes.
    assert!(
        !AGENT_WORK_PREAMBLE.contains("\"…\""),
        "a quoted ellipsis is a runnable argument, not a placeholder"
    );
}

/// Every built-in prompt string, flowed. The role preambles need a context to
/// render; a blank one exercises the fallbacks and still carries all the prose.
fn builtin_prompts() -> Vec<(&'static str, String)> {
    use lazybox_core::Role;
    use lazybox_core::prompts::{RolePromptCtx, role_preamble};
    let ctx = RolePromptCtx::default();
    let mut out = vec![("agent-work.md", AGENT_WORK_PREAMBLE.to_string())];
    for role in [
        Role::Planner,
        Role::Coordinator,
        Role::Worker,
        Role::Reviewer,
        Role::Integrator,
    ] {
        out.push((role.display_name(), role_preamble(role, &ctx)));
    }
    out
}

#[test]
fn no_builtin_prompt_hands_over_a_named_workspace_create() {
    // #1586 §6: the built-in prompts are the loudest instructions an agent
    // hears, and `lazybox workspace create --name` is the one command that
    // produces the split the rule exists to prevent — a second row beside a
    // tracker record, holding the branch, activity, cost and epic membership
    // the operator is watching the record for. The CLI keeps the flag for
    // repo-less scratch, and the preamble *names* it to forbid it, so the
    // guard is the same one `preamble_starts_new_work_from_the_tracker_record`
    // uses — a line that IS the invocation — extended to every role preamble.
    for (what, text) in builtin_prompts() {
        let offered = text
            .lines()
            .find(|line| as_command_line(line).starts_with("lazybox workspace create"));
        assert!(
            offered.is_none(),
            "built-in prompt `{what}` must not offer `lazybox workspace create` as a command \
             to run: {offered:?}"
        );
    }
}

#[test]
fn the_coordinator_preamble_starts_workers_on_a_record() {
    // `spawn_worker` no longer takes a `workspace_name` (#1586 §1). A
    // Coordinator briefed only with "start workers with `spawn_worker`"
    // discovers that by getting an error mid-fan-out, so the preamble names
    // the record-shaped arguments up front.
    use lazybox_core::Role;
    use lazybox_core::prompts::{RolePromptCtx, role_preamble};
    let text = role_preamble(Role::Coordinator, &RolePromptCtx::default());
    let flowed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for needle in ["`task`", "`create_issue`", "never a named one beside it"] {
        assert!(
            flowed.contains(needle),
            "the Coordinator preamble must name {needle}: {text}"
        );
    }
}
