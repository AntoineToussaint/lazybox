#[cfg(test)]
mod truncate_tests {
    use super::super::truncate_ellipsis;

    #[test]
    fn fits_unchanged() {
        assert_eq!(truncate_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn clips_with_ellipsis() {
        assert_eq!(truncate_ellipsis("hello world", 8), "hello w…");
    }

    #[test]
    fn zero_and_one_budgets() {
        assert_eq!(truncate_ellipsis("hello", 0), "");
        assert_eq!(truncate_ellipsis("hello", 1), "…");
    }

    #[test]
    fn handles_multibyte() {
        // Characters are kept whole (no byte-slicing into UTF-8).
        let s = "naïve résumé";
        let out = truncate_ellipsis(s, 6);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 6);
    }
}

#[cfg(test)]
mod multi_agent_state_tests {
    use super::super::*;
    use lazybox_ipc::{AgentState, Event, TerminalSnapshot};

    fn snapshot_terminal(
        terminal_id: u64,
        session_key: &SessionKey,
        state: AgentState,
    ) -> TerminalSnapshot {
        TerminalSnapshot {
            terminal_id: TerminalId(terminal_id),
            session_key: session_key.clone(),
            kind: TerminalKind::Agent("codex".into()),
            replay: Vec::new(),
            last_seq: 0,
            replay_available: true,
            no_permission: false,
            on_main: false,
            model_label: None,
            prompt_history: Vec::new(),
            composing_buffer: None,
            agent_state: Some(state),
            authenticating: false,
        }
    }

    #[test]
    fn snapshot_aggregation_preserves_input_needed_in_any_terminal_order() {
        let session_key = SessionKey::from("test:multi-agent");
        for terminals in [
            vec![
                snapshot_terminal(1, &session_key, AgentState::InputNeeded),
                snapshot_terminal(2, &session_key, AgentState::Working),
            ],
            vec![
                snapshot_terminal(2, &session_key, AgentState::Working),
                snapshot_terminal(1, &session_key, AgentState::InputNeeded),
            ],
        ] {
            let mut sidebar = Sidebar::new(PaneId::new(1));
            sidebar.on_event(&Event::Snapshot {
                workspaces: Vec::new(),
                terminals,
                projects: Vec::new(),
                recent_snippets: Vec::new(),
                dismissed_updates: Vec::new(),
            });
            assert_eq!(
                sidebar.agent_state(&session_key),
                Some(AgentState::InputNeeded),
                "a working sibling must not hide an agent waiting for input"
            );
        }
    }

    #[test]
    fn live_terminal_exit_reveals_the_remaining_agent_state() {
        let session_key = SessionKey::from("test:multi-agent-live");
        let mut sidebar = Sidebar::new(PaneId::new(1));
        sidebar.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key: session_key.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        sidebar.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(2),
            session_key: session_key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        sidebar.on_event(&Event::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::InputNeeded,
        });
        sidebar.on_event(&Event::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::Working,
        });
        assert_eq!(
            sidebar.agent_state(&session_key),
            Some(AgentState::InputNeeded)
        );

        sidebar.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });
        assert_eq!(
            sidebar.agent_state(&session_key),
            Some(AgentState::Working),
            "removing one terminal must recompute from the surviving terminal"
        );

        sidebar.on_event(&Event::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::Exited { code: Some(1) },
        });
        sidebar.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: Some(1),
            last_output: None,
        });
        assert_eq!(
            sidebar.agent_state(&session_key),
            Some(AgentState::Exited { code: Some(1) }),
            "the lifecycle tombstone remains visible after its terminal closes"
        );
    }
}

#[cfg(test)]
mod status_pill_tests {
    use super::super::status_pill;
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};

    pub(super) fn base_task() -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "gh".into(),
                key: "o/r#1".into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "u".into(),
            repo: Some("o/r".into()),
            branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    /// Status pills are now single-glyph icons (#1046), each a leading
    /// space + one BMP symbol, so the trailer stays narrow and hands its
    /// old width to the title. Guard that every pill is compact (≤ 3
    /// cells) rather than a wide text block.
    #[test]
    fn every_pill_label_is_a_compact_glyph() {
        let ci_cases: &[CiStatus] = &[
            CiStatus::Failure,
            CiStatus::Mixed,
            CiStatus::Running,
            CiStatus::Pending,
            CiStatus::Success,
        ];
        for ci in ci_cases {
            let mut t = base_task();
            t.ci = *ci;
            let pill = status_pill(&t).expect("CI status should produce a pill");
            assert!(
                crate::util::visual_width(pill.label) <= 3,
                "label {:?} for {:?} is not a compact glyph",
                pill.label,
                ci,
            );
        }
        let state_cases: &[TaskState] = &[TaskState::Draft, TaskState::Merged, TaskState::Closed];
        for state in state_cases {
            let mut t = base_task();
            t.state = *state;
            let pill = status_pill(&t).expect("state should produce a pill");
            assert!(
                crate::util::visual_width(pill.label) <= 3,
                "label {:?} for {:?} is not a compact glyph",
                pill.label,
                state,
            );
        }
    }

    #[test]
    fn ci_failure_renders_fail_glyph() {
        let mut t = base_task();
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " ✗");
    }

    #[test]
    fn ci_success_renders_ok_glyph() {
        // CI passing renders a green `✓` glyph instead of an empty
        // status column or a wide ` CI OK ` block (#1046).
        let mut t = base_task();
        t.ci = CiStatus::Success;
        let pill = status_pill(&t).expect("Success should produce a pill");
        assert_eq!(pill.label, " ✓");
    }

    #[test]
    fn ci_running_renders_running_glyph() {
        let mut t = base_task();
        t.ci = CiStatus::Running;
        assert_eq!(status_pill(&t).unwrap().label, " ◔");
        t.ci = CiStatus::Pending;
        assert_eq!(status_pill(&t).unwrap().label, " ◔");
    }

    #[test]
    fn ci_mixed_renders_mixed_glyph() {
        let mut t = base_task();
        t.ci = CiStatus::Mixed;
        assert_eq!(status_pill(&t).unwrap().label, " ±");
    }

    #[test]
    fn conflicts_trump_ci_status() {
        let mut t = base_task();
        t.mergeable = lazybox_core::Mergeable::Conflicting;
        t.ci = CiStatus::Success;
        // `⚠` carries a trailing U+FE0E text-presentation selector so it
        // renders one cell wide on emoji-forcing terminals (#1046).
        assert_eq!(status_pill(&t).unwrap().label, " ⚠\u{fe0e}");
    }

    #[test]
    fn merged_renders_merged_pill_overriding_ci() {
        // A closed PR's CI history is frozen; the user can't act on
        // it. Show the merged glyph instead of a stale CI fail.
        let mut t = base_task();
        t.state = TaskState::Merged;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " ⋈");
    }

    #[test]
    fn merged_glyph_is_distinct_from_actionable_ok_states() {
        // #1079: a merged PR (terminal, done-and-gone) must not share the
        // `✓` used for the actionable ready / approved / CI-green trio, and
        // it must read as a dimmed/terminal state rather than an active
        // signal. Color alone was too weak a distinction.
        let theme = crate::theme::current();

        let mut merged = base_task();
        merged.state = TaskState::Merged;
        let merged_pill = status_pill(&merged).expect("merged renders a pill");

        let mut ready = base_task();
        ready.review = ReviewStatus::Approved;
        ready.ci = CiStatus::Success;
        let ready_pill = status_pill(&ready).expect("ready renders a pill");

        let mut approved = base_task();
        approved.review = ReviewStatus::Approved;
        approved.ci = CiStatus::Running;
        let approved_pill = status_pill(&approved).expect("approved renders a pill");

        // Distinct glyph, not the shared `✓`.
        assert_eq!(merged_pill.label, " ⋈");
        assert_ne!(merged_pill.label, ready_pill.label);
        assert_ne!(merged_pill.label, approved_pill.label);
        assert_eq!(ready_pill.label, " ✓");
        assert_eq!(approved_pill.label, " ✓");

        // Terminal / past-tense styling: dimmed, unlike the bright
        // actionable `✓`s.
        assert_eq!(merged_pill.style.fg, Some(theme.text_dim));
        assert_ne!(merged_pill.style.fg, ready_pill.style.fg);
        assert_ne!(merged_pill.style.fg, approved_pill.style.fg);
    }

    #[test]
    fn closed_renders_closed_pill_overriding_ci() {
        let mut t = base_task();
        t.state = TaskState::Closed;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " ⊘");
    }

    #[test]
    fn draft_renders_draft_pill_when_ci_is_quiet() {
        // CI green or running, state Draft → the draft glyph wins so the
        // user remembers the PR isn't ready for review.
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " ◇");
    }

    #[test]
    fn ci_failure_beats_draft() {
        // A draft with red CI still needs the user's attention more
        // urgently than the draft state itself — the fail glyph wins.
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " ✗");
    }

    #[test]
    fn draft_row_shows_conflict_alongside_draft() {
        // #1058: the row renderer (`status_pills`, two slots) must not let
        // Draft swallow a real blocker. A conflicting draft shows `◇ ⚠` —
        // draft primary in slot one, the conflict glyph in slot two — so the
        // conflict is visible without un-drafting the PR.
        use super::super::status_pills;
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.mergeable = lazybox_core::Mergeable::Conflicting;
        let (a, b) = status_pills(&t);
        assert_eq!(a.unwrap().label, " ◇", "slot one stays the draft glyph");
        assert_eq!(
            b.unwrap().label,
            " ⚠\u{fe0e}",
            "slot two surfaces the conflict on a draft (#1058)",
        );
    }

    #[test]
    fn draft_row_shows_failing_ci_alongside_draft() {
        // #1058: a CI-failing draft surfaces the fail glyph in the second
        // slot rather than being hidden behind `◇`.
        use super::super::status_pills;
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Failure;
        let (a, b) = status_pills(&t);
        assert_eq!(a.unwrap().label, " ◇");
        assert_eq!(
            b.unwrap().label,
            " ✗",
            "slot two surfaces failing CI on a draft (#1058)",
        );
    }

    #[test]
    fn draft_row_conflict_beats_ci_in_second_slot() {
        // Both a conflict and failing CI on a draft: the conflict wins the
        // single blocker slot, mirroring `lifecycle_pill`'s precedence for
        // non-draft PRs (there are only two slots, draft owns the first).
        use super::super::status_pills;
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.mergeable = lazybox_core::Mergeable::Conflicting;
        t.ci = CiStatus::Failure;
        let (_a, b) = status_pills(&t);
        assert_eq!(b.unwrap().label, " ⚠\u{fe0e}");
    }

    #[test]
    fn draft_row_shows_mixed_ci_alongside_draft() {
        // Mixed CI (partly failing) is a blocker too — surface it.
        use super::super::status_pills;
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Mixed;
        let (a, b) = status_pills(&t);
        assert_eq!(a.unwrap().label, " ◇");
        assert_eq!(
            b.unwrap().label,
            " ±",
            "slot two surfaces mixed CI on a draft"
        );
    }

    #[test]
    fn draft_row_quiet_ci_shows_only_draft() {
        // No conflict, CI not configured → just `◇`, second slot empty.
        use super::super::status_pills;
        let mut t = base_task();
        t.state = TaskState::Draft;
        let (a, b) = status_pills(&t);
        assert_eq!(a.unwrap().label, " ◇");
        assert!(b.is_none(), "a clean draft carries no blocker glyph");
    }

    #[test]
    fn draft_row_non_blocking_ci_shows_only_draft() {
        // Green or in-flight CI is not a blocker — the second slot stays
        // empty so `◇ ✓` never reads as "good to go" on a not-ready PR.
        // Only conflict / failing / mixed CI earn the blocker slot.
        use super::super::status_pills;
        for ci in [CiStatus::Success, CiStatus::Running, CiStatus::Pending] {
            let mut t = base_task();
            t.state = TaskState::Draft;
            t.ci = ci;
            let (a, b) = status_pills(&t);
            assert_eq!(a.unwrap().label, " ◇");
            assert!(
                b.is_none(),
                "non-blocking CI {ci:?} must not add a glyph beside the draft marker",
            );
        }
    }

    #[test]
    fn ci_none_with_no_conflicts_renders_no_pill() {
        let t = base_task();
        assert!(status_pill(&t).is_none());
    }

    #[test]
    fn approved_plus_green_ci_renders_ready() {
        // The "this is mergeable right now" signal — both the human
        // half (review) and the machine half (CI) are done → the ready
        // glyph (green `✓`).
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " ✓");
    }

    #[test]
    fn approved_with_no_ci_yet_still_renders_ready() {
        // Some repos don't run CI on every PR (or the rollup is still
        // empty after a fresh push). Approval alone is enough to call
        // it ready rather than holding back forever.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::None;
        assert_eq!(status_pill(&t).unwrap().label, " ✓");
    }

    #[test]
    fn approved_with_running_ci_renders_approved() {
        // Human approval landed; CI is still chewing. The user can
        // safely walk away — once green, the PR is mergeable.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Running;
        assert_eq!(status_pill(&t).unwrap().label, " ✓");
    }

    #[test]
    fn ci_failure_overrides_approval() {
        // Approval is great but red CI still trumps — that's the
        // actionable problem.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " ✗");
    }
}

#[cfg(test)]
mod status_pill_consistency_tests {
    //! The renderer (`status_pill` → `pill_for_tag`) is a pure
    //! mapping from `StatusTag::for_task`. These tests pin the
    //! contract:
    //!
    //! - Every non-`None` tag produces `Some(pill)`. A missing arm
    //!   would mean the rendered row silently drops a real signal
    //!   (the bug that motivated this audit: status_pill used to
    //!   skip `ChangesRequested` / `Queued` / `ReviewPending`
    //!   entirely).
    //!
    //! - Every pill label is a compact single glyph (#1046) so the
    //!   reclaimed width goes to the title.
    //!
    //! - The `None` tag is the only tag that renders no pill.
    //!
    //! Adding a new `StatusTag` variant without a `pill_for_tag`
    //! arm is a compile error; adding a new arm without these
    //! tests catching it is the gap this module closes.

    use super::super::{pill_for_tag, status_pill};
    use super::status_pill_tests::base_task;
    use lazybox_core::{CiStatus, ReviewStatus, StatusTag, TaskState};

    /// Every variant of `StatusTag` the contract sweeps over. Keep
    /// this list exhaustive — a new variant on `StatusTag` should
    /// fail to compile here until added, because the match below
    /// is exhaustive. (The `let _: () = match` is the
    /// exhaustiveness pin.)
    const ALL_TAGS: &[StatusTag] = &[
        StatusTag::Merged,
        StatusTag::Closed,
        StatusTag::Conflict,
        StatusTag::CiFailed,
        StatusTag::CiMixed,
        StatusTag::ChangesRequested,
        StatusTag::Queued,
        StatusTag::Draft,
        StatusTag::Ready,
        StatusTag::Approved,
        StatusTag::ReviewPending,
        StatusTag::CiRunning,
        StatusTag::CiOk,
        StatusTag::Behind,
        StatusTag::None,
    ];

    #[test]
    fn all_tags_list_is_exhaustive() {
        // Compile-time exhaustiveness pin. If a new `StatusTag`
        // variant lands without being added to `ALL_TAGS`, this
        // arm-by-arm match stops compiling — forcing the
        // contributor to extend the sweep below at the same time.
        for tag in ALL_TAGS {
            let _: () = match tag {
                StatusTag::Merged => (),
                StatusTag::Closed => (),
                StatusTag::Conflict => (),
                StatusTag::CiFailed => (),
                StatusTag::CiMixed => (),
                StatusTag::ChangesRequested => (),
                StatusTag::Queued => (),
                StatusTag::Draft => (),
                StatusTag::Ready => (),
                StatusTag::Approved => (),
                StatusTag::ReviewPending => (),
                StatusTag::CiRunning => (),
                StatusTag::CiOk => (),
                StatusTag::Behind => (),
                StatusTag::None => (),
            };
        }
    }

    #[test]
    fn every_non_none_tag_renders_a_pill() {
        // No tag (except None) should silently drop. This was the
        // original bug: status_pill skipped CHANGES / QUEUED /
        // AUTO / REVIEW entirely, so a PR with changes requested
        // showed no signal in the trailer.
        for tag in ALL_TAGS {
            let pill = pill_for_tag(*tag);
            match tag {
                StatusTag::None => assert!(
                    pill.is_none(),
                    "StatusTag::None must render no pill, got {:?}",
                    pill.map(|p| p.label),
                ),
                other => assert!(pill.is_some(), "StatusTag::{other:?} must render a pill"),
            }
        }
    }

    #[test]
    fn every_pill_label_is_a_compact_glyph() {
        // The status trailer is a single-glyph icon per slot now (#1046),
        // not a fixed 10-cell text block. Guard that every tag's pill is
        // compact (≤ 3 cells: a leading space + one BMP symbol) so the
        // reclaimed width goes to the title.
        for tag in ALL_TAGS {
            if let Some(p) = pill_for_tag(*tag) {
                assert!(
                    crate::util::visual_width(p.label) <= 3,
                    "StatusTag::{tag:?} label {:?} is not a compact glyph",
                    p.label,
                );
            }
        }
    }

    #[test]
    fn changes_requested_now_renders_a_pill() {
        // Regression for the original bug: a PR with
        // ReviewStatus::ChangesRequested and no other CI/conflict
        // signal used to fall through to None and show no pill.
        let mut t = base_task();
        t.review = ReviewStatus::ChangesRequested;
        let pill = status_pill(&t).expect("changes-requested must produce a pill");
        assert_eq!(pill.label, " ✗");
    }

    #[test]
    fn auto_merge_is_not_a_status_pill() {
        // #778: GitHub-native auto-merge is a policy, not a status —
        // it renders as its own `◆` row pill (see
        // `workspace_row::cell_auto`), never in the status column. With
        // no other signal, an armed PR shows no status pill at all…
        let mut t = base_task();
        t.auto_merge_enabled = true;
        assert!(
            status_pill(&t).is_none(),
            "auto-merge alone must not produce a status pill",
        );
        // …and, crucially, it never suppresses a red-CI status pill.
        t.ci = lazybox_core::CiStatus::Failure;
        let pill = status_pill(&t).expect("failing CI must still produce a pill");
        assert_eq!(pill.label, " ✗");
    }

    #[test]
    fn queued_now_renders_a_pill() {
        let mut t = base_task();
        t.is_in_merge_queue = true;
        let pill = status_pill(&t).expect("in-merge-queue must produce a pill");
        assert_eq!(pill.label, " ⧖");
    }

    #[test]
    fn review_pending_now_renders_a_pill() {
        let mut t = base_task();
        t.review = ReviewStatus::Pending;
        let pill = status_pill(&t).expect("review-pending must produce a pill");
        assert_eq!(pill.label, " ◌");
    }

    /// The pills Ask Lazybox documents (`lazybox_tui_core::markers`)
    /// must be exactly the pills the sidebar actually paints. The
    /// renderer is `status_pills` (the two-column producer used by
    /// `workspace_row`), not the `StatusTag`→pill map — they diverge
    /// (`Behind` is a `StatusTag` variant with no rendered pill), and the
    /// help context is keyed to `StatusTag`, so a documented-but-
    /// unrendered pill (or a rendered-but-undocumented one) would let the
    /// help agent lie about the UI. This sweep is that drift guard; it
    /// lives here because only `tui` sees both sides.
    #[test]
    fn documented_status_pills_match_the_renderer() {
        use super::super::status_pills;
        use lazybox_core::Mergeable;
        use std::collections::BTreeSet;

        let states = [
            TaskState::Open,
            TaskState::InReview,
            TaskState::InProgress,
            TaskState::Merged,
            TaskState::Closed,
            TaskState::Draft,
        ];
        let reviews = [
            ReviewStatus::None,
            ReviewStatus::Pending,
            ReviewStatus::Approved,
            ReviewStatus::ChangesRequested,
        ];
        let cis = [
            CiStatus::None,
            CiStatus::Pending,
            CiStatus::Running,
            CiStatus::Success,
            CiStatus::Failure,
            CiStatus::Mixed,
        ];
        let mergeables = [
            Mergeable::Mergeable,
            Mergeable::Conflicting,
            Mergeable::Unknown,
        ];

        let mut rendered: BTreeSet<String> = BTreeSet::new();
        for &state in &states {
            for &review in &reviews {
                for &ci in &cis {
                    for &mergeable in &mergeables {
                        for in_queue in [false, true] {
                            for behind in [false, true] {
                                let mut t = base_task();
                                t.state = state;
                                t.review = review;
                                t.ci = ci;
                                t.mergeable = mergeable;
                                t.is_in_merge_queue = in_queue;
                                t.is_behind_base = behind;
                                let (a, b) = status_pills(&t);
                                for pill in [a, b].into_iter().flatten() {
                                    rendered.insert(pill.label.trim().to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        let documented: BTreeSet<String> = lazybox_tui_core::markers::status_pill_docs()
            .into_iter()
            .map(|d| d.label.trim().to_string())
            .collect();

        assert_eq!(
            documented, rendered,
            "Ask Lazybox's documented status pills must be exactly what the sidebar renders \
             (left = documented in tui-core::markers, right = emitted by status_pills)"
        );
    }

    #[test]
    fn task_pill_matches_tag_priority() {
        // Sanity-check the pipeline: for a handful of (task) inputs
        // the pill rendered must match the pill mapped from the
        // tag computed by `StatusTag::for_task`. Catches drift if
        // someone reintroduces priority logic into `pill_for_tag`.
        let mut cases: Vec<lazybox_core::Task> = Vec::new();
        cases.push({
            let mut t = base_task();
            t.mergeable = lazybox_core::Mergeable::Conflicting;
            t
        });
        cases.push({
            let mut t = base_task();
            t.state = TaskState::Draft;
            t.review = ReviewStatus::Approved;
            t.ci = CiStatus::Success;
            t
        });
        cases.push({
            let mut t = base_task();
            t.state = TaskState::Merged;
            t
        });
        cases.push({
            let mut t = base_task();
            t.review = ReviewStatus::Approved;
            t.ci = CiStatus::Running;
            t
        });
        for t in &cases {
            let via_task = status_pill(t).map(|p| p.label);
            let via_tag = pill_for_tag(StatusTag::for_task(t)).map(|p| p.label);
            assert_eq!(
                via_task, via_tag,
                "status_pill must equal pill_for_tag(StatusTag::for_task(task))",
            );
        }
    }

    /// WCAG relative luminance of an sRGB color. Built-in theme colors are
    /// always `Color::Rgb` (pinned by the theme module's own tests), so a
    /// non-Rgb here means a status glyph reached for a fixed palette index
    /// instead of a theme tone — the exact #1046 regression.
    fn luminance(c: ratatui::style::Color) -> f32 {
        let ratatui::style::Color::Rgb(r, g, b) = c else {
            panic!("status glyph color {c:?} is not a theme-derived Color::Rgb");
        };
        let lin = |v: u8| {
            let s = v as f32 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
    }

    fn contrast_ratio(a: ratatui::style::Color, b: ratatui::style::Color) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// #1046 regression: status glyphs are foreground-colored on
    /// `theme.surface`, so a fixed bright palette color (the old
    /// `Color::Indexed(220)` yellow) or a near-surface theme grey (`chrome`,
    /// the old draft tone) is unreadable on the near-white Lazybox Light
    /// surface — where the old black-on-color pill *fills* always held
    /// contrast. Every glyph a status column can emit must clear the 3:1
    /// floor for graphical indicators on that surface. The old CI colors
    /// (green `40` ~1.5:1, yellow `220` ~1.3:1) would fail this.
    #[test]
    fn status_glyph_colors_are_legible_on_the_light_theme() {
        use crate::theme;
        let prev = theme::current().name;
        assert!(
            theme::set_by_name("Lazybox Light"),
            "light theme must exist"
        );
        // Sample every tag's fg while the light theme is active, then
        // restore immediately so the brief global switch can't bleed into a
        // concurrently-rendering test (same pattern the theme module uses).
        let sampled: Vec<(StatusTag, ratatui::style::Color)> = ALL_TAGS
            .iter()
            .filter_map(|&tag| {
                pill_for_tag(tag).map(|p| (tag, p.style.fg.expect("glyph has a fg")))
            })
            .collect();
        let surface = theme::current().surface;
        theme::set_by_name(prev);

        for (tag, fg) in sampled {
            let ratio = contrast_ratio(fg, surface);
            assert!(
                ratio >= 3.0,
                "StatusTag::{tag:?} glyph color {fg:?} has {ratio:.2}:1 contrast on the light \
                 surface — below the 3:1 floor (must use a contrast-tuned theme tone)",
            );
        }
    }

    /// #1048: the live sidebar pill (`status_pills` → `lifecycle_pill`,
    /// what the row actually renders) honors the per-repo approval
    /// policy. A bot-only approval under `approval: human` must NOT read
    /// as READY — it shows REVIEW-pending; a human approval restores
    /// READY; and the default policy still treats a bot approval as
    /// enough. Guards against the pill diverging from the merge gate /
    /// `StatusTag::for_task`.
    #[test]
    fn human_approval_policy_governs_ready_pill() {
        use super::super::status_pills;

        // Compare against the tag→glyph mapping rather than a literal
        // glyph so this test doesn't re-pin the #1046 glyph choices.
        let ready_glyph = pill_for_tag(StatusTag::Ready).map(|p| p.label);
        let review_glyph = pill_for_tag(StatusTag::ReviewPending).map(|p| p.label);

        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        t.approval_policy = lazybox_core::ApprovalPolicy::Human;
        t.reviews = vec![lazybox_core::Reviewer {
            login: "claude".into(),
            state: lazybox_core::ReviewState::Approved,
            is_bot: true,
        }];
        let (review, _ci) = status_pills(&t);
        assert_eq!(
            review.map(|p| p.label),
            review_glyph,
            "bot-only approval under `human` shows REVIEW-pending, not READY",
        );

        // A human approval alongside the bot's flips it to the READY
        // end-state (a single lifecycle pill, no separate CI slot).
        t.reviews.push(lazybox_core::Reviewer {
            login: "alice".into(),
            state: lazybox_core::ReviewState::Approved,
            is_bot: false,
        });
        let (ready, ci) = status_pills(&t);
        assert_eq!(
            ready.map(|p| p.label),
            ready_glyph,
            "a human approval restores READY under `human`",
        );
        assert!(ci.is_none(), "READY is a single lifecycle pill");

        // Default policy: the bot approval alone is READY (unchanged).
        t.reviews.truncate(1);
        t.approval_policy = lazybox_core::ApprovalPolicy::Default;
        let (ready, _) = status_pills(&t);
        assert_eq!(
            ready.map(|p| p.label),
            ready_glyph,
            "default policy counts a bot approval — pre-#1048 behavior",
        );
    }
}

#[cfg(test)]
mod workspace_type_label_tests {
    use super::super::*;
    use lazybox_core::{Workspace, WorkspaceKey};

    fn empty_ws() -> Workspace {
        Workspace::empty(WorkspaceKey::new("k"), "main", chrono::Utc::now())
    }

    fn task(url: &str) -> lazybox_core::Task {
        let mut t = super::status_pill_tests::base_task();
        t.url = url.into();
        t
    }

    #[test]
    fn pr_workspace_returns_pr_glyph() {
        let mut w = empty_ws();
        w.attach_task(task("https://github.com/o/r/pull/1"));
        assert_eq!(workspace_type_label(&w, false), Some("⇄"));
    }

    #[test]
    fn issue_workspace_returns_issue_glyph() {
        let mut w = empty_ws();
        w.attach_task(task("https://github.com/o/r/issues/42"));
        assert_eq!(workspace_type_label(&w, false), Some("○"));
    }

    #[test]
    fn linear_only_workspace_returns_linear_glyph() {
        // Distinct source from github — issues that came from Linear
        // get `◆` so the row gives a stronger "where does this
        // live?" signal at a glance.
        let mut w = empty_ws();
        let mut t = task("https://linear.app/team/issue/ABC-7");
        t.id.source = "linear".into();
        w.attach_task(t);
        assert_eq!(workspace_type_label(&w, false), Some("◆"));
    }

    #[test]
    fn pr_workspace_with_linked_issue_still_labels_pr() {
        // Merged via closingIssuesReferences: workspace has both a
        // PR slot and a gh_issue. PR is the primary identity.
        let mut w = empty_ws();
        w.attach_task(task("https://github.com/o/r/pull/1"));
        w.attach_task(task("https://github.com/o/r/issues/42"));
        assert_eq!(workspace_type_label(&w, false), Some("⇄"));
    }

    #[test]
    fn all_workspace_type_labels_are_one_cell() {
        // Column alignment invariant — the `NNN` number sits
        // immediately after the glyph, so the type column is
        // exactly one cell wide on every row. See issue #42.
        for ascii in [false, true] {
            for label in [
                workspace_type_label(
                    &{
                        let mut w = empty_ws();
                        w.attach_task(task("https://github.com/o/r/pull/1"));
                        w
                    },
                    ascii,
                ),
                workspace_type_label(
                    &{
                        let mut w = empty_ws();
                        w.attach_task(task("https://github.com/o/r/issues/1"));
                        w
                    },
                    ascii,
                ),
                workspace_type_label(
                    &{
                        let mut w = empty_ws();
                        let mut t = task("https://linear.app/team/issue/ABC-1");
                        t.id.source = "linear".into();
                        w.attach_task(t);
                        w
                    },
                    ascii,
                ),
            ] {
                let label = label.expect("each workspace shape has a label");
                assert_eq!(
                    label.chars().count(),
                    1,
                    "label {label:?} (ascii={ascii}) must be exactly 1 cell",
                );
            }
        }
    }

    /// ASCII fallback exposes plain letters so fonts that don't
    /// render the unicode glyphs reliably still get a usable marker.
    #[test]
    fn ascii_fallback_returns_letters() {
        let mut pr = empty_ws();
        pr.attach_task(task("https://github.com/o/r/pull/1"));
        assert_eq!(workspace_type_label(&pr, true), Some("p"));

        let mut issue = empty_ws();
        issue.attach_task(task("https://github.com/o/r/issues/1"));
        assert_eq!(workspace_type_label(&issue, true), Some("i"));

        let mut linear = empty_ws();
        let mut t = task("https://linear.app/team/issue/ABC-1");
        t.id.source = "linear".into();
        linear.attach_task(t);
        assert_eq!(workspace_type_label(&linear, true), Some("l"));
    }

    #[test]
    fn empty_workspace_returns_none() {
        let w = empty_ws();
        assert_eq!(workspace_type_label(&w, false), None);
        assert_eq!(workspace_type_label(&w, true), None);
    }
}

#[cfg(test)]
mod mailbox_membership_tests {
    //! Cell tests for the `mailbox_membership` predicate. The
    //! filter used to live inline in `recompute_visible_inner` with
    //! the snoozed-merged interaction untested — exactly the kind
    //! of state-cell drift the user has been pushing back on.
    //! Each `(workspace state, mailbox)` cell gets one assertion;
    //! a new mailbox semantic is one helper + ~6 assertions.

    use super::super::{Mailbox, mailbox_membership};
    use chrono::{Duration, Utc};
    use lazybox_core::{TaskState, Workspace, WorkspaceKey};

    fn ws(state: Option<TaskState>) -> Workspace {
        ws_with_updated_at(state, Utc::now() - Duration::hours(2))
    }

    /// Build a workspace with an explicit `updated_at` so the
    /// grace-window tests can pin both ends (within grace = shown
    /// in Inbox; outside grace = not shown).
    ///
    /// Default `ws()` uses `now - 2h` so it's OUTSIDE the 30-min
    /// grace — most tests don't want the grace path to fire and
    /// would otherwise need to re-specify updated_at every time.
    fn ws_with_updated_at(
        state: Option<TaskState>,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Workspace {
        let now = Utc::now();
        let mut w = Workspace::empty(WorkspaceKey::new("k"), "main", now);
        if let Some(s) = state {
            let mut task = super::status_pill_tests::base_task();
            task.state = s;
            task.updated_at = updated_at;
            task.url = "https://github.com/o/r/pull/1".into();
            w.attach_task(task);
        }
        w
    }

    fn snoozed(mut w: Workspace) -> Workspace {
        w.snoozed_until = Some(Utc::now() + Duration::hours(1));
        w
    }

    // ── Inbox ────────────────────────────────────────────────────

    #[test]
    fn open_pr_is_in_inbox() {
        let w = ws(Some(TaskState::Open));
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn draft_pr_is_in_inbox() {
        let w = ws(Some(TaskState::Draft));
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn merged_pr_is_not_in_inbox_by_default() {
        let w = ws(Some(TaskState::Merged));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn closed_pr_is_not_in_inbox_by_default() {
        let w = ws(Some(TaskState::Closed));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn merged_pr_is_in_inbox_when_show_inactive_in_inbox_is_on() {
        let w = ws(Some(TaskState::Merged));
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), true));
    }

    #[test]
    fn freshly_merged_pr_stays_in_inbox_during_grace_window() {
        // User watches a PR merge between polls. The
        // recently-merged sweep brings it back with state=Merged.
        // The grace window (INACTIVE_GRACE) keeps it visible in
        // Inbox so the user sees the MERGED pill instead of the
        // row vanishing on their next refresh.
        let now = Utc::now();
        let w = ws_with_updated_at(
            Some(TaskState::Merged),
            now - Duration::minutes(5), // well inside the 30-min grace
        );
        assert!(
            mailbox_membership(&w, Mailbox::Inbox, now, false),
            "merged within grace must stay visible in Inbox",
        );
        assert!(
            mailbox_membership(&w, Mailbox::Inactive, now, false),
            "and is also in Inactive — its permanent home",
        );
    }

    #[test]
    fn freshly_closed_pr_stays_in_inbox_during_grace_window() {
        let now = Utc::now();
        let w = ws_with_updated_at(Some(TaskState::Closed), now - Duration::minutes(10));
        assert!(mailbox_membership(&w, Mailbox::Inbox, now, false));
    }

    #[test]
    fn merged_pr_past_grace_window_falls_out_of_inbox() {
        // 2 hours after merge: the row belongs in Inactive only.
        let now = Utc::now();
        let w = ws_with_updated_at(Some(TaskState::Merged), now - Duration::hours(2));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, now, false));
        assert!(mailbox_membership(&w, Mailbox::Inactive, now, false));
    }

    /// Regression for #96. A merged PR keeps drawing activity after
    /// the merge — branch deletion, the auto-close comment on linked
    /// issues, deploy/CI statuses — and GitHub bumps `updated_at`
    /// each time. The grace window must clock off the stable
    /// `closed_at`, so a long-merged PR stays OUT of the Inbox even
    /// when it was touched seconds ago.
    #[test]
    fn merged_pr_with_recent_activity_still_leaves_inbox() {
        let now = Utc::now();
        // `updated_at` a minute ago (fresh activity), but merged 2h ago.
        let mut w = ws_with_updated_at(Some(TaskState::Merged), now - Duration::minutes(1));
        if let Some(pr) = w.pr.as_mut() {
            pr.closed_at = Some(now - Duration::hours(2));
        }
        assert!(
            !mailbox_membership(&w, Mailbox::Inbox, now, false),
            "a long-merged PR must stay out of Inbox despite recent activity",
        );
        assert!(mailbox_membership(&w, Mailbox::Inactive, now, false));
    }

    /// The flip side: a just-merged PR shows in the Inbox grace
    /// window even when `updated_at` is stale (last polled hours ago,
    /// then merged on GitHub between polls). The grace clock is
    /// `closed_at`, not `updated_at`.
    #[test]
    fn freshly_merged_pr_in_grace_clocks_off_closed_at() {
        let now = Utc::now();
        let mut w = ws_with_updated_at(Some(TaskState::Merged), now - Duration::hours(2));
        if let Some(pr) = w.pr.as_mut() {
            pr.closed_at = Some(now - Duration::minutes(5));
        }
        assert!(
            mailbox_membership(&w, Mailbox::Inbox, now, false),
            "a just-merged PR must show in the Inbox grace window",
        );
    }

    #[test]
    fn empty_workspace_is_in_inbox() {
        let w = ws(None);
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    // ── Inactive ─────────────────────────────────────────────────

    #[test]
    fn merged_pr_is_in_inactive() {
        let w = ws(Some(TaskState::Merged));
        assert!(mailbox_membership(&w, Mailbox::Inactive, Utc::now(), false));
    }

    #[test]
    fn closed_pr_is_in_inactive() {
        let w = ws(Some(TaskState::Closed));
        assert!(mailbox_membership(&w, Mailbox::Inactive, Utc::now(), false));
    }

    #[test]
    fn open_pr_is_not_in_inactive() {
        let w = ws(Some(TaskState::Open));
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
    }

    #[test]
    fn empty_workspace_is_not_in_inactive() {
        let w = ws(None);
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
    }

    // ── Snoozed wins over everything ─────────────────────────────

    #[test]
    fn snoozed_open_pr_is_only_in_snoozed() {
        let w = snoozed(ws(Some(TaskState::Open)));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
        assert!(mailbox_membership(&w, Mailbox::Snoozed, Utc::now(), false));
    }

    #[test]
    fn snoozed_merged_pr_is_only_in_snoozed_not_inactive() {
        // The exact failure mode the audit called out: a merged-AND-
        // snoozed PR must NOT leak into Inactive. Snoozed wins.
        let w = snoozed(ws(Some(TaskState::Merged)));
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
        assert!(mailbox_membership(&w, Mailbox::Snoozed, Utc::now(), false));
    }

    #[test]
    fn snoozed_merged_pr_is_not_in_inbox_even_with_show_inactive() {
        // `show_inactive_in_inbox` flips merged → Inbox, but snooze
        // still wins over that.
        let w = snoozed(ws(Some(TaskState::Merged)));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), true));
    }

    #[test]
    fn unsnoozed_open_pr_is_not_in_snoozed() {
        let w = ws(Some(TaskState::Open));
        assert!(!mailbox_membership(&w, Mailbox::Snoozed, Utc::now(), false));
    }
}

#[cfg(test)]
mod attention_signal_tests {
    //! Single-source-of-truth contract: every "needs attention"
    //! signal flows through `workspace_attention_signals`. The
    //! per-repo badge (`workspace_needs_attention`) and the header
    //! counters (`input_pending_count` / `ci_failing_count` /
    //! `review_pending_count`) used to compute their own predicates
    //! and drifted — a workspace with reviewers requested but no
    //! ChangesRequested/Pending status used to bump the `N review`
    //! header counter but NOT the repo attention dot. Now both
    //! read the same signals.

    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{ReviewStatus, TaskRole, Workspace};

    fn ws_from_pr(mut task: lazybox_core::Task) -> Workspace {
        // The classifier slots tasks based on URL — `/pull/N` lands in
        // the PR slot, everything else falls through to gh_issues.
        // Force a PR URL so `primary_task` returns this task.
        if !task.url.contains("/pull/") {
            task.url = "https://github.com/o/r/pull/1".into();
        }
        Workspace::from_task(task, chrono::Utc::now())
    }

    fn empty_set() -> std::collections::HashMap<SessionKey, lazybox_ipc::AgentState> {
        std::collections::HashMap::new()
    }

    fn set_with(ws: &Workspace) -> std::collections::HashMap<SessionKey, lazybox_ipc::AgentState> {
        let mut s = std::collections::HashMap::new();
        s.insert(
            SessionKey::from(&ws.key),
            lazybox_ipc::AgentState::InputNeeded,
        );
        s
    }

    #[test]
    fn no_signals_when_quiet() {
        // Plain open PR, no review, no CI, no unread: no signals.
        let w = ws_from_pr(base_task());
        assert!(workspace_attention_signals(&w, &empty_set()).is_empty());
    }

    #[test]
    fn ci_failure_emits_ci_failing_signal() {
        let mut t = base_task();
        t.ci = lazybox_core::CiStatus::Failure;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::CiFailing),
        );
    }

    #[test]
    fn ci_mixed_also_emits_ci_failing_signal() {
        // CI Mixed is a "partial failure" — treated the same as
        // Failure for attention purposes.
        let mut t = base_task();
        t.ci = lazybox_core::CiStatus::Mixed;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::CiFailing),
        );
    }

    #[test]
    fn reviewers_requested_emits_review_signal_even_without_pending_status() {
        let mut t = base_task();
        t.review = ReviewStatus::None;
        t.reviewers = vec!["alice".into()];
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::ReviewPending),
        );
    }

    #[test]
    fn changes_requested_emits_review_signal() {
        let mut t = base_task();
        t.review = ReviewStatus::ChangesRequested;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::ReviewPending),
        );
    }

    #[test]
    fn mentioned_role_emits_mentioned_signal() {
        let mut t = base_task();
        t.role = TaskRole::Mentioned;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::Mentioned),
        );
    }

    #[test]
    fn agent_asking_signal_comes_from_state_map_not_workspace_sessions() {
        // Regression for the silent-clobber bug: the AgentAsking signal
        // MUST be driven by the sidebar-local `agents` state map, NOT
        // `Workspace.sessions[i].state`. The poll cycle reloads
        // workspace data from store every minute, which would
        // wipe a state-mutation-based signal.
        let w = ws_from_pr(base_task());

        // No entry in the map → no signal even if sessions claim
        // Asking (in production they never do, but the test pins
        // the contract).
        assert!(
            !workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::AgentAsking),
        );

        // Add the workspace's key to the set → signal fires.
        assert!(
            workspace_attention_signals(&w, &set_with(&w)).contains(&AttentionSignal::AgentAsking),
        );
    }

    // ── needs_attention vs the gate ───────────────────────────────

    #[test]
    fn needs_attention_returns_false_when_all_signals_gated_off() {
        let mut t = base_task();
        t.ci = lazybox_core::CiStatus::Failure;
        t.review = ReviewStatus::ChangesRequested;
        let w = ws_from_pr(t);
        let cfg = lazybox_config::AttentionConfig {
            unread: false,
            ci_failing: false,
            review_pending: false,
            agent_asking: false,
            mentioned: false,
            ..Default::default()
        };
        assert!(!workspace_needs_attention(&w, &cfg, &empty_set()));
    }

    #[test]
    fn needs_attention_returns_true_when_any_gated_on_signal_active() {
        let mut t = base_task();
        t.ci = lazybox_core::CiStatus::Failure;
        let w = ws_from_pr(t);
        let mut cfg = lazybox_config::AttentionConfig {
            unread: false,
            ci_failing: false,
            review_pending: false,
            agent_asking: false,
            mentioned: false,
            ..Default::default()
        };
        assert!(
            !workspace_needs_attention(&w, &cfg, &empty_set()),
            "all gates off → false",
        );
        cfg.ci_failing = true;
        assert!(
            workspace_needs_attention(&w, &cfg, &empty_set()),
            "CI gate on → true",
        );
    }

    // ── consistency contract: badge vs counter ─────────────────────

    #[test]
    fn reviewers_requested_workspace_lights_both_counter_and_attention() {
        let mut t = base_task();
        t.review = ReviewStatus::None;
        t.reviewers = vec!["alice".into()];
        let w = ws_from_pr(t);
        let signals = workspace_attention_signals(&w, &empty_set());
        assert!(signals.contains(&AttentionSignal::ReviewPending));
        let cfg = lazybox_config::AttentionConfig::default();
        assert!(workspace_needs_attention(&w, &cfg, &empty_set()));
    }
}

#[cfg(test)]
mod filter_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{CiStatus, TaskRole, Workspace};
    use std::collections::HashMap;

    fn ws_with_role(key: &str, role: TaskRole) -> Workspace {
        let mut t = base_task();
        t.id.key = key.into();
        t.url = format!("https://github.com/o/r/pull/{key}");
        t.role = role;
        Workspace::from_task(t, chrono::Utc::now())
    }

    fn ctx<'a>(
        w: &'a Workspace,
        agents: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
    ) -> FilterCtx<'a> {
        FilterCtx { w, agents }
    }

    #[test]
    fn empty_filter_set_accepts_everything() {
        let set = FilterSet::default();
        assert!(set.is_empty());
        let w = ws_with_role("1", TaskRole::Author);
        let agents = HashMap::new();
        assert!(set.accepts(&ctx(&w, &agents)));
    }

    #[test]
    fn every_filter_has_an_axis_and_appears_in_all() {
        // ALL must list each variant exactly once; drives the menu.
        assert_eq!(Filter::ALL.len(), 25);
        let mut seen = std::collections::BTreeSet::new();
        for f in Filter::ALL {
            assert!(seen.insert(f), "{f:?} listed twice in Filter::ALL");
            // axis() is total — just call it.
            let _ = f.axis();
            assert!(!f.label().is_empty());
        }
    }

    #[test]
    fn filter_all_groups_axes_into_contiguous_runs() {
        // The menu prints a section header only when the axis changes
        // between adjacent rows, so an interleaved ALL would render
        // duplicate `State`/`Role` headers. Assert each axis forms one
        // contiguous run: no axis reappears after a different one.
        let mut runs: Vec<FilterAxis> = Vec::new();
        for f in Filter::ALL {
            if runs.last() != Some(&f.axis()) {
                runs.push(f.axis());
            }
        }
        let mut distinct = runs.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            runs.len(),
            distinct.len(),
            "an axis reappears after another — ALL is interleaved: {runs:?}"
        );
    }

    #[test]
    fn role_filter_matches_only_its_role() {
        let agents = HashMap::new();
        let author = ws_with_role("1", TaskRole::Author);
        let reviewer = ws_with_role("2", TaskRole::Reviewer);
        assert!(Filter::Author.matches(&ctx(&author, &agents)));
        assert!(!Filter::Author.matches(&ctx(&reviewer, &agents)));
    }

    #[test]
    fn same_axis_filters_or_across_axes_and() {
        let agents = HashMap::new();
        let author = ws_with_role("1", TaskRole::Author);
        let reviewer = ws_with_role("2", TaskRole::Reviewer);

        // Two Role filters OR: both an author and a reviewer pass.
        let mut role_or = FilterSet::default();
        role_or.toggle(Filter::Author);
        role_or.toggle(Filter::Reviewer);
        assert!(role_or.accepts(&ctx(&author, &agents)));
        assert!(role_or.accepts(&ctx(&reviewer, &agents)));

        // Adding a Kind filter ANDs: base_task PRs are `Pr`, so the
        // author (a PR) still passes but an `Issue`-kind wouldn't.
        let mut role_and_kind = role_or.clone();
        role_and_kind.toggle(Filter::Pr);
        assert!(role_and_kind.accepts(&ctx(&author, &agents)));
        role_and_kind.toggle(Filter::Pr);
        role_and_kind.toggle(Filter::Issue);
        // author is a PR, not an issue → Kind axis now fails.
        assert!(!role_and_kind.accepts(&ctx(&author, &agents)));
    }

    #[test]
    fn ci_failing_state_filter_matches_failing_ci() {
        let agents = HashMap::new();
        // A PR (pull URL routes to the `pr` slot) with failing CI.
        let mut t = base_task();
        t.url = "https://github.com/o/r/pull/1".into();
        t.ci = CiStatus::Failure;
        let failing = Workspace::from_task(t, chrono::Utc::now());
        let healthy = ws_with_role("2", TaskRole::Author);
        assert!(Filter::CiFailing.matches(&ctx(&failing, &agents)));
        assert!(!Filter::CiFailing.matches(&ctx(&healthy, &agents)));
    }

    #[test]
    fn set_filters_narrows_visible_list_and_chips_reflect_active() {
        let mut sb = Sidebar::new(PaneId::new(1));
        for (key, role) in [
            ("1", TaskRole::Author),
            ("2", TaskRole::Reviewer),
            ("3", TaskRole::Assignee),
            ("4", TaskRole::Mentioned),
        ] {
            let w = ws_with_role(key, role);
            let sk = SessionKey::from(&w.key);
            sb.workspaces.insert(sk, w);
        }
        sb.recompute_visible();
        assert_eq!(sb.workspace_count(), 4, "no filters → all four show");

        sb.set_filters([Filter::Author]);
        assert_eq!(sb.workspace_count(), 1, "author filter → 1 row");
        assert_eq!(sb.filters().chips(), vec!["author"]);

        // Author OR Reviewer → two rows.
        sb.set_filters([Filter::Author, Filter::Reviewer]);
        assert_eq!(sb.workspace_count(), 2);

        sb.set_filters([]);
        assert!(sb.filters().is_empty());
        assert_eq!(sb.workspace_count(), 4);
    }

    #[test]
    fn filter_menu_entries_cover_every_fixed_filter_with_counts() {
        use crate::components::sidebar::FilterEntry;
        let mut sb = Sidebar::new(PaneId::new(1));
        let w = ws_with_role("1", TaskRole::Author);
        sb.workspaces.insert(SessionKey::from(&w.key), w);
        sb.recompute_visible();
        let entries = sb.filter_menu_entries();
        // Every fixed predicate appears (value axes add rows on top).
        let by: std::collections::HashMap<Filter, usize> = entries
            .iter()
            .filter_map(|(e, c)| match e {
                FilterEntry::Predicate(f) => Some((*f, *c)),
                _ => None,
            })
            .collect();
        assert_eq!(by.len(), Filter::ALL.len());
        // The single authored PR is counted under Author and PR.
        assert_eq!(by[&Filter::Author], 1);
        assert_eq!(by[&Filter::Pr], 1);
        assert_eq!(by[&Filter::Reviewer], 0);
    }

    #[test]
    fn sort_mode_default_is_split() {
        assert_eq!(SortMode::default(), SortMode::ByRoleSplit);
    }

    #[test]
    fn sort_mode_cycles_through_three_variants() {
        let order = [
            SortMode::Recent,
            SortMode::ByRole,
            SortMode::ByRoleSplit,
            SortMode::Recent,
        ];
        let mut cur = SortMode::Recent;
        for expected in &order[1..] {
            cur = cur.next();
            assert_eq!(cur, *expected);
        }
    }

    #[test]
    fn role_rank_orders_author_first_then_reviewer_assignee_mentioned() {
        // Sort key invariant: Author < Reviewer < Assignee < Mentioned
        // (lower rank = higher in the list).
        assert!(role_rank(Some(TaskRole::Author)) < role_rank(Some(TaskRole::Reviewer)));
        assert!(role_rank(Some(TaskRole::Reviewer)) < role_rank(Some(TaskRole::Assignee)));
        assert!(role_rank(Some(TaskRole::Assignee)) < role_rank(Some(TaskRole::Mentioned)));
        // Orphans (no primary task) sort last so they pile up at the
        // bottom of any ByRole group instead of disrupting the
        // ordered head.
        assert!(role_rank(Some(TaskRole::Mentioned)) < role_rank(None));
    }

    #[test]
    fn cycle_sort_mode_reorders_visible_workspaces_by_role() {
        // Build a sidebar with one workspace per role under the same
        // repo. Start by flipping to Recent mode (default is now
        // ByRoleSplit) so we can exercise the recency baseline
        // before cycling to ByRole.
        let mut sb = Sidebar::new(PaneId::new(1));
        while sb.sort_mode() != SortMode::Recent {
            sb.cycle_sort_mode();
        }
        let now = chrono::Utc::now();
        for (offset_secs, key, role) in [
            (0, "1", TaskRole::Mentioned),
            (10, "2", TaskRole::Assignee),
            (20, "3", TaskRole::Reviewer),
            (30, "4", TaskRole::Author),
        ] {
            let mut t = base_task();
            t.id.key = key.into();
            t.url = format!("https://github.com/o/r/pull/{key}");
            t.role = role;
            t.updated_at = now - chrono::Duration::seconds(offset_secs);
            let w = Workspace::from_task(t, now);
            let sk = SessionKey::from(&w.key);
            sb.workspaces.insert(sk, w);
        }
        sb.recompute_visible();

        // Recent sort: Mentioned (newest updated_at) leads, Author
        // (oldest) trails.
        let order_default: Vec<&str> = sb
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        // SessionKey normalizes to `<source>-<n>` (e.g. `gh-1`); the
        // test fixtures here all build through the same `base_task`
        // path so the keys come out as `gh-1`..`gh-4`. Check
        // suffixes — the source prefix is incidental.
        assert!(
            order_default.first().unwrap().ends_with("-1"),
            "default sort: most recent (key 1, Mentioned) leads — got {:?}",
            order_default
        );

        // Cycle to ByRole. Author (key 4) should now lead even
        // though it's the oldest by updated_at.
        sb.cycle_sort_mode();
        assert_eq!(sb.sort_mode(), SortMode::ByRole);
        let order_by_role: Vec<&str> = sb
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            order_by_role.first().unwrap().ends_with("-4"),
            "ByRole: Author (key 4) leads — got {:?}",
            order_by_role
        );
        // And Reviewer should be second.
        assert!(
            order_by_role[1].ends_with("-3"),
            "ByRole: Reviewer (key 3) second — got {:?}",
            order_by_role
        );
    }

    #[test]
    fn by_role_split_injects_kind_headers_between_pr_and_issue_groups() {
        // One PR and one issue under the same repo. ByRoleSplit must
        // emit a `KindHeader(Pr)` then a `KindHeader(Issue)` so the
        // two sections are visually distinct (issue #37).
        use crate::components::sidebar::WorkspaceKind;
        let mut sb = Sidebar::new(PaneId::new(1));
        let now = chrono::Utc::now();

        // PR workspace.
        let mut pr_task = base_task();
        pr_task.id.key = "1".into();
        pr_task.url = "https://github.com/o/r/pull/1".into();
        let pr_ws = Workspace::from_task(pr_task, now);
        sb.workspaces.insert(SessionKey::from(&pr_ws.key), pr_ws);

        // Issue workspace — task with `url` pointing at /issues/
        // routes through `Workspace::from_task` into the `gh_issues`
        // slot via `classify`, leaving `pr = None`.
        let mut issue_task = base_task();
        issue_task.id.key = "2".into();
        issue_task.url = "https://github.com/o/r/issues/2".into();
        let issue_ws = Workspace::from_task(issue_task, now);
        sb.workspaces
            .insert(SessionKey::from(&issue_ws.key), issue_ws);

        // Default is already ByRoleSplit — just recompute the
        // visible list.
        sb.recompute_visible();
        assert_eq!(sb.sort_mode(), SortMode::ByRoleSplit);

        let headers: Vec<&VisibleRow> = sb
            .visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::KindHeader(_)))
            .collect();
        assert_eq!(
            headers.len(),
            2,
            "one kind header per distinct kind — got {:?}",
            sb.visible
        );
        // PRs section comes before Issues so the eye lands on
        // actionable review work first.
        assert!(matches!(
            headers[0],
            VisibleRow::KindHeader(WorkspaceKind::Pr)
        ));
        assert!(matches!(
            headers[1],
            VisibleRow::KindHeader(WorkspaceKind::Issue)
        ));
    }

    #[test]
    fn classify_buckets_empty_workspace_as_other_not_issue() {
        // A sandbox/scratch workspace has no PR and no issues. It must
        // NOT fall through to the `Issue` bucket — it has no type, so
        // it belongs in `Other` (issue #195).
        use crate::components::sidebar::WorkspaceKind;
        let now = chrono::Utc::now();
        let empty = Workspace::empty(lazybox_core::WorkspaceKey::new("research"), "main", now);
        assert_eq!(WorkspaceKind::classify(&empty), WorkspaceKind::Other);
        assert_eq!(WorkspaceKind::Other.header_label(), "Other");

        let pr = Workspace::from_task(
            {
                let mut t = base_task();
                t.url = "https://github.com/o/r/pull/1".into();
                t
            },
            now,
        );
        assert_eq!(WorkspaceKind::classify(&pr), WorkspaceKind::Pr);

        let issue = Workspace::from_task(
            {
                let mut t = base_task();
                t.url = "https://github.com/o/r/issues/2".into();
                t
            },
            now,
        );
        assert_eq!(WorkspaceKind::classify(&issue), WorkspaceKind::Issue);
    }

    #[test]
    fn by_role_split_puts_empty_workspace_under_other_header() {
        // Regression for issue #195: a sandbox workspace (no PR, no
        // issues) used to render under the `Issues` header with the
        // `I` glyph. It must get its own `Other` section instead.
        use crate::components::sidebar::WorkspaceKind;
        let mut sb = Sidebar::new(PaneId::new(1));
        let now = chrono::Utc::now();

        let empty_ws = Workspace::empty(lazybox_core::WorkspaceKey::new("research"), "main", now);
        let key = SessionKey::from(&empty_ws.key);
        sb.workspaces.insert(key.clone(), empty_ws);

        sb.recompute_visible();
        assert_eq!(sb.sort_mode(), SortMode::ByRoleSplit);

        let headers: Vec<WorkspaceKind> = sb
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::KindHeader(k) => Some(*k),
                _ => None,
            })
            .collect();
        assert!(
            headers.contains(&WorkspaceKind::Other),
            "empty workspace must get an Other header — got {:?}",
            sb.visible
        );
        assert!(
            !headers.contains(&WorkspaceKind::Issue),
            "empty workspace must never be bucketed as an Issue — got {:?}",
            sb.visible
        );
    }

    #[test]
    fn kind_headers_only_appear_in_split_mode() {
        // PR + issue fixture, but in Recent + ByRole modes the kind
        // headers must NOT appear — they're a ByRoleSplit-only
        // affordance, so toggling between `[recent]`/`[by-role]` and
        // `[split]` produces a visibly different layout (issue #37).
        let mut sb = Sidebar::new(PaneId::new(1));
        let now = chrono::Utc::now();

        let mut pr_task = base_task();
        pr_task.id.key = "1".into();
        pr_task.url = "https://github.com/o/r/pull/1".into();
        let pr_ws = Workspace::from_task(pr_task, now);
        sb.workspaces.insert(SessionKey::from(&pr_ws.key), pr_ws);

        let mut issue_task = base_task();
        issue_task.id.key = "2".into();
        issue_task.url = "https://github.com/o/r/issues/2".into();
        let issue_ws = Workspace::from_task(issue_task, now);
        sb.workspaces
            .insert(SessionKey::from(&issue_ws.key), issue_ws);

        for mode in [SortMode::Recent, SortMode::ByRole] {
            // Reset to Recent then cycle to the target.
            while sb.sort_mode() != SortMode::Recent {
                sb.cycle_sort_mode();
            }
            while sb.sort_mode() != mode {
                sb.cycle_sort_mode();
            }
            let has_kind_header = sb
                .visible
                .iter()
                .any(|r| matches!(r, VisibleRow::KindHeader(_)));
            assert!(
                !has_kind_header,
                "KindHeader leaked into {:?} mode — got {:?}",
                mode, sb.visible
            );
        }
    }

    #[test]
    fn cursor_skips_kind_headers_with_j_navigation() {
        // After ByRoleSplit cycle, cursor parks on kind headers like
        // any other header but `selected_session_key` returns None.
        let mut sb = Sidebar::new(PaneId::new(1));
        let now = chrono::Utc::now();

        let mut pr_task = base_task();
        pr_task.id.key = "1".into();
        pr_task.url = "https://github.com/o/r/pull/1".into();
        let pr_ws = Workspace::from_task(pr_task, now);
        sb.workspaces.insert(SessionKey::from(&pr_ws.key), pr_ws);

        let mut issue_task = base_task();
        issue_task.id.key = "2".into();
        issue_task.url = "https://github.com/o/r/issues/2".into();
        let issue_ws = Workspace::from_task(issue_task, now);
        sb.workspaces
            .insert(SessionKey::from(&issue_ws.key), issue_ws);
        sb.recompute_visible();
        assert_eq!(sb.sort_mode(), SortMode::ByRoleSplit);

        // Contract: every KindHeader row resolves to `None` from
        // `selected_session_key` — same skip-the-header semantics
        // RepoHeader already provides.
        let visible_snapshot: Vec<VisibleRow> = sb.visible.to_vec();
        let mut saw_kind_header = false;
        for (idx, row) in visible_snapshot.iter().enumerate() {
            if matches!(row, VisibleRow::KindHeader(_)) {
                saw_kind_header = true;
                sb.cursor = idx;
                assert!(
                    sb.selected_session_key().is_none(),
                    "KindHeader cursor must not resolve to a session key (row {idx})"
                );
            }
        }
        assert!(
            saw_kind_header,
            "fixture should have produced at least one KindHeader — got {:?}",
            visible_snapshot
        );
    }

    #[test]
    fn sort_chip_label_short_enough() {
        for m in [SortMode::Recent, SortMode::ByRole, SortMode::ByRoleSplit] {
            assert!(
                m.chip_label().chars().count() <= 10,
                "sort chip `{}` exceeds 10 cells",
                m.chip_label()
            );
        }
    }

    #[test]
    fn filter_labels_are_short_enough_for_a_header_chip() {
        // Each filter label may render as a chip in row 1 of the
        // header. Cap each at 16 cells so a single active chip never
        // overflows the typical 30-column sidebar.
        for f in Filter::ALL {
            assert!(
                f.label().chars().count() <= 16,
                "filter label `{}` exceeds 16 cells",
                f.label()
            );
        }
    }
}

#[cfg(test)]
mod search_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lazybox_core::Workspace;

    fn issue_ws(key: &str, title: &str) -> Workspace {
        let mut t = base_task();
        t.id.key = key.into();
        t.title = title.into();
        // `/issues/` URL routes through `classify` into the gh_issues
        // slot so the workspace is a plain issue (no PR).
        t.url = format!("https://github.com/o/r/issues/{key}");
        let mut w = Workspace::from_task(t, chrono::Utc::now());
        w.name = title.into();
        w
    }

    fn sidebar_with_issues(items: &[(&str, &str)]) -> Sidebar {
        let mut sb = Sidebar::new(PaneId::new(1));
        for (key, title) in items {
            let w = issue_ws(key, title);
            sb.workspaces.insert(SessionKey::from(&w.key), w);
        }
        sb.recompute_visible();
        sb
    }

    /// Like [`issue_ws`] but in a caller-chosen repo, so a test can
    /// spread workspaces across multiple repo groups.
    fn issue_ws_in_repo(repo: &str, num: &str, title: &str) -> Workspace {
        let mut t = base_task();
        t.id.key = format!("{repo}#{num}");
        // `group_label` keys on `task.repo`, so set it too — otherwise every
        // workspace inherits base_task's fixed repo and collapses into one
        // group regardless of the `repo` argument.
        t.repo = Some(repo.to_string());
        t.title = title.into();
        t.url = format!("https://github.com/{repo}/issues/{num}");
        let mut w = Workspace::from_task(t, chrono::Utc::now());
        w.name = title.into();
        w
    }

    /// Frame-budget regression gate (#1090, acceptance #4): the sidebar's
    /// per-frame widget build must stay cheap at scale.
    /// `prebuild_workspace_lines` rebuilds every visible row every frame
    /// (audit R-3c, unmemoized); a change that makes a row expensive — the
    /// #1059-class "compute in the render path" regression — is caught here
    /// rather than in the field.
    ///
    /// Timing is only meaningful in an optimized build (debug carries a
    /// ~25× penalty), so the assertion is `not(debug_assertions)`-gated and
    /// the whole test is `#[ignore]`d under debug: CI runs it with
    /// `cargo test --release -p lazybox-tui sidebar_build_budget`. Under a
    /// normal debug `cargo test` it is skipped, so it never slows that run.
    #[cfg_attr(debug_assertions, ignore)]
    #[test]
    fn sidebar_build_budget_at_scale() {
        use ratatui::backend::TestBackend;

        let mut sb = Sidebar::new(PaneId::new(1));
        // 300 workspaces across 30 repos — a heavy but realistic inbox.
        for repo in 0..30 {
            for num in 0..10 {
                let w = issue_ws_in_repo(
                    &format!("owner/repo-{repo:02}"),
                    &format!("{num}"),
                    &format!("Issue {repo}-{num}: a fairly typical title of moderate length"),
                );
                sb.workspaces.insert(SessionKey::from(&w.key), w);
            }
        }
        sb.recompute_visible();
        let visible = sb.visible.len();

        let backend = TestBackend::new(48, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        // Warm once (first render primes any lazy state).
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .unwrap();

        let iters = 200u32;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            terminal
                .draw(|frame| sb.render(frame.area(), frame, true))
                .unwrap();
        }
        let per = start.elapsed() / iters;
        println!(
            "sidebar build: {visible} visible rows, {:?}/frame ({:.2}ms)",
            per,
            per.as_secs_f64() * 1000.0
        );

        // Release baseline is ~2.85ms for ~300 rows; gate at 20ms leaves
        // ~7× headroom (no flakiness on a loaded CI box) while still
        // catching an order-of-magnitude regression. Assertion only in an
        // optimized build — debug timings are not representative.
        #[cfg(not(debug_assertions))]
        {
            let budget = std::time::Duration::from_millis(20);
            assert!(
                per < budget,
                "sidebar per-frame build {per:?} exceeded the {budget:?} budget \
                 for {visible} rows — a row got expensive to build (regression \
                 of the #1090 class). Profile prebuild_workspace_lines."
            );
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn type_query(sb: &mut Sidebar, q: &str) {
        for c in q.chars() {
            sb.handle_search_key(key(c));
        }
    }

    /// Opening search scopes the bar to the focused project, in
    /// editing mode. The `/` key→action binding lives in the catalog
    /// now (issue #98); this covers the `open_search` behaviour it
    /// dispatches to.
    #[test]
    fn open_search_scoped_to_focused_project() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_search();
        assert!(sb.search_editing());
        let s = sb.search().expect("search state present");
        assert_eq!(s.scope.as_deref(), Some("o/r"));
        assert!(s.query.is_empty());
    }

    /// The global search opens with no scope (searches every repo) and
    /// needs no project under the cursor.
    #[test]
    fn open_global_search_is_unscoped() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_global_search();
        assert!(sb.search_editing());
        let s = sb.search().expect("search state present");
        assert_eq!(s.scope, None);
        assert!(s.query.is_empty());
    }

    /// A global query filters rows across every repo group, unlike the
    /// project-scoped `/` search.
    #[test]
    fn global_search_filters_across_repos() {
        let mut sb = Sidebar::new(PaneId::new(1));
        for (repo, num, title) in [
            ("o/a", "1", "Add search"),
            ("o/a", "2", "Unrelated"),
            ("o/b", "3", "Search here too"),
        ] {
            let w = issue_ws_in_repo(repo, num, title);
            sb.workspaces.insert(SessionKey::from(&w.key), w);
        }
        sb.recompute_visible();
        assert_eq!(sb.workspace_count(), 3);
        sb.open_global_search();
        type_query(&mut sb, "search");
        assert_eq!(sb.workspace_count(), 2, "matches in both repos survive");
    }

    /// The header renders an always-visible `# find` hint; a click on
    /// its rect is reported by `search_chip_hit` so the orchestrator
    /// can open the global search.
    #[test]
    fn header_renders_search_box_and_reports_clicks() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        // Row 1 carries the filter / sort / search chips.
        let header: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect();
        assert!(header.contains("# find"), "{header:?}");

        // A click on the box's rect hits; a click well off it misses.
        let rect = sb.search_chip_rect.expect("search chip rect recorded");
        assert!(sb.search_chip_hit(rect.x, rect.y));
        assert!(!sb.search_chip_hit(rect.x, rect.y + 1));
        assert!(!sb.search_chip_hit(0, rect.y));
    }

    /// Spawn a live agent terminal so its provider counts as "in use"
    /// and the always-visible usage summary renders for it (#1059).
    fn spawn_agent(sb: &mut Sidebar, terminal_id: u64, session_key: &SessionKey, agent: &str) {
        sb.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(terminal_id),
            session_key: session_key.clone(),
            kind: TerminalKind::Agent(agent.into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
    }

    fn agent_usage(input: u64, output: u64) -> lazybox_ipc::AgentUsage {
        lazybox_ipc::AgentUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cost_usd_micros: None,
        }
    }

    /// One structured-run turn's worth of usage: bind the run, report the
    /// turn's usage, and commit it (the total only moves on turn commit).
    fn run_turn(sb: &mut Sidebar, run_id: u64, agent: &str, input: u64, output: u64) {
        let run = lazybox_ipc::AgentRunId(run_id);
        sb.note_agent_run(run, agent);
        sb.add_agent_usage(run, &agent_usage(input, output));
        sb.commit_agent_turn(run);
    }

    fn usage_row(sb: &mut Sidebar) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        // The usage summary sits at row 3, just above the divider.
        (0..buffer.area.width)
            .map(|x| buffer[(x, 3)].symbol())
            .collect()
    }

    /// A live agent with a configured budget renders the full bar +
    /// percentage widget, built from the accumulated token usage — the
    /// proactive "how much is left" display, visible before any limit.
    #[test]
    fn header_renders_per_provider_usage_summary() {
        let session_key = SessionKey::from("gh:owner/repo#1");
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        spawn_agent(&mut sb, 1, &session_key, "claude");
        sb.set_usage_budgets([("claude".to_string(), 200_000u64)].into_iter().collect());
        run_turn(&mut sb, 1, "claude", 100_000, 24_000);

        let row = usage_row(&mut sb);
        assert!(row.contains("Claude"), "{row:?}");
        assert!(row.contains("62%"), "{row:?}");
        assert!(row.contains('▓') && row.contains('░'), "{row:?}");
    }

    /// A structured run reports usage twice per turn (a streaming
    /// `message_delta` then the `result` total); the header must count the
    /// turn once, not double it.
    #[test]
    fn usage_summary_counts_a_turn_once_not_per_report() {
        let session_key = SessionKey::from("gh:owner/repo#1");
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        spawn_agent(&mut sb, 1, &session_key, "claude");
        let run = lazybox_ipc::AgentRunId(1);
        sb.note_agent_run(run, "claude");
        sb.add_agent_usage(run, &agent_usage(0, 24_000)); // message_delta
        sb.add_agent_usage(run, &agent_usage(100_000, 24_000)); // result total
        sb.commit_agent_turn(run);
        // 124k, not 148k (delta+result).
        assert!(
            usage_row(&mut sb).contains("124k"),
            "{:?}",
            usage_row(&mut sb)
        );
    }

    /// Usage from a structured run surfaces even with no interactive
    /// terminal for that agent — usage events come only from structured
    /// runs, which spawn no terminal, so gating on live terminals alone
    /// would hide every real total.
    #[test]
    fn usage_summary_shows_a_structured_run_with_no_terminal() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        // No spawn_agent — a headless run only.
        run_turn(&mut sb, 1, "claude", 60_000, 8_000);
        assert!(
            usage_row(&mut sb).contains("Claude"),
            "{:?}",
            usage_row(&mut sb)
        );
        assert!(
            usage_row(&mut sb).contains("68k"),
            "{:?}",
            usage_row(&mut sb)
        );
    }

    /// A live agent terminal with no configured budget and no accumulated
    /// usage must not manufacture a row. Before #1109 the display set
    /// included every live terminal regardless of whether a real figure
    /// existed, so a freshly-spawned interactive Claude (which emits no
    /// structured usage) rendered a meaningless "Claude 0 used". Now such
    /// a terminal contributes nothing until it has a budget or real usage.
    #[test]
    fn usage_summary_omits_a_live_terminal_with_no_budget_and_no_usage() {
        let session_key = SessionKey::from("gh:owner/repo#1");
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        spawn_agent(&mut sb, 1, &session_key, "claude");

        let area = Rect::new(0, 0, 60, 14);
        assert_eq!(sb.usage_row_height(area), 0);
        let row = usage_row(&mut sb);
        assert!(!row.contains("used"), "{row:?}");
        assert!(!row.contains("Claude"), "{row:?}");
    }

    /// A live agent terminal with a configured budget but no usage yet
    /// shows the proactive quota bar at 0% — the budget is what makes the
    /// row a real quota display rather than a bare token count (#1109).
    #[test]
    fn usage_summary_shows_a_budgeted_live_terminal_at_zero_percent() {
        let session_key = SessionKey::from("gh:owner/repo#1");
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        spawn_agent(&mut sb, 1, &session_key, "claude");
        sb.set_usage_budgets([("claude".to_string(), 200_000u64)].into_iter().collect());

        let row = usage_row(&mut sb);
        assert!(row.contains("Claude") && row.contains("0%"), "{row:?}");
        assert!(!row.contains("used"), "{row:?}");
    }

    /// A quota-only agent (no terminal, no budget, no committed usage) still
    /// surfaces its "can I keep working?" headroom while its window is live —
    /// the quota fragment is the row's whole reason to exist.
    #[test]
    fn usage_summary_shows_a_quota_only_agent_with_a_live_window() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        // Reset far in the future so the window is unambiguously current.
        sb.note_provider_quota(
            "codex",
            lazybox_ipc::ProviderQuota {
                five_hour: Some(lazybox_ipc::QuotaWindow {
                    utilization_bp: 4500,
                    reset_at: Some(9_999_999_999),
                }),
                weekly: None,
            },
        );
        let row = usage_row(&mut sb);
        assert!(row.contains("Codex"), "{row:?}");
        assert!(row.contains("45%"), "{row:?}");
    }

    /// Once a quota-only agent's every window has passed its reset, the
    /// utilization is stale (the provider rolled the window over) and there
    /// is nothing left to show — the row must disappear rather than render a
    /// bare "Codex 0 used" carrying a pre-reset figure that no longer holds.
    #[test]
    fn usage_summary_drops_a_quota_only_agent_after_its_windows_reset() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        // Both resets in 1970 — unambiguously in the past for any real clock.
        sb.note_provider_quota(
            "codex",
            lazybox_ipc::ProviderQuota {
                five_hour: Some(lazybox_ipc::QuotaWindow {
                    utilization_bp: 9000,
                    reset_at: Some(0),
                }),
                weekly: Some(lazybox_ipc::QuotaWindow {
                    utilization_bp: 6000,
                    reset_at: Some(1),
                }),
            },
        );
        let area = Rect::new(0, 0, 60, 14);
        assert_eq!(sb.usage_row_height(area), 0);
        let row = usage_row(&mut sb);
        assert!(!row.contains("Codex"), "{row:?}");
        assert!(!row.contains("used"), "{row:?}");
    }

    /// Without a budget the widget degrades to a bare token total ("show
    /// what's known"), and the reset hint is folded in only while the
    /// agent is actually limited.
    #[test]
    fn usage_summary_degrades_without_a_budget_and_folds_in_the_reset() {
        let session_key = SessionKey::from("gh:owner/repo#1");
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        spawn_agent(&mut sb, 1, &session_key, "claude");
        run_turn(&mut sb, 1, "claude", 120_000, 8_000);

        // No budget → token total, no percentage, no reset yet.
        let row = usage_row(&mut sb);
        assert!(row.contains("Claude") && row.contains("128k"), "{row:?}");
        assert!(!row.contains('%'), "{row:?}");
        assert!(!row.contains("resets"), "{row:?}");

        // The reset hint alone does not surface it — only a live limit does.
        sb.note_usage_limit_reset(TerminalId(1), "3pm".into());
        assert!(!usage_row(&mut sb).contains("resets"));
        sb.on_event(&Event::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(1),
            state: lazybox_ipc::AgentState::LimitReached,
        });
        assert!(usage_row(&mut sb).contains("resets 3pm"));
    }

    /// A reset hint clears once the agent recovers, so a later limit
    /// episode whose banner has no parseable countdown can't resurface the
    /// stale time.
    #[test]
    fn usage_reset_clears_when_the_agent_recovers() {
        let session_key = SessionKey::from("gh:owner/repo#1");
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        spawn_agent(&mut sb, 1, &session_key, "claude");
        run_turn(&mut sb, 1, "claude", 1_000, 0);

        let limit = |sb: &mut Sidebar, state: lazybox_ipc::AgentState| {
            sb.on_event(&Event::AgentState {
                session_key: session_key.clone(),
                terminal_id: TerminalId(1),
                state,
            });
        };

        sb.note_usage_limit_reset(TerminalId(1), "3pm".into());
        limit(&mut sb, lazybox_ipc::AgentState::LimitReached);
        assert!(usage_row(&mut sb).contains("resets 3pm"));

        // Recover — the stale hint is dropped.
        limit(&mut sb, lazybox_ipc::AgentState::Working);
        // A fresh limit with no new parseable countdown must not re-show it.
        limit(&mut sb, lazybox_ipc::AgentState::LimitReached);
        assert!(
            !usage_row(&mut sb).contains("resets"),
            "{:?}",
            usage_row(&mut sb)
        );
    }

    /// `ui.usage_summary = false` hides the row entirely and reclaims its
    /// line — content shifts back up, and the click hit-test agrees.
    #[test]
    fn usage_summary_can_be_disabled() {
        let session_key = SessionKey::from("gh:owner/repo#1");
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        spawn_agent(&mut sb, 1, &session_key, "claude");
        run_turn(&mut sb, 1, "claude", 10_000, 0);

        let area = Rect::new(0, 0, 60, 14);
        assert_eq!(sb.usage_row_height(area), 1);
        assert!(usage_row(&mut sb).contains("Claude"));

        sb.set_usage_summary(false);
        assert_eq!(sb.usage_row_height(area), 0);
        assert!(!usage_row(&mut sb).contains("Claude"));
    }

    /// The bottom `/` search bar records its rect so a click on the
    /// input itself is distinguishable from a click that should dismiss
    /// the search (#780).
    #[test]
    fn search_bar_records_its_rect_for_hit_testing() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        assert!(sb.search_bar_rect.is_none(), "no bar before a search opens");
        sb.open_search();
        type_query(&mut sb, "al");
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");

        let rect = sb.search_bar_rect.expect("search bar rect recorded");
        assert!(sb.search_bar_hit(rect.x, rect.y), "click on the bar hits");
        assert!(!sb.search_bar_hit(rect.x, rect.y.saturating_sub(1)));

        // Closing the search drops the recorded rect on the next render.
        sb.handle_search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        assert!(
            sb.search_bar_rect.is_none(),
            "rect cleared once the bar is gone"
        );
    }

    /// Read the bottom `/` search bar row as a string at a given pane
    /// width (height fixed at 12 so the bar always draws).
    fn search_bar_row_at(sb: &mut Sidebar, width: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let bar = sb.search_bar_rect.expect("search bar rect recorded");
        (0..buffer.area.width)
            .map(|x| buffer[(x, bar.y)].symbol())
            .collect()
    }

    fn search_bar_row(sb: &mut Sidebar) -> String {
        search_bar_row_at(sb, 60)
    }

    /// Render the whole sidebar to a newline-joined string of cell
    /// symbols, for asserting on content-area panels.
    fn full_screen(sb: &mut Sidebar, width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// While the bar is capturing keystrokes it reads as an unmistakable
    /// field: the `🔍` glyph, the vim `/` prefix, the typed query, and a
    /// solid block cursor (#1099).
    #[test]
    fn editing_search_bar_is_a_prominent_field_with_a_block_cursor() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_search();
        type_query(&mut sb, "al");
        let bar = search_bar_row(&mut sb);
        assert!(bar.contains('🔍'), "search glyph present: {bar:?}");
        assert!(bar.contains('█'), "block cursor while editing: {bar:?}");
        assert!(bar.contains("al"), "shows the typed query: {bar:?}");
    }

    /// A search that filters every workspace away shows an explicit
    /// empty-state panel — naming the query and the Esc exit — instead of
    /// a blank pane that reads as "everything vanished / broke" (#1099).
    #[test]
    fn empty_search_result_shows_a_no_matches_panel() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_global_search();
        type_query(&mut sb, "zzzqqq");
        assert_eq!(sb.workspace_count(), 0, "the query matches nothing");
        let screen = full_screen(&mut sb, 46, 16);
        assert!(
            screen.contains("No matches"),
            "explicit empty state: {screen:?}"
        );
        assert!(
            screen.contains("Esc to clear"),
            "names the exit: {screen:?}"
        );
    }

    /// The highlight set tracks the search's *scope*, not raw title text:
    /// under a scoped `/` search, an out-of-scope row whose title happens
    /// to contain the query is left visible but NOT highlighted, exactly as
    /// the filter leaves it untouched. Guards the filter/highlight from
    /// drifting apart (#1099) — both read `search_scope_covers`.
    #[test]
    fn scoped_highlight_set_tracks_filter_scope_not_title_text() {
        // Distinct repo groups (`o/a`, `o/b`) so the scoped search pins to
        // one and the other stays out of scope.
        let a = issue_ws_in_repo("o/a", "1", "Add search bar");
        let b = issue_ws_in_repo("o/b", "2", "search everywhere");
        let a_key = SessionKey::from(&a.key);
        let b_key = SessionKey::from(&b.key);
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.workspaces.insert(a_key.clone(), a);
        sb.workspaces.insert(b_key.clone(), b);
        sb.recompute_visible();
        // Cursor lands on the first (alphabetically `o/a`) workspace, so the
        // scoped `/` search pins to `o/a`.
        sb.open_search();
        type_query(&mut sb, "search");
        assert!(
            sb.searched_keys.contains(&a_key),
            "the in-scope match is highlighted",
        );
        assert!(
            !sb.searched_keys.contains(&b_key),
            "an out-of-scope row is not highlighted even though its title contains the term",
        );
        assert_eq!(
            sb.workspace_count(),
            2,
            "the out-of-scope row stays visible (scoped search leaves other repos untouched)",
        );
    }

    /// A matching row underlines the searched substring in its title so
    /// the user can see *what* matched — the vim `/pattern` cue (#1099).
    #[test]
    fn matching_rows_underline_the_searched_substring() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut sb = sidebar_with_issues(&[("1", "Add Search bar")]);
        sb.open_search();
        type_query(&mut sb, "Search");
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let underlined = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                cell.modifier.contains(ratatui::style::Modifier::UNDERLINED)
                    && "Search".contains(cell.symbol())
                    && !cell.symbol().trim().is_empty()
            })
        });
        assert!(underlined, "the matched substring is underlined in the row");
    }

    /// A scoped `/` search names the project it's pinned to and points
    /// at `#` for the wider reach, so its scope is never invisible
    /// (#1033).
    #[test]
    fn scoped_search_bar_names_its_project_and_points_to_global() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_search();
        type_query(&mut sb, "al");
        let bar = search_bar_row(&mut sb);
        assert!(bar.contains("o/r"), "scope named: {bar:?}");
        assert!(bar.contains('#'), "points to global search: {bar:?}");
    }

    /// A scoped `/` search that matches nothing surfaces the `#`
    /// escape hatch instead of reading as broken (#1033).
    #[test]
    fn scoped_search_empty_result_suggests_global() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_search();
        type_query(&mut sb, "zzzqqq");
        let bar = search_bar_row(&mut sb);
        assert!(bar.contains("no matches"), "{bar:?}");
        assert!(bar.contains("# all repos"), "suggests widening: {bar:?}");
    }

    /// The `#` pointer is the actionable cue, so it leads the hint and
    /// survives a narrow pane while the (expendable) scope name is the
    /// first thing to clip — the failure the header search box already
    /// guards against, now guarded for the bottom bar too (#1033).
    #[test]
    fn scoped_search_hint_keeps_the_global_pointer_when_the_scope_name_clips() {
        let repo = "averylongowner/averylongreponame";
        let w = issue_ws_in_repo(repo, "1", "Alpha");
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.workspaces.insert(SessionKey::from(&w.key), w);
        sb.recompute_visible();
        sb.open_search();
        type_query(&mut sb, "al");
        let bar = search_bar_row_at(&mut sb, 34);
        assert!(
            bar.contains("# all repos"),
            "actionable pointer survives the clip: {bar:?}"
        );
        assert!(
            !bar.contains(repo),
            "the expendable scope name clips first: {bar:?}"
        );
    }

    /// A committed (non-editing) search still advertises `esc clear`,
    /// because Esc now clears it — the hint no longer lies (#1033).
    #[test]
    fn committed_scoped_search_hint_promises_esc_clear() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_search();
        type_query(&mut sb, "al");
        sb.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!sb.search_editing(), "Enter commits the search");
        let bar = search_bar_row(&mut sb);
        assert!(
            bar.contains("esc clear"),
            "esc-clear cue is honest now: {bar:?}"
        );
    }

    /// A bare Esc clears a committed search filter (the pane-handler
    /// path, distinct from the editing-time Esc). Without it a committed
    /// search trapped the user in a narrowed tree (#1033).
    #[test]
    fn esc_clears_a_committed_search_via_the_pane_handler() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        sb.open_search();
        type_query(&mut sb, "search");
        sb.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!sb.search_editing(), "Enter commits");
        assert_eq!(sb.workspace_count(), 1, "filter still applied");

        let mut cmds = Vec::new();
        let outcome = sb.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut cmds);
        assert!(matches!(outcome, PaneOutcome::Consumed), "Esc is consumed");
        assert!(sb.search().is_none(), "committed search cleared");
        assert_eq!(sb.workspace_count(), 2, "full tree restored");
    }

    /// While a global query is applied the header box shows the query
    /// rather than the `find` placeholder.
    #[test]
    fn header_search_box_shows_the_active_query() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_global_search();
        type_query(&mut sb, "720");
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let header: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect();
        assert!(header.contains("720"), "{header:?}");
        assert!(!header.contains("find"), "{header:?}");
    }

    /// A query too long for the header box is truncated to exactly the
    /// room the `⌕ …` frame leaves — the chip stays inside the pane,
    /// ends with an ellipsis, and drops precisely the overflowing
    /// characters (pins the measured frame width, not a guessed one).
    #[test]
    fn header_search_box_truncates_a_long_query_to_fit() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_global_search();
        type_query(&mut sb, "abcdefghijklmnopqrst");
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let header: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect();

        // With `f filter  o split  # ` (21 cells) ahead of it and a
        // 38-cell inner width, 17 cells remain — a 2-cell `⌕ ` frame
        // leaves 15 for the query, so 14 chars survive before the `…`.
        assert!(
            header.contains("abcdefghijklmn…"),
            "truncated to room: {header:?}"
        );
        assert!(
            !header.contains("abcdefghijklmno"),
            "15th char dropped: {header:?}"
        );
        let rect = sb.search_chip_rect.expect("box still renders");
        assert!(
            rect.x + rect.width <= buffer.area.width,
            "chip stays inside the pane: {rect:?}"
        );
    }

    /// Too narrow to fit even the frame → the box is dropped cleanly:
    /// no rect recorded, no panic, and the pane still renders.
    #[test]
    fn header_search_box_dropped_when_pane_too_narrow() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_global_search();
        type_query(&mut sb, "anything");
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        assert!(
            sb.search_chip_rect.is_none(),
            "box dropped when it can't fit"
        );
        assert!(!sb.search_chip_hit(0, 1), "no phantom hit zone");
    }

    /// Typing filters the project's rows live; non-matches drop out.
    #[test]
    fn typing_filters_visible_rows() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        assert_eq!(sb.workspace_count(), 2);
        sb.open_search();
        type_query(&mut sb, "search");
        assert_eq!(sb.workspace_count(), 1, "only the matching row survives");
    }

    /// `Enter` keeps the filter applied but stops capturing keys.
    #[test]
    fn enter_keeps_filter_and_exits_editing() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        sb.open_search();
        type_query(&mut sb, "search");
        sb.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!sb.search_editing(), "Enter exits editing mode");
        assert!(sb.search().is_some(), "filter stays applied");
        assert_eq!(sb.workspace_count(), 1);
    }

    /// `Esc` clears the query and restores the full tree.
    #[test]
    fn esc_clears_search_and_restores_tree() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        sb.open_search();
        type_query(&mut sb, "search");
        assert_eq!(sb.workspace_count(), 1);
        sb.handle_search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(sb.search().is_none());
        assert_eq!(sb.workspace_count(), 2, "full tree restored");
    }

    /// A click outside the input dismisses a non-empty search the same
    /// way `Enter` does — the filter stays applied but editing stops, so
    /// keystrokes reach the pane the click focused instead of being
    /// trapped in "find land" (#780).
    #[test]
    fn dismiss_search_commits_a_nonempty_query() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        sb.open_search();
        type_query(&mut sb, "search");
        assert!(sb.search_editing());
        sb.dismiss_search();
        assert!(!sb.search_editing(), "click-outside exits editing mode");
        assert!(sb.search().is_some(), "the filter stays applied");
        assert_eq!(sb.workspace_count(), 1);
    }

    /// An empty search has nothing to keep, so a click outside closes
    /// the bar outright and restores the full tree (#780).
    #[test]
    fn dismiss_search_closes_an_empty_query() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        sb.open_search();
        assert!(sb.search_editing());
        sb.dismiss_search();
        assert!(sb.search().is_none(), "an empty search closes outright");
        assert_eq!(sb.workspace_count(), 2, "full tree restored");
    }

    /// `dismiss_search` never re-opens or re-edits a committed filter,
    /// and is a no-op when no search is open (#780).
    #[test]
    fn dismiss_search_is_a_noop_without_active_editing() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        sb.dismiss_search();
        assert!(sb.search().is_none());
        sb.open_search();
        type_query(&mut sb, "search");
        sb.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!sb.search_editing());
        sb.dismiss_search();
        assert!(
            !sb.search_editing(),
            "an already-committed filter is untouched"
        );
        assert!(sb.search().is_some());
    }

    /// Backspace shrinks the query and re-widens the result set.
    #[test]
    fn backspace_widens_results() {
        let mut sb = sidebar_with_issues(&[("1", "Add search bar"), ("2", "Fix flaky test")]);
        sb.open_search();
        type_query(&mut sb, "searchx"); // matches nothing
        assert_eq!(sb.workspace_count(), 0);
        sb.handle_search_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(sb.workspace_count(), 1, "back to a matching prefix");
    }

    #[test]
    fn reveal_workspace_crosses_every_sidebar_visibility_boundary() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut workspace = issue_ws("674", "Notification focus");
        workspace.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::hours(1));
        let key = SessionKey::from(&workspace.key);
        sb.workspaces.insert(key.clone(), workspace);
        sb.filters.replace([Filter::CiFailing]);
        sb.search = Some(SearchState {
            scope: Some("o/r".into()),
            query: "does-not-match".into(),
            editing: false,
        });
        sb.collapsed_repos.insert("o/r".into());
        sb.recompute_visible();
        assert!(!sb.focus_workspace_key(&key));

        assert!(sb.reveal_workspace_key(&key));
        assert_eq!(sb.mailbox(), Mailbox::Snoozed);
        assert!(sb.filters().is_empty());
        assert!(sb.search().is_none());
        assert!(!sb.collapsed_repos.contains("o/r"));
        assert_eq!(sb.selected_session_key(), Some(&key));
    }

    #[test]
    fn reveal_workspace_preserves_a_view_that_already_contains_the_target() {
        let mut sb = sidebar_with_issues(&[("674", "Notification focus")]);
        let key = sb.selected_session_key().expect("selected").clone();
        sb.set_filters([Filter::Author]);
        sb.open_search();
        type_query(&mut sb, "notification");

        assert!(sb.reveal_workspace_key(&key));
        assert_eq!(
            sb.filters().iter().collect::<Vec<_>>(),
            vec![Filter::Author]
        );
        assert_eq!(
            sb.search().map(|search| search.query.as_str()),
            Some("notification")
        );
    }

    /// Enter with an empty query just closes the bar (nothing to keep).
    #[test]
    fn enter_on_empty_query_closes_bar() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_search();
        sb.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(sb.search().is_none());
    }

    /// While a recompute batch is open, an upsert defers the visible-list
    /// rebuild — but a by-key scan (`focus_workspace_key`, and its sibling
    /// `focus_project_header`) must self-heal that pending rebuild so it
    /// finds the row instead of missing it against a stale list (#1030).
    /// Without the self-heal, a `WorkspaceFocusRequested` / `ProjectUpserted`
    /// / merge-follow that lands in the same drain batch as the upsert
    /// silently fails to move the cursor.
    #[test]
    fn focus_workspace_key_flushes_a_pending_batched_recompute() {
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.begin_recompute_batch();
        let workspace = issue_ws("991", "Batched upsert");
        let key = SessionKey::from(&workspace.key);
        sb.on_event(&lazybox_ipc::Event::WorkspaceUpserted(std::sync::Arc::new(
            workspace,
        )));
        // The batch deferred the rebuild, so the list is still empty here…
        assert!(
            sb.visible_rows().is_empty(),
            "an open batch must defer the visible-list rebuild"
        );
        // …but the by-key scan self-heals it and lands on the row.
        assert!(
            sb.focus_workspace_key(&key),
            "focus must find a row upserted while a batch is open"
        );
    }
}

#[cfg(test)]
mod workspace_removal_cursor_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{SessionKind, Workspace, WorkspaceSession};
    use std::path::PathBuf;

    fn issue_ws(key: &str) -> Workspace {
        let mut task = base_task();
        task.id.key = key.into();
        task.title = key.into();
        task.url = format!("https://github.com/o/r/issues/{key}");
        Workspace::from_task(task, chrono::Utc::now())
    }

    fn sidebar_with_issues(count: usize) -> Sidebar {
        let mut sidebar = Sidebar::new(PaneId::new(1));
        for index in 0..count {
            let workspace = issue_ws(&index.to_string());
            sidebar
                .workspaces
                .insert(SessionKey::from(&workspace.key), workspace);
        }
        sidebar.recompute_visible();
        sidebar
    }

    /// #1090: the sidebar's per-row line build is the render hot spot, and a
    /// chatty agent triggers a flood of `TerminalOutput` redraws that repaint
    /// the whole frame without changing any sidebar state. `cached_workspace_lines`
    /// must skip the rebuild on those unchanged frames, and rebuild exactly once
    /// when a rendered input actually changes.
    #[test]
    fn unchanged_frames_reuse_the_cached_workspace_lines() {
        let mut sb = sidebar_with_issues(5);
        let now = chrono::Utc::now();
        let theme = crate::theme::current();

        // First frame builds.
        let _ = sb.cached_workspace_lines(40, theme, now, true);
        assert_eq!(sb.workspace_line_builds.get(), 1);

        // Repeated identical frames — the streaming-redraw path — must hit the
        // cache and never rebuild.
        for _ in 0..10 {
            let _ = sb.cached_workspace_lines(40, theme, now, true);
        }
        assert_eq!(
            sb.workspace_line_builds.get(),
            1,
            "unchanged frames must reuse the cache"
        );

        // A width change re-lays out → one rebuild, then holds.
        let _ = sb.cached_workspace_lines(30, theme, now, true);
        let _ = sb.cached_workspace_lines(30, theme, now, true);
        assert_eq!(sb.workspace_line_builds.get(), 2);

        // A focus change restyles rows → one rebuild.
        let _ = sb.cached_workspace_lines(30, theme, now, false);
        assert_eq!(sb.workspace_line_builds.get(), 3);

        // Workspace data changing (any daemon upsert recomputes) bumps
        // `data_version` → the cache invalidates.
        sb.recompute_visible();
        let _ = sb.cached_workspace_lines(30, theme, now, false);
        assert_eq!(
            sb.workspace_line_builds.get(),
            4,
            "a recompute (workspace-data change) must invalidate the cache"
        );
        let _ = sb.cached_workspace_lines(30, theme, now, false);
        assert_eq!(sb.workspace_line_builds.get(), 4);
    }

    fn visible_workspace_keys(sidebar: &Sidebar) -> Vec<SessionKey> {
        sidebar
            .visible_rows()
            .iter()
            .filter_map(|row| match row {
                VisibleRow::Workspace(key) => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    fn add_agent_session(sidebar: &mut Sidebar, key: &SessionKey) {
        let workspace = sidebar
            .workspaces
            .get_mut(key)
            .expect("workspace must exist");
        workspace.add_session(WorkspaceSession::new(
            workspace.key.clone(),
            SessionKind::Agent {
                agent_id: "codex".into(),
            },
            PathBuf::from("/tmp/agent-workspace"),
            chrono::Utc::now(),
        ));
        sidebar.recompute_visible();
    }

    #[test]
    fn optimistic_removal_prefers_the_next_agent_workspace_and_echo_keeps_focus() {
        let mut sidebar = sidebar_with_issues(4);
        let keys = visible_workspace_keys(&sidebar);
        let removed = keys[1].clone();
        let immediate_next = keys[2].clone();
        let next_agent = keys[3].clone();
        add_agent_session(&mut sidebar, &next_agent);
        assert!(sidebar.focus_workspace_key(&removed));

        let removed_workspace = sidebar
            .take_workspace(&removed)
            .expect("optimistic removal must return the workspace");

        assert_ne!(immediate_next, next_agent);
        assert_eq!(sidebar.selected_session_key(), Some(&next_agent));

        sidebar.on_event(&Event::WorkspaceRemoved(removed_workspace.key));
        assert_eq!(
            sidebar.selected_session_key(),
            Some(&next_agent),
            "the daemon echo must not move the cursor again",
        );
    }

    #[test]
    fn daemon_removal_falls_back_to_the_immediate_workspace_below() {
        let mut sidebar = sidebar_with_issues(4);
        let keys = visible_workspace_keys(&sidebar);
        let agent_above = keys[0].clone();
        let removed = keys[1].clone();
        let expected = keys[2].clone();
        add_agent_session(&mut sidebar, &agent_above);
        assert!(sidebar.focus_workspace_key(&removed));
        let workspace_key = sidebar
            .workspaces
            .get(&removed)
            .expect("workspace must exist")
            .key
            .clone();

        sidebar.on_event(&Event::WorkspaceRemoved(workspace_key));

        assert_eq!(sidebar.selected_session_key(), Some(&expected));
    }

    #[test]
    fn removing_the_bottom_workspace_clamps_to_the_new_bottom() {
        let mut sidebar = sidebar_with_issues(3);
        let keys = visible_workspace_keys(&sidebar);
        let removed = keys[2].clone();
        let expected = keys[1].clone();
        assert!(sidebar.focus_workspace_key(&removed));
        let workspace_key = sidebar
            .workspaces
            .get(&removed)
            .expect("workspace must exist")
            .key
            .clone();

        sidebar.on_event(&Event::WorkspaceRemoved(workspace_key));

        assert_eq!(sidebar.selected_session_key(), Some(&expected));
    }
}

#[cfg(test)]
mod broadcast_select_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lazybox_core::Workspace;

    fn issue_ws(key: &str, title: &str) -> Workspace {
        let mut t = base_task();
        t.id.key = key.into();
        t.title = title.into();
        t.url = format!("https://github.com/o/r/issues/{key}");
        let mut w = Workspace::from_task(t, chrono::Utc::now());
        w.name = title.into();
        w
    }

    fn sidebar_with_issues(items: &[(&str, &str)]) -> Sidebar {
        let mut sb = Sidebar::new(PaneId::new(1));
        for (key, title) in items {
            let w = issue_ws(key, title);
            sb.workspaces.insert(SessionKey::from(&w.key), w);
        }
        sb.recompute_visible();
        sb
    }

    #[test]
    fn focused_auto_fix_is_explained_in_the_sidebar_header() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        let key = sb.selected_session_key().expect("workspace row").clone();
        let workspace = sb.workspaces.get_mut(&key).expect("workspace");
        workspace.policies.set(
            lazybox_core::AutoFixKind::CiFailure,
            lazybox_core::PolicyArm::Arm,
        );
        workspace.policies.set(
            lazybox_core::AutoFixKind::MergeConflict,
            lazybox_core::PolicyArm::Arm,
        );

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let header: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 2)].symbol())
            .collect();
        let screen: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect();

        assert!(header.contains("AUTO-FIX ON · CI+CONFLICT"), "{header:?}");
        assert!(
            screen.contains(crate::components::sidebar::FIX_GLYPH),
            "compact auto-fix row glyph is still visible"
        );
    }

    /// #794: the focused row's merge automation is spelled out in the
    /// header so the durability difference the ` ARM ` / ` AUTO ` pills
    /// can't show is legible. lazybox's client-side arm names its
    /// while-running limit; GitHub-native auto-merge names that it works
    /// offline, and wins the header when both are set.
    #[test]
    fn focused_merge_automation_is_explained_in_the_sidebar_header() {
        // lazybox client-side arm.
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut armed = pr_ws("https://github.com/o/r/pull/1");
        armed.auto_merge_on_green = true;
        sb.workspaces.insert(SessionKey::from(&armed.key), armed);
        sb.recompute_visible();
        let header = header_at(&mut sb, 60);
        assert!(header.contains("MERGE ON GREEN"), "{header:?}");
        assert!(header.contains("lazybox only"), "{header:?}");

        // GitHub-native auto-merge takes precedence in the label.
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut both = pr_ws("https://github.com/o/r/pull/1");
        both.auto_merge_on_green = true;
        both.pr.as_mut().expect("pr").auto_merge_enabled = true;
        sb.workspaces.insert(SessionKey::from(&both.key), both);
        sb.recompute_visible();
        let header = header_at(&mut sb, 60);
        assert!(header.contains("AUTO-MERGE · GitHub"), "{header:?}");
        assert!(header.contains("works offline"), "{header:?}");
    }

    /// Render the header (row 2) at an arbitrary width.
    fn header_at(sb: &mut Sidebar, width: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, 2)].symbol())
            .collect()
    }

    fn pr_ws(url: &str) -> Workspace {
        let mut t = base_task();
        t.url = url.into();
        let mut w = Workspace::from_task(t, chrono::Utc::now());
        w.name = "Alpha".into();
        w
    }

    /// #794 regression: the merge-automation label must never render as a
    /// mid-word fragment. The old row-2 code pushed the full label and let
    /// the `Paragraph` hard-clip it, so at ~37 cells the durability word
    /// truncated to `works offli…` — dropping the exact signal the label
    /// exists to convey. The label now drops whole when it doesn't fit.
    #[test]
    fn focused_merge_label_is_shown_whole_or_not_at_all() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut both = pr_ws("https://github.com/o/r/pull/1");
        both.auto_merge_on_green = true;
        both.pr.as_mut().expect("pr").auto_merge_enabled = true;
        sb.workspaces.insert(SessionKey::from(&both.key), both);
        sb.recompute_visible();

        for width in [22u16, 26, 30, 34, 37, 38, 40, 50, 60] {
            let header = header_at(&mut sb, width);
            // If any of the label shows, the whole durable phrase shows —
            // never a fragment missing "works offline".
            if header.contains("AUTO-MERGE") {
                assert!(
                    header.contains("works offline"),
                    "width {width} rendered a truncated fragment: {header:?}"
                );
            }
        }
        // Too narrow to fit the label at all → it is fully absent, not a stub.
        assert!(
            !header_at(&mut sb, 22).contains("AUTO-MERGE"),
            "a label that can't fit must drop whole"
        );
        // Generous width → present whole.
        let wide = header_at(&mut sb, 60);
        assert!(wide.contains("AUTO-MERGE · GitHub"), "{wide:?}");
        assert!(wide.contains("works offline"), "{wide:?}");
    }

    /// #794 regression: dropping the (higher-priority) merge label under
    /// width pressure must leave the lower-priority global tally intact and
    /// whole — a group yields cleanly instead of a clip mangling the line.
    #[test]
    fn narrow_header_drops_merge_label_but_keeps_ci_tally() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut ws = pr_ws("https://github.com/o/r/pull/1");
        ws.auto_merge_on_green = true;
        ws.pr.as_mut().expect("pr").ci = lazybox_core::CiStatus::Failure;
        sb.workspaces.insert(SessionKey::from(&ws.key), ws);
        sb.recompute_visible();
        assert_eq!(sb.ci_failing_count(), 1, "the failing PR is counted");

        // 24 cells (inner 22) can't hold the 31-cell " MERGE ON GREEN ·
        // lazybox only " label, but easily holds the "1 CI" tally.
        let header = header_at(&mut sb, 24);
        assert!(
            !header.contains("MERGE ON GREEN"),
            "merge label must drop whole when it can't fit: {header:?}"
        );
        assert!(
            header.contains("CI"),
            "the global CI tally survives the merge label being dropped: {header:?}"
        );
    }

    /// #794: the width-gating applies to the pre-existing global tally
    /// too, not just the merge label — a tally that can't fit whole behind
    /// a higher-priority group drops entirely rather than clipping to a
    /// fragment like `1 C`. The old row-2 code pushed every group and let
    /// the `Paragraph` hard-clip, so this is the regression guard for the
    /// generalized behavior.
    #[test]
    fn global_tally_drops_whole_behind_a_wider_merge_label() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut ws = pr_ws("https://github.com/o/r/pull/1");
        ws.auto_merge_on_green = true;
        ws.pr.as_mut().expect("pr").ci = lazybox_core::CiStatus::Failure;
        sb.workspaces.insert(SessionKey::from(&ws.key), ws);
        sb.recompute_visible();
        assert_eq!(sb.ci_failing_count(), 1, "the failing PR is counted");

        // Wide: the 31-cell " MERGE ON GREEN · lazybox only " label and the
        // "1 CI" tally both render.
        let wide = header_at(&mut sb, 60);
        assert!(wide.contains("lazybox only"), "{wide:?}");
        assert!(wide.contains("1 CI"), "{wide:?}");

        // width 38 → inner 36: the label fits, but label + 2-cell separator
        // + 4-cell tally (37) does not. The tally drops whole; the old clip
        // would have sliced it to "1 C" trailing the label.
        let tight = header_at(&mut sb, 38);
        assert!(
            tight.contains("lazybox only"),
            "label still whole: {tight:?}"
        );
        assert!(
            tight.trim_end().ends_with("only"),
            "the tally must drop whole, not clip to a fragment: {tight:?}"
        );
    }

    /// `v` marks the cursor row and the mark survives navigating away
    /// — the selection is keyed by workspace, not row index.
    #[test]
    fn toggle_marks_row_and_survives_navigation() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta")]);
        let first = sb.selected_session_key().expect("cursor on a row").clone();
        assert_eq!(sb.toggle_broadcast_select(), Some(true));
        let mut cmds = Vec::new();
        sb.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut cmds,
        );
        assert_ne!(sb.selected_session_key(), Some(&first), "cursor moved");
        assert!(sb.is_broadcast_selected(&first), "mark survives j/k");
        // Toggling again from the OTHER row selects that one too.
        assert_eq!(sb.toggle_broadcast_select(), Some(true));
        assert_eq!(sb.broadcast_selected_count(), 2);
        // Re-toggle deselects.
        assert_eq!(sb.toggle_broadcast_select(), Some(false));
        assert_eq!(sb.broadcast_selected_count(), 1);
    }

    /// Issue #786: pressing `v` marks the *current* row visibly and
    /// immediately — the cursor row shows its `✓` (not just the caret) —
    /// and the header carries a persistent `N selected` count.
    #[test]
    fn selection_shows_on_cursor_row_and_header_count() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta")]);
        // Mark the row the cursor is already on — the repro case.
        assert_eq!(sb.toggle_broadcast_select(), Some(true));

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();

        let header: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();
        assert!(header.contains("1 selected"), "header count: {header:?}");

        // The cursor row itself carries the `✓` gutter mark, so `v` gives
        // feedback without moving off the row.
        let marked_rows = (0..buffer.area.height)
            .filter(|&y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains('✓')
            })
            .count();
        // One `✓` in the header, one in the cursor row's gutter.
        assert_eq!(marked_rows, 2, "expected header + cursor-row check marks");
    }

    /// Render `row 0` of the header at a given width.
    fn header_row(sb: &mut Sidebar, width: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(width, 12)).expect("terminal");
        term.draw(|f| sb.render(f.area(), f, true)).expect("draw");
        let buf = term.backend().buffer();
        (0..width).map(|x| buf[(x, 0)].symbol()).collect()
    }

    /// Whether any cell in the rendered screen shows a `✓`.
    fn screen_has_check(sb: &mut Sidebar, width: u16) -> bool {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(width, 12)).expect("terminal");
        term.draw(|f| sb.render(f.area(), f, true)).expect("draw");
        let buf = term.backend().buffer();
        (0..12).any(|y| (0..width).any(|x| buf[(x, y)].symbol() == "✓"))
    }

    /// Issue #786: the header count reflects only the *visible* marks —
    /// it stays in lockstep with the on-screen `✓` gutter and with what a
    /// broadcast targets. Marks on rows a filter later hides don't inflate
    /// the count (they'd otherwise claim rows that aren't there and won't
    /// broadcast).
    #[test]
    fn header_count_tracks_visible_marks_not_hidden_ones() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta")]);
        sb.toggle_broadcast_select();
        sb.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut Vec::new(),
        );
        sb.toggle_broadcast_select();
        assert_eq!(sb.broadcast_selected_count(), 2, "both rows marked");

        assert!(
            header_row(&mut sb, 60).contains("2 selected"),
            "both visible → header counts 2",
        );
        assert!(screen_has_check(&mut sb, 60), "gutter marks visible");

        // Hide every issue behind a PR-only filter. The marks persist in
        // the set, but nothing is visible — the header must not claim a
        // stale count and no gutter `✓` should paint.
        sb.set_filters([Filter::Pr]);
        assert_eq!(sb.broadcast_selected_count(), 2, "set retains the marks");
        assert_eq!(
            sb.visible_broadcast_selected_count(),
            0,
            "none of the marked rows are visible",
        );
        assert!(
            !header_row(&mut sb, 60).contains("selected"),
            "no stale count when the marked rows are hidden",
        );
        assert!(
            !screen_has_check(&mut sb, 60),
            "no gutter marks when the marked rows are hidden",
        );
    }

    /// Issue #786: the live selection count is the highest-priority
    /// header signal. On a narrow sidebar it outranks the passive badges
    /// (here `☼ awake`) instead of the whole strip dropping as an
    /// all-or-nothing block — so there is a width band where the count
    /// shows but the passive badge is dropped, and never the reverse.
    #[test]
    fn selection_count_outranks_passive_badges_when_space_is_tight() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.toggle_broadcast_select();
        sb.set_keep_awake(true);
        let key = sb.selected_session_key().expect("cursor row").clone();
        sb.agents.insert(key, lazybox_ipc::AgentState::Working);

        let mut saw_count_without_badge = false;
        for width in 24..=110u16 {
            let header = header_row(&mut sb, width);
            let has_count = header.contains("selected");
            let has_badge = header.contains("awake");
            assert!(
                !(has_badge && !has_count),
                "passive badge shown without the selection count at width {width}: {header:?}",
            );
            if has_count && !has_badge {
                saw_count_without_badge = true;
            }
        }
        assert!(
            saw_count_without_badge,
            "expected a width where the count survives but the passive badge is dropped",
        );

        // Given enough room, both coexist — the count doesn't suppress
        // the passive badge, it just wins when space is scarce.
        let wide = header_row(&mut sb, 110);
        assert!(
            wide.contains("selected") && wide.contains("awake"),
            "{wide:?}"
        );
    }

    /// The broadcast target list comes out in visible (sidebar) order
    /// regardless of the order rows were marked in.
    #[test]
    fn selected_keys_follow_visible_order() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta"), ("3", "Gamma")]);
        let visible: Vec<SessionKey> = sb
            .visible_rows()
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.clone()),
                _ => None,
            })
            .collect();
        // Mark last visible row first, then the first one.
        assert!(sb.focus_workspace_key(&visible[2]));
        sb.toggle_broadcast_select();
        assert!(sb.focus_workspace_key(&visible[0]));
        sb.toggle_broadcast_select();
        assert_eq!(
            sb.selected_broadcast_keys(),
            vec![visible[0].clone(), visible[2].clone()],
            "targets in sidebar order, not selection order",
        );
    }

    /// Esc clears the marks (consumed); with nothing selected it
    /// bubbles so outer layers keep their Esc semantics.
    #[test]
    fn esc_clears_selection_then_passes_through() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.toggle_broadcast_select();
        assert_eq!(sb.broadcast_selected_count(), 1);
        let mut cmds = Vec::new();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(sb.handle_key(esc, &mut cmds), PaneOutcome::Consumed);
        assert_eq!(sb.broadcast_selected_count(), 0);
        assert_eq!(
            sb.handle_key(esc, &mut cmds),
            PaneOutcome::Pass,
            "no selection — Esc bubbles",
        );
    }

    /// `extend_selection` (Shift-↑/↓) sweeps a contiguous range from the
    /// cursor: at every step the marked set is a top-anchored prefix of
    /// the visible workspace order — never a gap — and a full downward
    /// sweep grabs every row (#932).
    #[test]
    fn extend_selection_sweeps_a_contiguous_range() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta"), ("3", "Gamma")]);
        let order: Vec<SessionKey> = sb
            .visible_rows()
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(order.len(), 3);

        // Cursor on the first workspace row, nothing marked yet.
        assert!(sb.focus_workspace_key(&order[0]));
        assert!(sb.selected_broadcast_keys().is_empty());

        // One visible row at a time; the selection stays a contiguous
        // top prefix, even across the KindHeader rows in between.
        let steps = sb.visible_rows().len();
        for _ in 0..steps {
            sb.extend_selection(1);
            let sel = sb.selected_broadcast_keys();
            assert_eq!(
                sel,
                order[..sel.len()].to_vec(),
                "selection stays a contiguous top prefix",
            );
        }
        assert_eq!(
            sb.selected_broadcast_keys(),
            order,
            "a full sweep ends with every row selected",
        );
    }

    /// Shift-click (`extend_selection_to`) marks every workspace row
    /// between the cursor and the clicked row, inclusive (#932).
    #[test]
    fn extend_selection_to_grabs_the_clicked_range() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta"), ("3", "Gamma")]);
        let order: Vec<SessionKey> = sb
            .visible_rows()
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.clone()),
                _ => None,
            })
            .collect();
        let last_ws_idx = sb
            .visible_rows()
            .iter()
            .rposition(|r| matches!(r, VisibleRow::Workspace(_)))
            .expect("a workspace row");

        // Cursor on the first workspace, Shift-click the last row.
        assert!(sb.focus_workspace_key(&order[0]));
        // Mirror `click_to_select`'s row math (HEADER_HEIGHT = 5).
        let area = Rect::new(0, 0, 40, 40);
        let click_row = area.y + 5 + last_ws_idx as u16;
        assert!(sb.extend_selection_to(area, click_row));
        assert_eq!(
            sb.selected_broadcast_keys(),
            order,
            "the whole cursor→click range is marked",
        );
    }

    /// A removed workspace drops out of the selection so a later
    /// broadcast can't target a ghost.
    #[test]
    fn workspace_removed_prunes_selection() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta")]);
        let key = sb.selected_session_key().expect("cursor on a row").clone();
        sb.toggle_broadcast_select();
        assert!(sb.is_broadcast_selected(&key));
        sb.on_event(&Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(
            key.as_str(),
        )));
        assert!(!sb.is_broadcast_selected(&key));
        assert_eq!(sb.broadcast_selected_count(), 0);
    }

    /// Delivery routing: agents beat shells, shells beat nothing, and
    /// agent ties break on the lowest terminal id (deterministic).
    #[test]
    fn broadcast_terminal_prefers_agent_then_shell() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        let key = sb.selected_session_key().expect("cursor on a row").clone();
        assert_eq!(sb.broadcast_terminal(&key), None, "no sessions yet");
        let spawn = |sb: &mut Sidebar, id: u64, kind: TerminalKind| {
            sb.on_event(&Event::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(id),
                session_key: key.clone(),
                kind,
                no_permission: false,
                on_main: false,
            });
        };
        spawn(&mut sb, 3, TerminalKind::Shell);
        assert_eq!(
            sb.broadcast_terminal(&key),
            Some((TerminalId(3), false)),
            "shell-only workspace delivers to the shell",
        );
        spawn(&mut sb, 7, TerminalKind::Agent("claude".into()));
        spawn(&mut sb, 5, TerminalKind::Agent("codex".into()));
        assert_eq!(
            sb.broadcast_terminal(&key),
            Some((TerminalId(5), true)),
            "agent wins over the shell; lowest id breaks the tie",
        );
    }

    /// Selected rows render the `✓` mark in the selection gutter.
    #[test]
    fn selected_row_renders_check_mark() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta")]);
        // Mark the cursor row, then move off it — the ✓ renders on
        // non-cursor rows (the cursor row keeps its caret).
        sb.toggle_broadcast_select();
        let mut cmds = Vec::new();
        sb.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut cmds,
        );
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| sb.render(f.area(), f, true))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                screen.push_str(buffer[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.contains('✓'),
            "selected row must show the ✓ mark:\n{screen}",
        );
    }

    /// Three+ active filters collapse to `a, b, +N` in the header so
    /// the chip can't push the sort chip off the row (issue #443
    /// review). The sort chip stays visible.
    #[test]
    fn many_active_filters_collapse_with_plus_n_and_keep_sort_chip() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut sb = sidebar_with_issues(&[("1", "Alpha"), ("2", "Beta")]);
        sb.set_filters([Filter::Author, Filter::Reviewer, Filter::Assignee]);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| sb.render(f.area(), f, true))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                screen.push_str(buffer[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.contains("+1"),
            "3 filters should collapse the 3rd into `+1`:\n{screen}",
        );
        assert!(
            screen.contains("o split"),
            "the sort chip must stay on the row:\n{screen}",
        );
    }
}

#[cfg(test)]
mod working_spinner_tests {
    use super::super::*;
    use lazybox_core::WorkspaceKey;
    use std::time::{Duration, Instant};

    fn working_sidebar() -> Sidebar {
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.agents.insert(
            SessionKey::from(&WorkspaceKey::new("owner/repo#1")),
            lazybox_ipc::AgentState::Working,
        );
        sb
    }

    /// The displayed frame is a pure function of elapsed time, not a
    /// per-tick counter — `epoch + 600ms` must read as frame 5
    /// (600 / 120) regardless of how many times `tick_working` ran.
    #[test]
    fn frame_is_derived_from_elapsed_time() {
        let mut sb = working_sidebar();
        sb.spinner_epoch = Instant::now() - Duration::from_millis(600);
        assert!(
            sb.tick_working(),
            "crossing a frame boundary asks for a redraw"
        );
        assert_eq!(sb.working_spinner_frame, 5);
    }

    /// Nothing working → no animation, no redraw churn.
    #[test]
    fn noop_when_no_agent_is_working() {
        let mut sb = Sidebar::new(PaneId::new(1));
        assert!(!sb.tick_working());
    }

    /// A single tick after a long gap jumps straight to the correct
    /// frame instead of crawling forward one step — this is what keeps
    /// the spinner from "freezing" then slowly catching up after the
    /// run loop stalls.
    #[test]
    fn recovers_to_correct_frame_after_a_stall() {
        let mut sb = working_sidebar();
        // One frame in, then the loop stalls for ~1.2s.
        sb.working_spinner_frame = 1;
        sb.spinner_epoch = Instant::now() - Duration::from_millis(1200);
        assert!(sb.tick_working());
        assert_eq!(sb.working_spinner_frame, 10, "jumps to now, not +1");
    }

    /// A transient `Working → Idle → Working` flap (the daemon dedupes
    /// `AgentState` and its detector can briefly misread a busy agent
    /// as idle) must not snap the spinner back to frame 0 — the phase
    /// belongs to the wall clock, so the glyph keeps spinning from
    /// where the eye left it.
    #[test]
    fn holds_phase_across_a_working_idle_flap() {
        let mut sb = working_sidebar();
        sb.spinner_epoch = Instant::now() - Duration::from_millis(600);
        sb.tick_working();
        let before = sb.working_spinner_frame;
        assert_eq!(before, 5);

        // Flap to idle: the working state clears for a beat.
        sb.agents.clear();
        assert!(!sb.tick_working(), "idle asks for no spinner redraw");
        assert_eq!(
            sb.working_spinner_frame, before,
            "frame is not reset to 0 while idle",
        );

        // Working again a little later — the frame reflects the clock,
        // strictly ahead of where it was, never restarting at 0.
        sb.agents.insert(
            SessionKey::from(&WorkspaceKey::new("owner/repo#1")),
            lazybox_ipc::AgentState::Working,
        );
        sb.spinner_epoch = Instant::now() - Duration::from_millis(840);
        assert!(sb.tick_working());
        assert_eq!(sb.working_spinner_frame, 7);
    }

    /// Repeated calls inside the same frame window don't report a
    /// change — the redraw only fires when the glyph actually advances.
    #[test]
    fn same_frame_window_is_a_noop() {
        let mut sb = working_sidebar();
        sb.spinner_epoch = Instant::now() - Duration::from_millis(600);
        assert!(sb.tick_working(), "first cross of the boundary redraws");
        assert!(!sb.tick_working(), "still in frame 5 → no second redraw",);
    }
}

/// #1069: the pre-terminal "spawning" arc. A spawn's `WorktreeProgress`
/// stream marks its workspace spawning so the row shows the agent is
/// *coming* during clone → worktree → launch, before any terminal
/// reports an `AgentState`; the first live state, the `TerminalSpawned`,
/// or a `Failed` step clears it.
#[cfg(test)]
mod spawning_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::Workspace;
    use lazybox_ipc::{
        AgentState, Event, SpawnOrigin, TerminalId, TerminalKind, WorktreeStep, WorktreeStepStatus,
    };

    fn progress(key: &SessionKey, step: WorktreeStep, status: WorktreeStepStatus) -> Event {
        Event::WorktreeProgress {
            session_key: key.clone(),
            step,
            status,
            origin: SpawnOrigin::Interactive,
        }
    }

    fn one_workspace() -> (Sidebar, SessionKey) {
        let mut t = base_task();
        t.title = "Ship it".into();
        let mut w = Workspace::from_task(t, chrono::Utc::now());
        w.name = "Ship it".into();
        let key = SessionKey::from(&w.key);
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.workspaces.insert(key.clone(), w);
        sb.recompute_visible();
        (sb, key)
    }

    #[test]
    fn worktree_progress_started_marks_spawning() {
        let (mut sb, key) = one_workspace();
        assert!(!sb.is_spawning(&key));
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        assert!(sb.is_spawning(&key));
    }

    /// A single step finishing (or reporting live progress) doesn't clear
    /// the arc — more steps, and finally the agent, are still coming.
    #[test]
    fn a_step_completing_keeps_spawning() {
        let (mut sb, key) = one_workspace();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Done,
        ));
        assert!(sb.is_spawning(&key), "more steps still to come");
        sb.on_event(&progress(
            &key,
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Progress("42%".into()),
        ));
        assert!(sb.is_spawning(&key));
    }

    #[test]
    fn first_agent_state_clears_spawning() {
        let (mut sb, key) = one_workspace();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Setup,
            WorktreeStepStatus::Started,
        ));
        assert!(sb.is_spawning(&key));
        sb.on_event(&Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Working,
        });
        assert!(!sb.is_spawning(&key), "the live agent owns the slot now");
    }

    /// #1069 redraw gate: while spawning, the row shows the arc, not the
    /// agent glyph — so the orchestrator's `changed` check (which reads
    /// `displays_agent_state`) must repaint when the first `AgentState`
    /// arrives, *even for `Idle`*. Without this, an `Idle`-first agent
    /// whose `TerminalSpawned` was dropped on the lossy bus would clear
    /// the spawning set with no repaint, stranding the arc on screen.
    #[test]
    fn displays_agent_state_forces_repaint_out_of_spawning() {
        let (mut sb, key) = one_workspace();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        // Absent entry and `Idle` both map to "no glyph", but the row
        // currently shows the arc — so folding `Idle` in *does* change it.
        assert!(
            !sb.displays_agent_state(&key, AgentState::Idle),
            "spawning row must repaint when the first (Idle) state lands"
        );
        // Once the state is folded and spawning cleared, the normal
        // no-op dedup applies again so repeated pings don't churn redraws.
        sb.on_event(&Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Idle,
        });
        assert!(!sb.is_spawning(&key));
        assert!(sb.displays_agent_state(&key, AgentState::Idle));
    }

    #[test]
    fn terminal_spawned_clears_spawning() {
        let (mut sb, key) = one_workspace();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Setup,
            WorktreeStepStatus::Started,
        ));
        sb.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        assert!(!sb.is_spawning(&key));
    }

    #[test]
    fn spawn_failure_clears_spawning() {
        let (mut sb, key) = one_workspace();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        assert!(sb.is_spawning(&key));
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Failed("boom".into()),
        ));
        assert!(
            !sb.is_spawning(&key),
            "a failed spawn must not spin forever"
        );
    }

    fn spawn_error(source: &str) -> Event {
        Event::ProviderError {
            source: source.into(),
            message: "agent binary not found".into(),
            detail: String::new(),
            kind: String::new(),
        }
    }

    /// A post-provisioning agent-*launch* failure emits only a keyless
    /// `ProviderError` (source `"spawn"`) — never a `WorktreeStepStatus::
    /// Failed` — so the arc, set while the worktree provisioned, would
    /// otherwise spin forever. The spawn error must clear it (#1069).
    #[test]
    fn spawn_provider_error_clears_the_arc() {
        let (mut sb, key) = one_workspace();
        // Provisioning ran (arc set); the daemon then failed to launch the
        // agent — no `Failed` step, only the keyless spawn `ProviderError`.
        sb.on_event(&progress(
            &key,
            WorktreeStep::Setup,
            WorktreeStepStatus::Started,
        ));
        assert!(sb.is_spawning(&key));
        sb.on_event(&spawn_error("spawn"));
        assert!(
            !sb.is_spawning(&key),
            "a keyless launch-failure spawn error must clear the arc"
        );
    }

    /// An unrelated provider (sync) error must NOT touch the arc — only
    /// `spawn*`-sourced failures mean a spawn ended.
    #[test]
    fn non_spawn_provider_error_leaves_the_arc() {
        let (mut sb, key) = one_workspace();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        sb.on_event(&spawn_error("github"));
        assert!(
            sb.is_spawning(&key),
            "a sync-provider error is unrelated to the spawn arc"
        );
    }

    /// Finding 2: a workspace with a *live* `Working` agent and a second
    /// session cold-provisioning shows the working spinner (working >
    /// spawning), so the live agent's repeated pings must still dedup —
    /// `displays_agent_state` must not force a repaint just because the
    /// key is in the spawning set (#1069).
    #[test]
    fn live_sibling_pings_dedup_during_a_concurrent_spawn() {
        let (mut sb, key) = one_workspace();
        sb.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(1),
            session_key: key.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
        sb.on_event(&Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Working,
        });
        // A second session for the same workspace starts provisioning.
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        assert!(sb.is_spawning(&key), "the second session is provisioning");
        // The row shows the live working spinner, so a repeated Working
        // ping is a no-op that must NOT force a redraw.
        assert!(
            sb.displays_agent_state(&key, AgentState::Working),
            "a live sibling's repeated ping must still dedup during a spawn"
        );
    }

    /// Removing a workspace mid-spawn drops its spawning entry so a
    /// cancelled/closed workspace can't leak a stuck spinner.
    #[test]
    fn removing_a_workspace_clears_spawning() {
        let (mut sb, key) = one_workspace();
        let ws_key = sb.workspaces.get(&key).unwrap().key.clone();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        sb.on_event(&Event::WorkspaceRemoved(ws_key));
        assert!(!sb.is_spawning(&key));
    }

    /// The shared spinner counter advances while a row is merely
    /// spawning, even with no agent yet `Working`.
    #[test]
    fn spinner_animates_while_only_spawning() {
        use std::time::{Duration, Instant};
        let (mut sb, key) = one_workspace();
        assert!(
            !sb.tick_working(),
            "nothing spawning or working → no animation"
        );
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        sb.spinner_epoch = Instant::now() - Duration::from_millis(600);
        assert!(
            sb.tick_working(),
            "a spawning row animates the shared spinner"
        );
        assert_eq!(sb.working_spinner_frame, 5);
    }

    /// Release regression #1156: the provisioning state belongs to the target
    /// workspace, not the cursor. Navigating to another row must leave the
    /// original row's animated spawning arc intact.
    #[test]
    fn spawning_arc_survives_focus_moving_to_another_workspace() {
        let (mut sb, spawning_key) = one_workspace();
        let mut other_task = base_task();
        other_task.id.key = "o/r#2".into();
        other_task.title = "Other work".into();
        let other = Workspace::from_task(other_task, chrono::Utc::now());
        let other_key = SessionKey::from(&other.key);
        sb.workspaces.insert(other_key.clone(), other);
        sb.recompute_visible();

        sb.on_event(&progress(
            &spawning_key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));
        assert!(sb.focus_workspace_key(&other_key));

        assert!(
            sb.is_spawning(&spawning_key),
            "focus changes cannot erase another workspace's provision state"
        );
        assert_eq!(sb.selected_session_key(), Some(&other_key));
    }

    /// Acceptance render: the row shows the distinct spawning arc during
    /// provisioning, then yields to the working braille spinner once the
    /// agent goes live.
    #[test]
    fn row_shows_spawning_arc_then_working_spinner() {
        use crate::components::workspace_row::{spawning_glyph, working_glyph};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (mut sb, key) = one_workspace();
        sb.on_event(&progress(
            &key,
            WorktreeStep::Clone,
            WorktreeStepStatus::Started,
        ));

        fn screen(sb: &mut Sidebar) -> String {
            let backend = TestBackend::new(60, 12);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| sb.render(frame.area(), frame, true))
                .expect("draw");
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_string())
                })
                .collect()
        }

        let spawning = screen(&mut sb);
        assert!(
            spawning.contains(spawning_glyph(0)),
            "spawning arc must be on the row: {spawning:?}"
        );
        assert!(
            !spawning.contains(working_glyph(0)),
            "not the working spinner yet: {spawning:?}"
        );

        sb.on_event(&Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(1),
            state: AgentState::Working,
        });
        assert!(!sb.is_spawning(&key));
        let working = screen(&mut sb);
        assert!(
            working.contains(working_glyph(0)),
            "working spinner must replace the arc: {working:?}"
        );
        assert!(
            !working.contains(spawning_glyph(0)),
            "spawning arc cleared once live: {working:?}"
        );
    }
}

#[cfg(test)]
mod done_alert_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::Workspace;
    use lazybox_ipc::{AgentState, Event};

    fn agent_state(key: &SessionKey, state: AgentState) -> Event {
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: lazybox_ipc::TerminalId(1),
            state,
        }
    }

    fn sidebar_with_one_workspace() -> (Sidebar, SessionKey) {
        let mut t = base_task();
        t.title = "Ship it".into();
        let mut w = Workspace::from_task(t, chrono::Utc::now());
        w.name = "Ship it".into();
        let key = SessionKey::from(&w.key);
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.workspaces.insert(key.clone(), w);
        sb.recompute_visible();
        (sb, key)
    }

    /// On a WORKSPACE row the obvious always-on actions — repo-group
    /// collapse (`Space`) and pin (`p`) — are kept OUT of the footer so
    /// they can't crowd out the state-driven hints that matter on the
    /// selected row (#1026). They stay mouse-discoverable (the header
    /// ▾/▸ triangle) and in `?` help; the state-contextual `Work` action
    /// leads instead.
    #[test]
    fn obvious_group_actions_absent_on_workspace_row() {
        let catalog =
            lazybox_tui_core::action::ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let (mut sb, key) = sidebar_with_one_workspace();
        assert!(sb.focus_workspace_key(&key), "cursor on the workspace");
        // A workspace is selected — the OLD footer appended collapse +
        // pin here unconditionally; the new footer must not.
        assert!(sb.selected_workspace().is_some(), "workspace is selected");
        assert!(
            sb.cursor_repo().is_some(),
            "workspace row resolves to a group"
        );

        let binds = sb.contextual_bindings(&catalog, false);
        assert!(
            !binds
                .iter()
                .any(|b| b.label == "collapse group" || b.label == "expand group"),
            "collapse-group must not occupy a footer slot on a workspace row: {binds:?}",
        );
        assert!(
            !binds
                .iter()
                .any(|b| b.label == "pin group" || b.label == "unpin group"),
            "pin-group must not occupy a footer slot: {binds:?}",
        );
        // The state-contextual Work action leads the bar (its verb
        // tracks the row's classification — never a group/pin hint).
        let work_label = lazybox_tui_core::action::contextual_label(
            &lazybox_tui_core::action::Action::Work,
            sb.selected_workspace(),
        );
        assert_eq!(
            binds.first().map(|b| b.label.as_ref()),
            Some(work_label),
            "footer leads with the state-driven Work hint: {binds:?}",
        );
    }

    /// On a repo/space HEADER row no workspace is selected, so nothing
    /// state-driven competes and folding the group you're sitting on is
    /// the likely next action — collapse (`Space`) is surfaced there,
    /// with a verb that tracks the group's state (#1026). Pin stays
    /// dropped even here (it's the secondary action on a header).
    #[test]
    fn collapse_hint_returns_on_header_row() {
        let catalog =
            lazybox_tui_core::action::ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let (mut sb, _key) = sidebar_with_one_workspace();

        // Park the cursor on the repo header row.
        let header_idx = sb
            .visible_rows()
            .iter()
            .position(|r| matches!(r, VisibleRow::RepoHeader(_)))
            .expect("a repo header row exists");
        sb.set_cursor(header_idx);
        assert!(
            sb.selected_workspace().is_none(),
            "header row selects no workspace",
        );
        assert!(sb.cursor_repo().is_some(), "header resolves to its group");

        let expanded = sb.contextual_bindings(&catalog, false);
        assert!(
            expanded.iter().any(|b| b.label == "collapse group"),
            "expanded group on a header → `collapse group`: {expanded:?}",
        );
        assert!(
            !expanded.iter().any(|b| b.label.contains("pin")),
            "pin stays dropped even on a header: {expanded:?}",
        );

        // Fold it — the cursor re-parks on the header and the verb flips.
        sb.toggle_repo_at_cursor();
        assert_eq!(sb.cursor_repo_collapsed(), Some(true), "group is collapsed");
        let collapsed = sb.contextual_bindings(&catalog, false);
        assert!(
            collapsed.iter().any(|b| b.label == "expand group"),
            "collapsed group on a header → `expand group`: {collapsed:?}",
        );
    }

    /// A live multi-select leads the footer with the broadcast action —
    /// the one selection-only action with no single-row equivalent —
    /// ahead of the per-row state hints, and never surfaces the obvious
    /// group/pin cells (#1026, #932).
    #[test]
    fn multi_select_footer_leads_with_broadcast() {
        let catalog =
            lazybox_tui_core::action::ActionDef::catalog(&[], &std::collections::BTreeMap::new());
        let (mut sb, key) = sidebar_with_one_workspace();
        assert!(sb.focus_workspace_key(&key), "cursor on the workspace");
        sb.toggle_broadcast_select();
        assert!(sb.is_broadcast_selected(&key), "row is selected");

        let binds = sb.contextual_bindings(&catalog, false);
        assert_eq!(
            binds.first().map(|b| b.label.as_ref()),
            Some("broadcast"),
            "multi-select footer leads with broadcast: {binds:?}",
        );
        assert!(
            !binds.iter().any(|b| b.label.contains("group")),
            "no obvious group/pin cells under selection: {binds:?}",
        );
    }

    /// The editor action launches locally against a server-side worktree
    /// path, so a remote (`--connect`) client omits it from the sidebar
    /// footer hints while a local client keeps it. See #742.
    #[test]
    fn editor_footer_hint_hidden_for_remote_client() {
        let catalog = lazybox_tui_core::action::ActionDef::catalog(
            &["claude".to_string()],
            &std::collections::BTreeMap::new(),
        );
        let (mut sb, key) = sidebar_with_one_workspace();
        assert!(sb.focus_workspace_key(&key), "cursor on the workspace");

        assert!(
            sb.contextual_bindings(&catalog, false)
                .iter()
                .any(|b| b.label == "editor"),
            "local client offers the editor footer hint",
        );
        assert!(
            !sb.contextual_bindings(&catalog, true)
                .iter()
                .any(|b| b.label == "editor"),
            "remote client hides the editor footer hint",
        );
    }

    /// Focus mode (`.`) surfaces in the contextual footer only when the
    /// selected workspace has a coding agent to maximize — advertising
    /// it on an agent-less row would point at a no-op key.
    #[test]
    fn focus_mode_surfaces_in_footer_only_with_an_agent() {
        let catalog = lazybox_tui_core::action::ActionDef::catalog(
            &["claude".to_string()],
            &std::collections::BTreeMap::new(),
        );

        // Plain workspace, no agent session → no focus-mode hint.
        let (mut sb, key) = sidebar_with_one_workspace();
        assert!(sb.focus_workspace_key(&key), "cursor on the workspace");
        assert!(
            !sb.contextual_bindings(&catalog, false)
                .iter()
                .any(|b| b.label.contains("focus mode")),
            "no agent → no focus-mode footer hint",
        );

        // Add an agent session → `.` focus-mode hint appears.
        let wk = sb.workspaces.get(&key).unwrap().key.clone();
        sb.workspaces
            .get_mut(&key)
            .unwrap()
            .add_session(lazybox_core::WorkspaceSession::new(
                wk,
                lazybox_core::SessionKind::Agent {
                    agent_id: "claude".into(),
                },
                std::path::PathBuf::from("/tmp/wt"),
                chrono::Utc::now(),
            ));
        sb.recompute_visible();
        assert!(sb.focus_workspace_key(&key));
        assert!(
            sb.contextual_bindings(&catalog, false)
                .iter()
                .any(|b| b.label.contains("focus mode") && b.keys == "."),
            "agent present → `.` focus-mode footer hint",
        );
    }

    /// Reaching `Done` (agent finished its turn) alerts the user: it
    /// flags the done-set, queues an OS notification, and a footer
    /// notice — the #80 "tell me when it's done" requirement.
    #[test]
    fn reaching_done_flags_set_and_alerts() {
        let (mut sb, key) = sidebar_with_one_workspace();
        sb.on_event(&agent_state(&key, AgentState::Working));
        // Working alone is silent — nothing to act on.
        assert!(sb.drain_pending_notifications().is_empty());
        assert!(sb.drain_pending_asking_notices().is_empty());

        sb.on_event(&agent_state(&key, AgentState::Done));
        assert_eq!(
            sb.agent_state(&key),
            Some(AgentState::Done),
            "state is Done"
        );
        let notifs = sb.drain_pending_notifications();
        assert_eq!(notifs.len(), 1);
        assert!(notifs[0].title.contains("finished"), "OS banner on done");
        assert!(
            sb.drain_pending_asking_notices()
                .iter()
                .any(|n| n.contains("finished")),
            "footer notice on done",
        );
    }

    /// The daemon re-emits `Done` on follow-up chunks; the alert must
    /// fire once (rising edge only), not on every repeat.
    #[test]
    fn repeated_done_does_not_re_alert() {
        let (mut sb, key) = sidebar_with_one_workspace();
        sb.on_event(&agent_state(&key, AgentState::Done));
        sb.drain_pending_notifications();
        sb.drain_pending_asking_notices();

        sb.on_event(&agent_state(&key, AgentState::Done));
        assert!(
            sb.drain_pending_notifications().is_empty(),
            "no re-notify on a repeat Done broadcast",
        );
        assert!(sb.drain_pending_asking_notices().is_empty());
    }

    /// Working again after Done clears the done flag so the row stops
    /// showing `✓` once the agent resumes.
    #[test]
    fn working_after_done_clears_the_flag() {
        let (mut sb, key) = sidebar_with_one_workspace();
        sb.on_event(&agent_state(&key, AgentState::Done));
        assert_eq!(sb.agent_state(&key), Some(AgentState::Done));
        sb.on_event(&agent_state(&key, AgentState::Working));
        assert_eq!(sb.agent_state(&key), Some(AgentState::Working));
    }

    /// #356/#357: a dead agent's `Exited` pill must SURVIVE the
    /// `TerminalExited` event that tears its terminal down — the workspace
    /// stays with the exit marker (a restart affordance), rather than the
    /// pill blanking to nothing. Only a new agent, or removing the
    /// workspace, clears it.
    #[test]
    fn exited_state_survives_the_terminal_exit_event() {
        let (mut sb, key) = sidebar_with_one_workspace();
        sb.on_event(&agent_state(&key, AgentState::Working));
        // The daemon broadcasts the terminal Exited state, then the
        // TerminalExited that removes the terminal itself.
        sb.on_event(&agent_state(&key, AgentState::Exited { code: Some(1) }));
        assert_eq!(
            sb.agent_state(&key),
            Some(AgentState::Exited { code: Some(1) })
        );
        sb.on_event(&Event::TerminalExited {
            terminal_id: lazybox_ipc::TerminalId(1),
            exit_code: Some(1),
            last_output: None,
        });
        assert_eq!(
            sb.agent_state(&key),
            Some(AgentState::Exited { code: Some(1) }),
            "the exit marker must persist as a restart affordance",
        );
        // A fresh agent spawn clears it.
        sb.on_event(&agent_state(&key, AgentState::Working));
        assert_eq!(sb.agent_state(&key), Some(AgentState::Working));
    }

    /// Removing a workspace drops its agent-state entry, so a churn of
    /// closed/merged workspaces can't leak stale keys into the map over
    /// a long session.
    #[test]
    fn removing_a_workspace_prunes_its_agent_state() {
        let (mut sb, key) = sidebar_with_one_workspace();
        let ws_key = sb.workspaces.get(&key).unwrap().key.clone();
        sb.on_event(&agent_state(&key, AgentState::Working));
        assert_eq!(sb.agent_state(&key), Some(AgentState::Working));
        sb.on_event(&Event::WorkspaceRemoved(ws_key));
        assert_eq!(
            sb.agent_state(&key),
            None,
            "state entry pruned with the workspace"
        );
    }

    /// Footer notices must never carry the raw workspace name —
    /// issue/PR workspaces are named after their full issue title,
    /// which displaces the footer's shortcut hints (#291). Both the
    /// asking and done notices cap it to a short slug.
    #[test]
    fn agent_notices_truncate_long_workspace_names() {
        let long = "Footer notices (workspace/issue titles) hide the shortcut \
                    hints — hints must always stay visible";
        let (mut sb, key) = sidebar_with_one_workspace();
        sb.workspaces.get_mut(&key).unwrap().name = long.into();

        sb.on_event(&agent_state(&key, AgentState::InputNeeded));
        let asking = sb.drain_pending_asking_notices();
        assert_eq!(asking.len(), 1);
        assert!(!asking[0].contains(long), "raw title leaked: {}", asking[0]);
        assert!(asking[0].contains('…'), "cut must be visible");
        assert!(asking[0].ends_with("needs input — press ! to jump"));

        sb.on_event(&agent_state(&key, AgentState::Done));
        let done = sb.drain_pending_asking_notices();
        assert_eq!(done.len(), 1);
        assert!(!done[0].contains(long), "raw title leaked: {}", done[0]);
        assert!(done[0].ends_with("finished"));
    }

    /// Short names pass through untouched — the slug cap only bites
    /// on title-length names.
    #[test]
    fn agent_notices_keep_short_workspace_names_intact() {
        let (mut sb, key) = sidebar_with_one_workspace();
        sb.on_event(&agent_state(&key, AgentState::InputNeeded));
        let asking = sb.drain_pending_asking_notices();
        assert_eq!(
            asking,
            vec!["Ship it needs input — press ! to jump".to_string()]
        );
    }
}

#[cfg(test)]
mod rebadge_attention_tests {
    //! Issue #205: combining an issue into a PR (`x j`) rebadges the
    //! issue's live terminals onto the PR. The sidebar's transient
    //! agent-state sets are keyed by session, so a `TerminalsRebadged`
    //! must migrate them — otherwise an agent parked on a prompt (which
    //! the daemon never re-broadcasts) keeps its `?` pill pinned to the
    //! deleted issue key and the PR row shows no badge, reading as lost.
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{SessionKind, WorkspaceKey, WorkspaceSession};
    use lazybox_ipc::{AgentState, Event, TerminalId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    #[test]
    fn rebadge_repoints_the_runner_badge_onto_the_pr() {
        // #241: the agent-count badge (`N C`) reads `running_terminals`,
        // keyed by session. A rebadge must move the terminal there too,
        // or the PR row shows no badge while the deleted issue key keeps it.
        let issue: SessionKey = (&WorkspaceKey::new("github:o/r#80")).into();
        let pr: SessionKey = (&WorkspaceKey::new("github:o/r#81")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.running_terminals.insert(
            TerminalId(1),
            (issue.clone(), TerminalKind::Agent("claude".to_string())),
        );

        sb.on_event(&Event::TerminalsRebadged {
            from: issue.clone(),
            to: pr.clone(),
        });

        assert_eq!(
            sb.runner_badges(&issue),
            vec![],
            "the absorbed issue row must lose the badge",
        );
        assert_eq!(
            sb.runner_badges(&pr),
            vec![('C', 1)],
            "the PR row must inherit the `1 C` runner badge",
        );
    }

    #[test]
    fn a_live_claude_and_codex_both_badge_in_the_columns() {
        // #440: two agents in one workspace must each surface their own
        // letter. Codex declares `X` on the `Agent` trait; Claude `C`.
        // The pre-fix hardcoded match still special-cased codex, but the
        // point of the fix is that identity is generic — a rebadge (next
        // test) and any new agent inherit the same path.
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#90")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.running_terminals.insert(
            TerminalId(1),
            (ws.clone(), TerminalKind::Agent("claude".to_string())),
        );
        sb.running_terminals.insert(
            TerminalId(2),
            (ws.clone(), TerminalKind::Agent("codex".to_string())),
        );

        assert_eq!(
            sb.runner_badges(&ws),
            vec![('C', 1), ('X', 1)],
            "both the Claude (`C`) and Codex (`X`) badges must show",
        );
    }

    #[test]
    fn bulk_badge_aggregation_matches_the_per_key_reference() {
        // #1031: the sidebar render switched from a per-row
        // O(terminals) scan (`runner_badges`/`agent_models`) to one
        // aggregated pass (`*_by_key`). The bulk maps must agree with
        // the per-key reference for every workspace, or rows badge wrong.
        let a: SessionKey = (&WorkspaceKey::new("github:o/r#1")).into();
        let b: SessionKey = (&WorkspaceKey::new("github:o/r#2")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.show_agent_model = true;
        sb.running_terminals.insert(
            TerminalId(1),
            (a.clone(), TerminalKind::Agent("claude".into())),
        );
        sb.running_terminals.insert(
            TerminalId(2),
            (a.clone(), TerminalKind::Agent("codex".into())),
        );
        sb.running_terminals.insert(
            TerminalId(3),
            (b.clone(), TerminalKind::Agent("claude".into())),
        );
        sb.terminal_models.insert(TerminalId(1), "Opus".to_string());
        sb.terminal_models
            .insert(TerminalId(3), "Sonnet".to_string());

        let badges = sb.runner_badges_by_key();
        let models = sb.agent_models_by_key();
        for key in [&a, &b] {
            assert_eq!(
                badges.get(key).cloned().unwrap_or_default(),
                sb.runner_badges(key),
                "bulk badges must match the per-key reference for {key:?}",
            );
            assert_eq!(
                models.get(key).cloned().unwrap_or_default(),
                sb.agent_models(key),
                "bulk models must match the per-key reference for {key:?}",
            );
        }
        // Sanity: the aggregation carried real content, not just agreed
        // on emptiness. `a` has two distinct agents; only Claude's model
        // is unambiguous (`codex` has no label).
        assert_eq!(
            badges.get(&a).cloned().unwrap_or_default(),
            vec![('C', 1), ('X', 1)],
        );
        assert_eq!(
            models.get(&a).cloned().unwrap_or_default(),
            vec![('C', "Opus".to_string())],
        );
    }

    #[test]
    fn rebadge_preserves_both_the_claude_and_codex_badges() {
        // #440 / #404: an issue→PR transfer must carry EVERY agent kind,
        // codex included, onto the PR row — not just Claude.
        let issue: SessionKey = (&WorkspaceKey::new("github:o/r#91")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut task = base_task();
        task.id.key = "o/r#92".into();
        task.title = "Transferred PR".into();
        task.url = "https://github.com/o/r/pull/92".into();
        let mut workspace = Workspace::from_task(task, chrono::Utc::now());
        let pr = SessionKey::from(&workspace.key);
        sb.workspaces.insert(pr.clone(), workspace.clone());
        sb.recompute_visible();
        sb.running_terminals.insert(
            TerminalId(1),
            (issue.clone(), TerminalKind::Agent("claude".to_string())),
        );
        sb.running_terminals.insert(
            TerminalId(2),
            (issue.clone(), TerminalKind::Agent("codex".to_string())),
        );

        sb.on_event(&Event::TerminalsRebadged {
            from: issue.clone(),
            to: pr.clone(),
        });

        assert_eq!(sb.runner_badges(&issue), vec![], "issue row loses both");
        assert_eq!(
            sb.runner_badges(&pr),
            vec![('C', 1), ('X', 1)],
            "the PR row inherits BOTH the Claude and Codex badges",
        );

        workspace.add_session(WorkspaceSession::new(
            workspace.key.clone(),
            SessionKind::Agent {
                agent_id: "claude".into(),
            },
            PathBuf::from("/tmp/transferred-pr"),
            chrono::Utc::now(),
        ));
        sb.on_event(&Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));

        // Jump numbers now ride only focused (starred) workspaces, so star
        // the PR row to make its `]]1` badge render alongside the agents.
        sb.focused_workspaces.push(pr.clone());

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sb.render(frame.area(), frame, true))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let row = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .find(|line| line.contains("Transferred PR"))
            .expect("transferred PR row");
        assert!(
            row.contains(" 1CX"),
            "transferred PR row must visibly render its jump number and both agents: {row:?}",
        );
    }

    #[test]
    fn agent_badge_comes_from_the_trait_not_a_first_char_collision() {
        // Cursor's id is `cursor-agent`; its first char is `C`, which
        // would collide with Claude under the old first-char fallback.
        // The trait declares `U`, so the generic lookup can't collide.
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#93")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.running_terminals.insert(
            TerminalId(1),
            (ws.clone(), TerminalKind::Agent("claude".to_string())),
        );
        sb.running_terminals.insert(
            TerminalId(2),
            (ws.clone(), TerminalKind::Agent("cursor-agent".to_string())),
        );

        assert_eq!(
            sb.runner_badges(&ws),
            vec![('C', 1), ('U', 1)],
            "Cursor declares `U`, so it never collapses onto Claude's `C`",
        );
    }

    fn agent_state(key: &SessionKey, state: AgentState) -> Event {
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(1),
            state,
        }
    }

    #[test]
    fn rebadge_carries_a_parked_input_needed_agent_onto_the_pr() {
        let issue: SessionKey = (&WorkspaceKey::new("github:o/r#50")).into();
        let pr: SessionKey = (&WorkspaceKey::new("github:o/r#51")).into();
        let mut sb = Sidebar::new(PaneId::new(1));

        // Agent on the issue is parked on a prompt.
        sb.on_event(&agent_state(&issue, AgentState::InputNeeded));
        assert_eq!(sb.agent_state(&issue), Some(AgentState::InputNeeded));

        // Collapse: the daemon rebadges the terminal onto the PR. No
        // further AgentState follows — the agent is stalled.
        sb.on_event(&Event::TerminalsRebadged {
            from: issue.clone(),
            to: pr.clone(),
        });

        assert_eq!(
            sb.agent_state(&issue),
            None,
            "the dead issue key must be dropped",
        );
        assert_eq!(
            sb.agent_state(&pr),
            Some(AgentState::InputNeeded),
            "the PR must inherit the asking pill so the agent stays visible",
        );
    }

    #[test]
    fn rebadge_migrates_working_and_done_state_too() {
        let issue: SessionKey = (&WorkspaceKey::new("github:o/r#60")).into();
        let pr: SessionKey = (&WorkspaceKey::new("github:o/r#61")).into();

        for state in [AgentState::Working, AgentState::Done] {
            let mut sb = Sidebar::new(PaneId::new(1));
            sb.on_event(&agent_state(&issue, state));
            sb.on_event(&Event::TerminalsRebadged {
                from: issue.clone(),
                to: pr.clone(),
            });
            assert_eq!(sb.agent_state(&issue), None, "{state:?}: issue key dropped");
            assert_eq!(
                sb.agent_state(&pr),
                Some(state),
                "{state:?}: PR key inherited"
            );
        }
    }

    #[test]
    fn rebadge_of_an_idle_agent_does_not_flag_the_pr() {
        // No agent state on the issue → the PR must not gain a spurious
        // badge from the rebadge.
        let issue: SessionKey = (&WorkspaceKey::new("github:o/r#70")).into();
        let pr: SessionKey = (&WorkspaceKey::new("github:o/r#71")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.on_event(&Event::TerminalsRebadged {
            from: issue,
            to: pr.clone(),
        });
        assert_eq!(sb.agent_state(&pr), None);
    }
}

mod agent_model_badge_tests {
    //! #779: the sidebar surfaces each agent's model + reasoning-effort
    //! label next to its runner badge. `agent_models` is the single
    //! producer feeding the row; it reads the per-terminal `terminal_models`
    //! map kept in sync by the spawn / model-changed / exit / snapshot
    //! handlers.
    use super::super::*;
    use lazybox_core::WorkspaceKey;
    use lazybox_ipc::{Event, TerminalSnapshot};

    fn spawn(sb: &mut Sidebar, id: u64, key: &SessionKey, agent: &str, model: Option<&str>) {
        sb.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(id),
            session_key: key.clone(),
            kind: TerminalKind::Agent(agent.into()),
            no_permission: false,
            on_main: false,
            model_label: model.map(str::to_string),
        });
    }

    fn models(sb: &Sidebar, key: &SessionKey) -> Vec<(char, String)> {
        let mut got = sb.agent_models(key);
        got.sort();
        got
    }

    #[test]
    fn spawn_model_label_surfaces_next_to_the_badge() {
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#1")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        spawn(&mut sb, 1, &ws, "claude", Some("Opus"));
        assert_eq!(models(&sb, &ws), vec![('C', "Opus".to_string())]);
    }

    #[test]
    fn spawn_without_a_tier_shows_no_label() {
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#2")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        spawn(&mut sb, 1, &ws, "codex", None);
        assert_eq!(models(&sb, &ws), vec![], "a default-model spawn stays bare");
    }

    #[test]
    fn model_changed_supersedes_the_spawn_tier() {
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#3")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        spawn(&mut sb, 1, &ws, "codex", None);
        sb.on_event(&Event::TerminalModelChanged {
            session_key: ws.clone(),
            terminal_id: TerminalId(1),
            model_label: "gpt-5.5 · xhigh".to_string(),
        });
        assert_eq!(
            models(&sb, &ws),
            vec![('X', "gpt-5.5 · xhigh".to_string())],
            "the live-detected model must win over the spawn tier",
        );
    }

    #[test]
    fn snapshot_seeds_the_model_from_the_terminal() {
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#4")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.on_event(&Event::Snapshot {
            workspaces: Vec::new(),
            terminals: vec![TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: ws.clone(),
                kind: TerminalKind::Agent("claude".into()),
                replay: Vec::new(),
                last_seq: 0,
                replay_available: true,
                no_permission: false,
                on_main: false,
                model_label: Some("Sonnet".to_string()),
                prompt_history: Vec::new(),
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            }],
            projects: Vec::new(),
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert_eq!(
            models(&sb, &ws),
            vec![('C', "Sonnet".to_string())],
            "a reconnect snapshot must not lose the model label",
        );
    }

    #[test]
    fn terminal_exit_drops_the_model() {
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#5")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        spawn(&mut sb, 1, &ws, "claude", Some("Opus"));
        sb.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });
        assert_eq!(models(&sb, &ws), vec![], "the exited agent's model clears");
    }

    #[test]
    fn two_same_kind_agents_drop_the_ambiguous_model() {
        // Two Claude terminals collapse to a single `C×2` badge, so their
        // models are ambiguous against one letter — dropped, not guessed.
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#6")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        spawn(&mut sb, 1, &ws, "claude", Some("Opus"));
        spawn(&mut sb, 2, &ws, "claude", Some("Sonnet"));
        assert_eq!(
            models(&sb, &ws),
            vec![],
            "same-kind agents can't attribute a model to the collapsed badge",
        );
    }

    #[test]
    fn distinct_agents_each_keep_their_own_model() {
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#7")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        spawn(&mut sb, 1, &ws, "claude", Some("Opus"));
        spawn(&mut sb, 2, &ws, "codex", None);
        sb.on_event(&Event::TerminalModelChanged {
            session_key: ws.clone(),
            terminal_id: TerminalId(2),
            model_label: "gpt-5.5 · xhigh".to_string(),
        });
        assert_eq!(
            models(&sb, &ws),
            vec![
                ('C', "Opus".to_string()),
                ('X', "gpt-5.5 · xhigh".to_string()),
            ],
            "distinct letters each attribute their own model",
        );
    }

    #[test]
    fn show_agent_model_off_suppresses_every_label() {
        let ws: SessionKey = (&WorkspaceKey::new("github:o/r#8")).into();
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.set_show_agent_model(false);
        spawn(&mut sb, 1, &ws, "claude", Some("Opus"));
        assert_eq!(
            models(&sb, &ws),
            vec![],
            "the opt-out keeps the sidebar compact",
        );
    }
}

mod getting_started_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;

    fn one_workspace_sidebar() -> Sidebar {
        let mut sb = Sidebar::new(PaneId::new(1));
        let mut t = base_task();
        t.id.key = "1".into();
        t.url = "https://github.com/o/r/pull/1".into();
        let w = Workspace::from_task(t, chrono::Utc::now());
        sb.workspaces.insert(SessionKey::from(&w.key), w);
        sb.recompute_visible();
        sb
    }

    #[test]
    fn fresh_empty_inbox_is_getting_started() {
        // Default construction: Inbox mailbox, All filter, no rows.
        let sb = Sidebar::new(PaneId::new(1));
        assert!(sb.is_getting_started());
    }

    #[test]
    fn populated_inbox_is_not_getting_started() {
        let sb = one_workspace_sidebar();
        assert_eq!(sb.workspace_count(), 1);
        assert!(!sb.is_getting_started());
    }

    #[test]
    fn filtered_empty_view_is_not_getting_started() {
        // An empty list because an active filter hid everything is a
        // user-driven narrowing, not first-run — no panel.
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.set_filters([Filter::Author]);
        assert!(!sb.is_getting_started());
    }

    #[test]
    fn non_inbox_mailbox_is_not_getting_started() {
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.mailbox = Mailbox::Snoozed;
        assert!(!sb.is_getting_started());
    }

    #[test]
    fn active_search_query_suppresses_getting_started() {
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.search = Some(SearchState {
            scope: None,
            query: "foo".into(),
            editing: true,
        });
        assert!(!sb.is_getting_started());
    }
}

mod work_target_tests {
    use super::super::*;

    fn ws_key(s: &str) -> SessionKey {
        (&lazybox_core::WorkspaceKey::new(s)).into()
    }

    fn spawn_agent(sb: &mut Sidebar, tid: u64, ws: &SessionKey, agent: &str) {
        sb.running_terminals.insert(
            TerminalId(tid),
            (ws.clone(), TerminalKind::Agent(agent.to_string())),
        );
    }

    fn running(tid: u64, agent: &str) -> RunningWorkTarget {
        RunningWorkTarget {
            terminal_id: TerminalId(tid),
            agent_id: agent.to_string(),
        }
    }

    #[test]
    fn no_running_agent_falls_back_to_default() {
        let sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        assert_eq!(
            sb.work_target(&ws, "claude"),
            WorkTarget::Spawn("claude".into())
        );
        assert!(sb.running_work_targets(&ws).is_empty());
    }

    #[test]
    fn single_running_agent_wins_over_default() {
        // The core bug fix (#418, regression of #224): only Codex is
        // running, so `w w` targets Codex (which
        // `rewrite_spawn_to_inject` then injects into) instead of
        // spawning the default Claude.
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "codex");
        assert_eq!(
            sb.work_target(&ws, "claude"),
            WorkTarget::Running(running(1, "codex"))
        );
        assert_eq!(sb.running_work_targets(&ws), vec![running(1, "codex")]);
    }

    #[test]
    fn several_agents_ask_even_when_default_is_among_them() {
        // #418: with several DIFFERENT agents running there is no right
        // guess — not even the default. The model mounts a chooser.
        // Ids come back sorted for a stable picker order.
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "codex");
        spawn_agent(&mut sb, 2, &ws, "claude");
        assert_eq!(
            sb.work_target(&ws, "claude"),
            WorkTarget::Choose(vec![running(2, "claude"), running(1, "codex")])
        );
    }

    #[test]
    fn several_non_default_agents_ask_too() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "cursor");
        spawn_agent(&mut sb, 2, &ws, "codex");
        assert_eq!(
            sb.work_target(&ws, "claude"),
            WorkTarget::Choose(vec![running(2, "codex"), running(1, "cursor")])
        );
    }

    #[test]
    fn running_agent_in_another_workspace_is_ignored() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        let other = ws_key("github:o/r#2");
        spawn_agent(&mut sb, 1, &other, "codex");
        assert_eq!(
            sb.work_target(&ws, "claude"),
            WorkTarget::Spawn("claude".into())
        );
        assert!(sb.running_work_targets(&ws).is_empty());
    }

    #[test]
    fn shell_terminals_are_not_agents() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        sb.running_terminals
            .insert(TerminalId(1), (ws.clone(), TerminalKind::Shell));
        assert_eq!(
            sb.work_target(&ws, "claude"),
            WorkTarget::Spawn("claude".into())
        );
        assert!(sb.running_work_targets(&ws).is_empty());
    }

    #[test]
    fn duplicate_agent_terminals_remain_distinct_targets() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "codex");
        spawn_agent(&mut sb, 2, &ws, "codex");
        assert_eq!(
            sb.work_target(&ws, "claude"),
            WorkTarget::Choose(vec![running(1, "codex"), running(2, "codex")])
        );
        assert_eq!(
            sb.work_target_for_agent(&ws, "codex"),
            WorkTarget::Choose(vec![running(1, "codex"), running(2, "codex")])
        );
    }
}
#[cfg(test)]
mod keep_awake_badge_tests {
    use super::super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn header_row(sb: &mut Sidebar) -> String {
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| sb.render(Rect::new(0, 0, 80, 20), f, true))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..80).map(|x| buf[(x, 0)].symbol()).collect::<String>()
    }

    fn working_agent(sb: &mut Sidebar) {
        let ws: SessionKey = (&lazybox_core::WorkspaceKey::new("github:o/r#1")).into();
        sb.agents.insert(ws, lazybox_ipc::AgentState::Working);
    }

    /// The badge paints exactly while the daemon's inhibitor holds:
    /// `ui.keep_awake` on AND ≥1 agent working. Either side alone
    /// paints nothing.
    #[test]
    fn awake_badge_requires_option_and_a_working_agent() {
        let mut sb = Sidebar::new(PaneId::new(1));
        working_agent(&mut sb);
        assert!(!header_row(&mut sb).contains("awake"));

        sb.set_keep_awake(true);
        assert!(header_row(&mut sb).contains("awake"));
    }

    #[test]
    fn awake_badge_clears_when_agents_go_idle() {
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.set_keep_awake(true);
        assert!(!header_row(&mut sb).contains("awake"));

        working_agent(&mut sb);
        assert!(header_row(&mut sb).contains("awake"));

        for state in sb.agents.values_mut() {
            *state = lazybox_ipc::AgentState::Done;
        }
        assert!(!header_row(&mut sb).contains("awake"));
    }
}

#[cfg(test)]
mod stack_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{TaskKind, Workspace};

    fn pr_ws(num: u64, head: &str, base: &str) -> Workspace {
        let mut t = base_task();
        t.id.key = format!("o/r#{num}");
        t.url = format!("https://github.com/o/r/pull/{num}");
        t.kind = Some(TaskKind::Pr);
        t.branch = Some(head.into());
        t.base_branch = Some(base.into());
        Workspace::from_task(t, chrono::Utc::now())
    }

    /// A recompute over a base==head chain links the child to its parent
    /// and reports each PR's `k/N` position (issue #969).
    #[test]
    fn recompute_stacks_links_child_to_parent() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let parent = pr_ws(1, "feat-a", "main");
        let child = pr_ws(2, "feat-b", "feat-a");
        let parent_key = SessionKey::from(&parent.key);
        let child_key = SessionKey::from(&child.key);
        sb.workspaces.insert(parent_key.clone(), parent);
        sb.workspaces.insert(child_key.clone(), child);
        sb.recompute_visible();

        let cs = sb.stack_info(&child_key).expect("child is stacked");
        assert_eq!((cs.position, cs.depth), (2, 2));
        assert_eq!(cs.parent.as_ref().and_then(|p| p.number()), Some(1));

        let ps = sb.stack_info(&parent_key).expect("parent has a child");
        assert_eq!((ps.position, ps.depth), (1, 2));
        assert_eq!(ps.children.len(), 1);
        assert!(ps.parent.is_none());
    }

    /// A PR based directly on the default branch with nothing stacked on
    /// it is not part of any stack, so `stack_info` returns `None`.
    #[test]
    fn standalone_pr_has_no_stack_info() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = pr_ws(1, "feat-a", "main");
        let key = SessionKey::from(&ws.key);
        sb.workspaces.insert(key.clone(), ws);
        sb.recompute_visible();
        assert!(sb.stack_info(&key).is_none());
    }
}

#[cfg(test)]
mod linear_team_repo_picker_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{Project, TaskId, Workspace};

    fn sidebar_tracking(repos: &[&str]) -> Sidebar {
        let mut sb = Sidebar::new(PaneId::new(1));
        let now = chrono::Utc::now();
        let mut projects = std::collections::BTreeMap::new();
        for repo in repos {
            let (owner, name) = repo.split_once('/').expect("owner/repo");
            let p = Project::github(owner, name, now);
            projects.insert(p.key.clone(), p);
        }
        sb.apply_projects(projects);
        sb
    }

    fn linear_ticket_linking(team: &str, id: &str, linked_repo: Option<&str>) -> Workspace {
        let mut t = base_task();
        t.id = TaskId {
            source: "linear".into(),
            key: id.into(),
        };
        t.repo = Some(format!("linear/{team}"));
        t.branch = None;
        t.linked_tasks = linked_repo
            .into_iter()
            .map(|repo| TaskId {
                source: "github".into(),
                key: format!("{repo}#42"),
            })
            .collect();
        Workspace::from_task(t, chrono::Utc::now())
    }

    /// #1041 (reopened) smart proposals: a repo that another ticket in the
    /// *same* Linear team already links a GitHub PR to floats to the top of
    /// the picker — the team's real repo, learned from its own tickets —
    /// while every tracked repo is still offered.
    #[test]
    fn ranks_repos_linked_by_sibling_team_tickets_first() {
        let mut sb = sidebar_tracking(&["obin-ai/obin-platform", "obin-ai/obin-infra"]);
        let ws = linear_ticket_linking("OBI", "OBI-1000", Some("obin-ai/obin-infra"));
        sb.workspaces.insert(SessionKey::from(&ws.key), ws);

        let ranked = sb.github_repos_ranked_for_linear_team("OBI");
        assert_eq!(
            ranked.first().map(String::as_str),
            Some("obin-ai/obin-infra"),
            "the repo a sibling ticket links floats first: {ranked:?}",
        );
        assert!(
            ranked.iter().any(|r| r == "obin-ai/obin-platform"),
            "the rest are still offered: {ranked:?}",
        );
    }

    /// A different team's linked repo must not reorder this team's picker —
    /// the signal is scoped to the team being mapped.
    #[test]
    fn ranking_ignores_other_teams_links() {
        let mut sb = sidebar_tracking(&["obin-ai/obin-platform", "obin-ai/obin-infra"]);
        let other = linear_ticket_linking("NYL", "NYL-1", Some("obin-ai/obin-infra"));
        sb.workspaces.insert(SessionKey::from(&other.key), other);

        // No OBI ticket links anything, so ordering stays the tracked order.
        let ranked = sb.github_repos_ranked_for_linear_team("OBI");
        assert_eq!(
            ranked,
            sb.github_repos_for_picker(),
            "another team's link must not reorder OBI's picker: {ranked:?}",
        );
    }

    /// With no signal at all the picker still lists every tracked repo —
    /// never a blank picker.
    #[test]
    fn ranks_all_repos_even_without_a_signal() {
        let sb = sidebar_tracking(&["obin-ai/obin-platform"]);
        assert_eq!(
            sb.github_repos_ranked_for_linear_team("OBI"),
            vec!["obin-ai/obin-platform".to_string()],
        );
    }

    #[test]
    fn quota_window_formats_percent_and_countdown() {
        use super::super::{format_quota_window, format_reset_countdown};
        use lazybox_ipc::QuotaWindow;

        // 4512 bp → 45%; reset 2h out → "45% · 2h".
        let window = QuotaWindow {
            utilization_bp: 4512,
            reset_at: Some(7_200),
        };
        assert_eq!(
            format_quota_window(Some(window), 0),
            Some("45% · 2h".to_string())
        );
        // No reset → bare percentage.
        let no_reset = QuotaWindow {
            utilization_bp: 6000,
            reset_at: None,
        };
        assert_eq!(
            format_quota_window(Some(no_reset), 0),
            Some("60%".to_string())
        );
        // Absent window → nothing.
        assert_eq!(format_quota_window(None, 0), None);
        // A reset already in the past means the window rolled over: the
        // utilization is stale, so the whole window is dropped (unknown) —
        // never reported as the pre-reset percentage.
        let past = QuotaWindow {
            utilization_bp: 9000,
            reset_at: Some(100),
        };
        assert_eq!(format_quota_window(Some(past), 500), None);
        // The exact-boundary tick counts as passed (reset "now" is over).
        let at_boundary = QuotaWindow {
            utilization_bp: 9000,
            reset_at: Some(500),
        };
        assert_eq!(format_quota_window(Some(at_boundary), 500), None);

        // Countdown units.
        assert_eq!(format_reset_countdown(45, 0), Some("45s".to_string()));
        assert_eq!(format_reset_countdown(120, 0), Some("2m".to_string()));
        assert_eq!(format_reset_countdown(7_200, 0), Some("2h".to_string()));
        assert_eq!(format_reset_countdown(172_800, 0), Some("2d".to_string()));
        assert_eq!(format_reset_countdown(0, 0), None);
    }
}

#[cfg(test)]
mod ticket_hierarchy_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{TaskId, TaskKind, Workspace};

    fn ticket(identifier: &str, parent: Option<&str>) -> Workspace {
        let mut task = base_task();
        task.id = TaskId {
            source: "linear".into(),
            key: identifier.into(),
        };
        task.repo = Some("linear/ENG".into());
        task.title = identifier.into();
        task.kind = Some(TaskKind::Issue);
        task.parent = parent.map(|key| TaskId {
            source: "linear".into(),
            key: key.into(),
        });
        Workspace::from_task(task, chrono::Utc::now())
    }

    #[test]
    fn space_on_parent_ticket_toggles_only_its_descendants() {
        let mut sidebar = Sidebar::new(PaneId::new(1));
        let parent = ticket("ENG-1", None);
        let child = ticket("ENG-2", Some("ENG-1"));
        let parent_key = SessionKey::from(&parent.key);
        let child_key = SessionKey::from(&child.key);
        sidebar.workspaces.insert(parent_key.clone(), parent);
        sidebar.workspaces.insert(child_key.clone(), child);
        sidebar.recompute_visible();

        let parent_row = sidebar
            .visible
            .iter()
            .position(|row| matches!(row, VisibleRow::Workspace(key) if key == &parent_key))
            .expect("parent row visible");
        sidebar.set_cursor(parent_row);
        assert!(sidebar.toggle_ticket_at_cursor());
        assert!(
            sidebar
                .visible
                .iter()
                .any(|row| matches!(row, VisibleRow::Workspace(key) if key == &parent_key))
        );
        assert!(
            !sidebar
                .visible
                .iter()
                .any(|row| matches!(row, VisibleRow::Workspace(key) if key == &child_key))
        );
        assert!(
            sidebar
                .cursor_ticket_tree()
                .expect("cursor stays on parent")
                .collapsed
        );

        assert!(sidebar.toggle_ticket_at_cursor());
        assert!(
            sidebar
                .visible
                .iter()
                .any(|row| matches!(row, VisibleRow::Workspace(key) if key == &child_key))
        );
    }

    #[test]
    fn rendered_ticket_tree_has_disclosure_and_indented_child() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut sidebar = Sidebar::new(PaneId::new(1));
        for workspace in [ticket("ENG-1", None), ticket("ENG-2", Some("ENG-1"))] {
            sidebar
                .workspaces
                .insert(SessionKey::from(&workspace.key), workspace);
        }
        sidebar.recompute_visible();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| sidebar.render(Rect::new(0, 0, 80, 20), frame, true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(
            screen.contains("▾ ENG-1"),
            "parent disclosure missing:\n{screen}"
        );
        assert!(
            screen.contains("  · ENG-2"),
            "child indentation missing:\n{screen}"
        );
    }
}
