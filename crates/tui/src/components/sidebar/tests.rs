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
mod status_pill_tests {
    use super::super::status_pill;
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};

    pub(super) fn base_task() -> Task {
        Task {
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
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    /// All pill labels render in a fixed 10-cell column so the time
    /// column lines up across rows. Regression-guard the width.
    #[test]
    fn every_pill_label_is_ten_cells_wide() {
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
            assert_eq!(
                pill.label.chars().count(),
                10,
                "label {:?} for {:?} is not 10 cells wide",
                pill.label,
                ci,
            );
        }
        let state_cases: &[TaskState] = &[TaskState::Draft, TaskState::Merged, TaskState::Closed];
        for state in state_cases {
            let mut t = base_task();
            t.state = *state;
            let pill = status_pill(&t).expect("state should produce a pill");
            assert_eq!(
                pill.label.chars().count(),
                10,
                "label {:?} for {:?} is not 10 cells wide",
                pill.label,
                state,
            );
        }
        // Approval pills.
        for ci in [CiStatus::Success, CiStatus::Running] {
            let mut t = base_task();
            t.review = ReviewStatus::Approved;
            t.ci = ci;
            let pill = status_pill(&t).expect("approval should produce a pill");
            assert_eq!(
                pill.label.chars().count(),
                10,
                "label {:?} for approval + {:?} is not 10 cells wide",
                pill.label,
                ci,
            );
        }
    }

    #[test]
    fn ci_failure_renders_ci_fail() {
        let mut t = base_task();
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CI FAIL  ");
    }

    #[test]
    fn ci_success_renders_ci_ok() {
        // New behaviour: CI passing now renders an explicit green
        // ` CI OK    ` pill instead of an empty status column.
        let mut t = base_task();
        t.ci = CiStatus::Success;
        let pill = status_pill(&t).expect("Success should produce a pill");
        assert_eq!(pill.label, " CI OK    ");
    }

    #[test]
    fn ci_running_renders_ci_run() {
        // New behaviour: Running was previously a barely-visible amber
        // fg `       CI `. Now it renders a yellow-bg ` CI RUN   ` pill
        // matching the FAIL / MIX styling so users actually see it.
        let mut t = base_task();
        t.ci = CiStatus::Running;
        assert_eq!(status_pill(&t).unwrap().label, " CI RUN   ");
        t.ci = CiStatus::Pending;
        assert_eq!(status_pill(&t).unwrap().label, " CI RUN   ");
    }

    #[test]
    fn ci_mixed_renders_ci_mix() {
        let mut t = base_task();
        t.ci = CiStatus::Mixed;
        assert_eq!(status_pill(&t).unwrap().label, " CI MIX   ");
    }

    #[test]
    fn conflicts_trump_ci_status() {
        let mut t = base_task();
        t.mergeable = lazybox_core::Mergeable::Conflicting;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " CONFLICT ");
    }

    #[test]
    fn merged_renders_merged_pill_overriding_ci() {
        // A closed PR's CI history is frozen; the user can't act on
        // it. Show the inactive-state badge instead of a stale
        // CI FAIL.
        let mut t = base_task();
        t.state = TaskState::Merged;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " MERGED   ");
    }

    #[test]
    fn closed_renders_closed_pill_overriding_ci() {
        let mut t = base_task();
        t.state = TaskState::Closed;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CLOSED   ");
    }

    #[test]
    fn draft_renders_draft_pill_when_ci_is_quiet() {
        // CI green or running, state Draft → DRAFT wins so the user
        // remembers the PR isn't ready for review.
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " DRAFT    ");
    }

    #[test]
    fn ci_failure_beats_draft() {
        // A draft with red CI still needs the user's attention more
        // urgently than the draft state itself — CI FAIL wins.
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CI FAIL  ");
    }

    #[test]
    fn ci_none_with_no_conflicts_renders_no_pill() {
        let t = base_task();
        assert!(status_pill(&t).is_none());
    }

    #[test]
    fn approved_plus_green_ci_renders_ready() {
        // The "this is mergeable right now" signal — both the human
        // half (review) and the machine half (CI) are done.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " READY    ");
    }

    #[test]
    fn approved_with_no_ci_yet_still_renders_ready() {
        // Some repos don't run CI on every PR (or the rollup is still
        // empty after a fresh push). Approval alone is enough to call
        // it READY rather than holding back forever.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::None;
        assert_eq!(status_pill(&t).unwrap().label, " READY    ");
    }

    #[test]
    fn approved_with_running_ci_renders_approved() {
        // Human approval landed; CI is still chewing. The user can
        // safely walk away — once green, the PR is mergeable.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Running;
        assert_eq!(status_pill(&t).unwrap().label, " APPROVED ");
    }

    #[test]
    fn ci_failure_overrides_approval() {
        // Approval is great but red CI still trumps — that's the
        // actionable problem.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CI FAIL  ");
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
    //!   skip `ChangesRequested` / `Queued` / `AutoMerge` / `ReviewPending`
    //!   entirely).
    //!
    //! - Every pill label is the same 10-cell width so the time
    //!   column stays right-aligned across rows.
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
        StatusTag::AutoMerge,
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
                StatusTag::AutoMerge => (),
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
    fn every_pill_label_is_ten_cells_wide() {
        // The right-trailer reserves 10 cells for the status pill
        // so the time column stays aligned. Width is checked here
        // for every tag, not just the ones reachable from a Task —
        // the renderer is the truth, the producer is the input.
        for tag in ALL_TAGS {
            if let Some(p) = pill_for_tag(*tag) {
                assert_eq!(
                    p.label.chars().count(),
                    10,
                    "StatusTag::{tag:?} label {:?} is not 10 cells wide",
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
        assert_eq!(pill.label, " CHANGES  ");
    }

    #[test]
    fn auto_merge_now_renders_a_pill() {
        // Same bug class: auto_merge_enabled with no other signal
        // used to produce no pill. Now renders AUTO.
        let mut t = base_task();
        t.auto_merge_enabled = true;
        let pill = status_pill(&t).expect("auto-merge must produce a pill");
        assert_eq!(pill.label, " AUTO     ");
    }

    #[test]
    fn queued_now_renders_a_pill() {
        let mut t = base_task();
        t.is_in_merge_queue = true;
        let pill = status_pill(&t).expect("in-merge-queue must produce a pill");
        assert_eq!(pill.label, " QUEUED   ");
    }

    #[test]
    fn review_pending_now_renders_a_pill() {
        let mut t = base_task();
        t.review = ReviewStatus::Pending;
        let pill = status_pill(&t).expect("review-pending must produce a pill");
        assert_eq!(pill.label, " REVIEW   ");
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

    fn empty_set() -> std::collections::HashSet<SessionKey> {
        std::collections::HashSet::new()
    }

    fn set_with(ws: &Workspace) -> std::collections::HashSet<SessionKey> {
        let mut s = std::collections::HashSet::new();
        s.insert(SessionKey::from(&ws.key));
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
    fn agent_asking_signal_comes_from_asking_set_not_workspace_sessions() {
        // Regression for the silent-clobber bug fixed in this
        // commit: the AgentAsking signal MUST be driven by the
        // sidebar-local `agents_asking` set, NOT
        // `Workspace.sessions[i].state`. The poll cycle reloads
        // workspace data from store every minute, which would
        // wipe a state-mutation-based signal.
        let w = ws_from_pr(base_task());

        // No entry in the set → no signal even if sessions claim
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
mod role_filter_tests {
    use super::super::*;
    use super::status_pill_tests::base_task;
    use lazybox_core::{TaskRole, Workspace};

    fn ws_with_role(key: &str, role: TaskRole) -> Workspace {
        let mut t = base_task();
        t.id.key = key.into();
        t.url = format!("https://github.com/o/r/pull/{key}");
        t.role = role;
        Workspace::from_task(t, chrono::Utc::now())
    }

    #[test]
    fn role_filter_default_is_all() {
        assert_eq!(RoleFilter::default(), RoleFilter::All);
    }

    #[test]
    fn role_filter_cycles_through_every_variant_and_wraps() {
        // Five variants: All → Author → Reviewer → Assignee →
        // Mentioned → All. Walk the full loop to lock the order in.
        let order = [
            RoleFilter::All,
            RoleFilter::Author,
            RoleFilter::Reviewer,
            RoleFilter::Assignee,
            RoleFilter::Mentioned,
            RoleFilter::All,
        ];
        let mut cur = RoleFilter::All;
        for expected_next in &order[1..] {
            cur = cur.next();
            assert_eq!(cur, *expected_next);
        }
    }

    #[test]
    fn all_filter_accepts_every_role_and_orphans() {
        for role in [
            Some(TaskRole::Author),
            Some(TaskRole::Reviewer),
            Some(TaskRole::Assignee),
            Some(TaskRole::Mentioned),
            None,
        ] {
            assert!(RoleFilter::All.accepts(role));
        }
    }

    #[test]
    fn author_filter_only_accepts_author_role() {
        assert!(RoleFilter::Author.accepts(Some(TaskRole::Author)));
        assert!(!RoleFilter::Author.accepts(Some(TaskRole::Reviewer)));
        assert!(!RoleFilter::Author.accepts(Some(TaskRole::Assignee)));
        assert!(!RoleFilter::Author.accepts(Some(TaskRole::Mentioned)));
        // Orphan workspaces (no primary task) fail any non-All filter
        // — role lives on the task and there's nothing to compare.
        assert!(!RoleFilter::Author.accepts(None));
    }

    #[test]
    fn cycle_role_filter_drops_unrelated_workspaces_from_visible_list() {
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
        assert_eq!(sb.workspace_count(), 4, "all four show under `all`");

        sb.cycle_role_filter(); // All → Author
        assert_eq!(sb.role_filter(), RoleFilter::Author);
        assert_eq!(sb.workspace_count(), 1, "author filter → 1 row");

        sb.cycle_role_filter(); // Author → Reviewer
        assert_eq!(sb.workspace_count(), 1);

        // Walk all the way back to All.
        for _ in 0..3 {
            sb.cycle_role_filter();
        }
        assert_eq!(sb.role_filter(), RoleFilter::All);
        assert_eq!(sb.workspace_count(), 4);
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
    fn chip_label_is_short_enough_for_the_header_row() {
        // The chip renders into row 1 alongside the `f ` prefix and a
        // dim cycle hint. Cap each label at 10 cells so layout never
        // overflows the typical 30-column sidebar.
        for f in [
            RoleFilter::All,
            RoleFilter::Author,
            RoleFilter::Reviewer,
            RoleFilter::Assignee,
            RoleFilter::Mentioned,
        ] {
            assert!(
                f.chip_label().chars().count() <= 10,
                "chip label `{}` exceeds 10 cells",
                f.chip_label()
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
        assert_eq!(s.scope, "o/r");
        assert!(s.query.is_empty());
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

    /// Enter with an empty query just closes the bar (nothing to keep).
    #[test]
    fn enter_on_empty_query_closes_bar() {
        let mut sb = sidebar_with_issues(&[("1", "Alpha")]);
        sb.open_search();
        sb.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(sb.search().is_none());
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
}

#[cfg(test)]
mod working_spinner_tests {
    use super::super::*;
    use lazybox_core::WorkspaceKey;
    use std::time::{Duration, Instant};

    fn working_sidebar() -> Sidebar {
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.agents_working
            .insert(SessionKey::from(&WorkspaceKey::new("owner/repo#1")));
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

        // Flap to idle: the working-set empties for a beat.
        sb.agents_working.clear();
        assert!(!sb.tick_working(), "idle asks for no spinner redraw");
        assert_eq!(
            sb.working_spinner_frame, before,
            "frame is not reset to 0 while idle",
        );

        // Working again a little later — the frame reflects the clock,
        // strictly ahead of where it was, never restarting at 0.
        sb.agents_working
            .insert(SessionKey::from(&WorkspaceKey::new("owner/repo#1")));
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
            !sb.contextual_bindings(&catalog)
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
            sb.contextual_bindings(&catalog)
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
        assert!(sb.agents_done.contains(&key), "done-set holds the key");
        assert!(
            !sb.agents_working.contains(&key),
            "done is disjoint from working",
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
        assert!(sb.agents_done.contains(&key));
        sb.on_event(&agent_state(&key, AgentState::Working));
        assert!(!sb.agents_done.contains(&key));
        assert!(sb.agents_working.contains(&key));
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
    //! Issue #205: combining an issue into a PR (`Shift-J`) rebadges the
    //! issue's live terminals onto the PR. The sidebar's transient
    //! agent-state sets are keyed by session, so a `TerminalsRebadged`
    //! must migrate them — otherwise an agent parked on a prompt (which
    //! the daemon never re-broadcasts) keeps its `?` pill pinned to the
    //! deleted issue key and the PR row shows no badge, reading as lost.
    use super::super::*;
    use lazybox_core::WorkspaceKey;
    use lazybox_ipc::{AgentState, Event, TerminalId};

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
        assert!(sb.agents_asking.contains(&issue));

        // Collapse: the daemon rebadges the terminal onto the PR. No
        // further AgentState follows — the agent is stalled.
        sb.on_event(&Event::TerminalsRebadged {
            from: issue.clone(),
            to: pr.clone(),
        });

        assert!(
            !sb.agents_asking.contains(&issue),
            "the dead issue key must be dropped",
        );
        assert!(
            sb.agents_asking.contains(&pr),
            "the PR must inherit the asking pill so the agent stays visible",
        );
    }

    #[test]
    fn rebadge_migrates_working_and_done_sets_too() {
        let issue: SessionKey = (&WorkspaceKey::new("github:o/r#60")).into();
        let pr: SessionKey = (&WorkspaceKey::new("github:o/r#61")).into();

        for state in [AgentState::Working, AgentState::Done] {
            let mut sb = Sidebar::new(PaneId::new(1));
            sb.on_event(&agent_state(&issue, state));
            sb.on_event(&Event::TerminalsRebadged {
                from: issue.clone(),
                to: pr.clone(),
            });
            let set = match state {
                AgentState::Working => &sb.agents_working,
                AgentState::Done => &sb.agents_done,
                _ => unreachable!(),
            };
            assert!(!set.contains(&issue), "{state:?}: issue key dropped");
            assert!(set.contains(&pr), "{state:?}: PR key inherited");
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
        assert!(!sb.agents_asking.contains(&pr));
        assert!(!sb.agents_working.contains(&pr));
        assert!(!sb.agents_done.contains(&pr));
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
    fn role_filtered_empty_view_is_not_getting_started() {
        // An empty list because a role filter hid everything is a
        // user-driven narrowing, not first-run — no panel.
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.role_filter = RoleFilter::Author;
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
            scope: String::new(),
            query: "foo".into(),
            editing: true,
        });
        assert!(!sb.is_getting_started());
    }
}

mod work_target_agent_tests {
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

    #[test]
    fn no_running_agent_falls_back_to_default() {
        let sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        assert_eq!(sb.work_target_agent(&ws, "claude"), "claude");
        assert!(sb.running_agent_ids(&ws).is_empty());
    }

    #[test]
    fn single_running_agent_wins_over_default() {
        // The core bug fix: only Codex is running, so bare `w` targets
        // Codex (which `rewrite_spawn_to_inject` then injects into)
        // instead of spawning the default Claude.
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "codex");
        assert_eq!(sb.work_target_agent(&ws, "claude"), "codex");
        assert_eq!(sb.running_agent_ids(&ws), vec!["codex".to_string()]);
    }

    #[test]
    fn default_agent_wins_when_among_several() {
        // Tie-break: with several different agents running, prefer the
        // default if it's one of them.
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "codex");
        spawn_agent(&mut sb, 2, &ws, "claude");
        assert_eq!(sb.work_target_agent(&ws, "claude"), "claude");
    }

    #[test]
    fn multiple_non_default_agents_fall_back_to_default() {
        // Tie-break: several non-default agents and no default running →
        // the bare `w` outcome stays predictable (the default, a fresh
        // spawn). The scoped `w c` / `w x` chords pick a specific one.
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "codex");
        spawn_agent(&mut sb, 2, &ws, "cursor");
        assert_eq!(sb.work_target_agent(&ws, "claude"), "claude");
    }

    #[test]
    fn running_agent_in_another_workspace_is_ignored() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        let other = ws_key("github:o/r#2");
        spawn_agent(&mut sb, 1, &other, "codex");
        assert_eq!(sb.work_target_agent(&ws, "claude"), "claude");
        assert!(sb.running_agent_ids(&ws).is_empty());
    }

    #[test]
    fn shell_terminals_are_not_agents() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        sb.running_terminals
            .insert(TerminalId(1), (ws.clone(), TerminalKind::Shell));
        assert_eq!(sb.work_target_agent(&ws, "claude"), "claude");
        assert!(sb.running_agent_ids(&ws).is_empty());
    }

    #[test]
    fn duplicate_agent_terminals_dedupe_to_one_id() {
        let mut sb = Sidebar::new(PaneId::new(1));
        let ws = ws_key("github:o/r#1");
        spawn_agent(&mut sb, 1, &ws, "codex");
        spawn_agent(&mut sb, 2, &ws, "codex");
        assert_eq!(sb.running_agent_ids(&ws), vec!["codex".to_string()]);
        assert_eq!(sb.work_target_agent(&ws, "claude"), "codex");
    }
}

#[cfg(test)]
mod outdated_build_tests {
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

    /// A current build paints no warning; a stale one paints the
    /// persistent header banner naming the fix. The header is the
    /// always-visible surface a uniformly-stale install can't scroll
    /// past (#234). Sets the outdated flag directly to test the render
    /// path — the provenance gate that decides whether the flag is ever
    /// set lives in `build_guard`/`check_build_freshness` (#251).
    #[test]
    fn header_shows_outdated_warning_only_when_behind() {
        let mut sb = Sidebar::new(PaneId::new(1));
        assert!(!header_row(&mut sb).contains("behind"));

        sb.set_outdated_build(Some(89));
        let row = header_row(&mut sb);
        assert!(row.contains("89"), "header row was: {row:?}");
        assert!(row.contains("update & restart"), "header row was: {row:?}");
    }

    /// Zero commits behind is current, not stale — normalize it away so
    /// a `Some(0)` from the guard never paints a phantom warning.
    #[test]
    fn zero_behind_is_not_outdated() {
        let mut sb = Sidebar::new(PaneId::new(1));
        sb.set_outdated_build(Some(0));
        assert_eq!(sb.outdated_commits_behind(), None);
        assert!(!header_row(&mut sb).contains("behind"));
    }
}
