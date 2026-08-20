//! Protocol-level tests: serde round-trip every `Command` / `Event`
//! variant, plus framing over a real tokio duplex pair.
//!
//! These tests exist to catch *silent* wire-format breakage. If anyone
//! renames a variant or reorders a field, bincode's tagging changes and
//! one of these assertions blows up. Far better than finding out when
//! a v0.2 client can't talk to a v0.3 daemon.

use lazybox_ipc::{
    AgentApprovalDecision, AgentInputMessage, AgentQuestionAnswer, AgentRunId, AgentRunRequestId,
    AgentRuntimeMode, AgentState, AgentUsage, Command, Event, HookEvent, HookEventKind,
    PrincipalId, PromptSource, ProviderCredentialInput, ProviderCredentialMetadata, ProviderQuota,
    QuotaWindow, RemovableTerminalState, SpawnFallback, TerminalId, TerminalInputIntent,
    TerminalKind, TerminalSnapshot, UserPrompt, WorktreeStep, WorktreeStepStatus,
};
use tokio::io::duplex;

fn sample_time() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
        .expect("valid fixture timestamp")
        .with_timezone(&chrono::Utc)
}

fn sample_session_id(value: u128) -> lazybox_core::SessionId {
    lazybox_core::SessionId(uuid::Uuid::from_u128(value))
}

fn sample_workspace() -> lazybox_core::Workspace {
    let task = lazybox_core::Task {
        id: lazybox_core::TaskId {
            source: "github".into(),
            key: "o/r#1".into(),
        },
        title: "t".into(),
        body: None,
        state: lazybox_core::TaskState::Open,
        role: lazybox_core::TaskRole::Author,
        ci: lazybox_core::CiStatus::None,
        review: lazybox_core::ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: "https://github.com/o/r/pull/1".into(),
        repo: Some("o/r".into()),
        branch: Some("b".into()),
        base_branch: None,
        updated_at: sample_time(),
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        reviews: vec![],
        assignees: vec![],
        author: String::new(),
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
    };
    lazybox_core::Workspace::from_task(task, sample_time())
}

fn sample_project() -> lazybox_core::Project {
    lazybox_core::Project::new(
        lazybox_core::ProjectKey::github("o", "r"),
        "o/r",
        sample_time(),
    )
}

fn all_commands() -> Vec<Command> {
    let key: lazybox_core::SessionKey = "github:o/r#1".into();
    let principal_id = PrincipalId::local();
    vec![
        Command::Subscribe,
        Command::Spawn {
            session_key: key.clone(),
            session_id: None,
            client_request_id: Some("spawn-1".into()),
            kind: TerminalKind::Agent("claude".into()),
            cwd: Some("/tmp".into()),
            initial_prompt: None,
            initial_snippet: None,
            on_main: false,
            model_alias: Some("L".into()),
            access: lazybox_ipc::AgentRunAccess::ReadOnly,
        },
        Command::Spawn {
            session_key: key.clone(),
            session_id: Some(sample_session_id(1)),
            client_request_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
            initial_prompt: None,
            initial_snippet: None,
            on_main: true,
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
        },
        Command::Spawn {
            session_key: key.clone(),
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::LogTail {
                path: "/var/log/x.log".into(),
            },
            cwd: None,
            initial_prompt: Some("fix the failing CI".into()),
            initial_snippet: Some(Box::new(lazybox_ipc::SnippetRef {
                key: "rev".into(),
                category: "review".into(),
            })),
            on_main: false,
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
        },
        Command::CancelSpawn {
            session_key: key.clone(),
        },
        Command::CreateSession {
            session_key: key.clone(),
            kind: TerminalKind::Shell,
            label: Some("review".into()),
        },
        Command::Write {
            terminal_id: TerminalId(7),
            bytes: b"hello\n".to_vec(),
            intent: TerminalInputIntent::Submit,
        },
        Command::Resize {
            terminal_id: TerminalId(7),
            cols: 120,
            rows: 40,
        },
        Command::RequestTerminalResync {
            requests: vec![lazybox_ipc::TerminalResyncRequest {
                terminal_id: TerminalId(7),
                required_seq: 42,
            }],
        },
        Command::RequestTerminalDelta {
            terminal_id: TerminalId(7),
            since_offset: 4096,
        },
        Command::Close {
            terminal_id: TerminalId(7),
            client_request_id: Some("close-1".into()),
        },
        Command::StartAgentRun {
            request_id: AgentRunRequestId("request-1".into()),
            session_key: key.clone(),
            session_id: Some(sample_session_id(2)),
            source_terminal_id: Some(TerminalId(7)),
            agent: "claude".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: Some("/tmp/worktree".into()),
            initial_input: Some(AgentInputMessage {
                text: Some("review this diff".into()),
                json: Some(r#"{"type":"user","message":"review this diff"}"#.into()),
            }),
            resume_latest: true,
            access: lazybox_ipc::AgentRunAccess::Default,
        },
        Command::StartAgentRun {
            request_id: AgentRunRequestId("request-2".into()),
            session_key: key.clone(),
            session_id: None,
            source_terminal_id: None,
            agent: "claude".into(),
            mode: AgentRuntimeMode::Terminal,
            cwd: None,
            initial_input: None,
            resume_latest: false,
            access: lazybox_ipc::AgentRunAccess::Default,
        },
        Command::SendAgentInput {
            run_id: AgentRunId(9),
            message: AgentInputMessage {
                text: Some("continue".into()),
                json: None,
            },
        },
        Command::InterruptAgentRun {
            run_id: AgentRunId(9),
        },
        Command::DecideAgentApproval {
            run_id: AgentRunId(9),
            request_id: "perm-1".into(),
            decision: AgentApprovalDecision::Approve,
        },
        Command::DecideAgentApproval {
            run_id: AgentRunId(9),
            request_id: "perm-2".into(),
            decision: AgentApprovalDecision::Deny {
                reason: Some("outside workspace".into()),
            },
        },
        Command::AnswerAgentQuestion {
            run_id: AgentRunId(9),
            question_id: "q-1".into(),
            answer: AgentQuestionAnswer {
                answer: "use the existing style".into(),
            },
        },
        Command::UpsertProviderCredential {
            principal_id: principal_id.clone(),
            credential: ProviderCredentialInput {
                provider_id: "github".into(),
                token: "secret-token".into(),
                source: "unit-test".into(),
                scopes: vec!["repo".into()],
                expires_at: None,
            },
        },
        Command::RemoveProviderCredential {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
        },
        Command::ListProviderCredentials {
            principal_id: principal_id.clone(),
        },
        Command::RecordUserMessage {
            terminal_id: TerminalId(7),
            prompt: lazybox_ipc::UserPrompt {
                text: "fix the flaky test".into(),
                timestamp_ms: 1_700_000_000_000,
                source: lazybox_ipc::PromptSource::Snippet {
                    key: "test".into(),
                    category: "CI".into(),
                },
            },
        },
        Command::InjectPrompt {
            terminal_id: TerminalId(7),
            prompt: "review this diff".into(),
            fallback_spawn: Some(SpawnFallback {
                session_key: key.clone(),
                session_id: None,
                client_request_id: Some("fallback-1".into()),
                kind: TerminalKind::Agent("codex".into()),
                cwd: Some("/tmp/worktree".into()),
                model_alias: Some("L".into()),
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
            }),
            submit: false,
        },
        Command::RecordComposingBuffer {
            terminal_id: TerminalId(7),
            buffer: "half typed".into(),
        },
        Command::IngestHook {
            terminal_id: TerminalId(7),
            hook: HookEvent {
                kind: HookEventKind::PermissionRequest,
                session_id: Some("agent-session".into()),
                cwd: Some("/tmp/worktree".into()),
                tool_name: Some("Bash".into()),
                notification: Some("permission_prompt".into()),
            },
            backend_key: Some("tmux-key".into()),
        },
        Command::Kill {
            session_key: key.clone(),
        },
        Command::RemoveMergedWorkspace {
            session_key: key.clone(),
        },
        Command::DeleteProject {
            project_key: lazybox_core::ProjectKey::github("o", "r"),
        },
        Command::CollapseIntoPr {
            issue_workspace_key: key.clone(),
        },
        Command::MarkRead {
            session_key: key.clone(),
        },
        Command::FocusWorkspace {
            session_key: key.clone(),
        },
        Command::ActivateWorkspace {
            session_key: key.clone(),
        },
        Command::MarkActivityRead {
            session_key: key.clone(),
            index: 2,
            fingerprint: Some(lazybox_core::ActivityFingerprint::NodeId(
                "IC_kwDOtest123".into(),
            )),
        },
        Command::UnmarkActivityRead {
            session_key: key.clone(),
            index: 2,
        },
        Command::CreateWorkspace {
            name: "audit-wire".into(),
            project_key: lazybox_core::ProjectKey::github("o", "r"),
            spawn_agent: Some("codex".into()),
            client_request_id: Some("create-audit-wire".into()),
        },
        Command::CreateProject {
            name: "local project".into(),
        },
        Command::SetSessionLayout {
            session_key: key.clone(),
            session_id_raw: "00000000-0000-0000-0000-000000000001".into(),
            layout_json: r#"{"mode":"tabs"}"#.into(),
        },
        Command::Snooze {
            session_key: key.clone(),
            until: sample_time() + chrono::Duration::hours(4),
        },
        Command::Unsnooze {
            session_key: key.clone(),
        },
        Command::SetAutoMergeOnGreen {
            session_key: key.clone(),
            enabled: true,
        },
        Command::SetTrackMain {
            session_key: key.clone(),
            enabled: true,
        },
        Command::SetAutoFixPolicy {
            session_key: key.clone(),
            kind: lazybox_core::AutoFixKind::CiFailure,
            arm: lazybox_core::PolicyArm::Arm,
        },
        Command::SetAutoFixPolicies {
            session_key: key.clone(),
            ci: lazybox_core::PolicyArm::Arm,
            conflict: lazybox_core::PolicyArm::Disarm,
        },
        Command::PostReply {
            session_key: key.clone(),
            body: "ship it".into(),
        },
        Command::ConfirmMerge {
            issue_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            accept: true,
        },
        Command::AdoptSessions {
            source_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            target_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Command::MergePr {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Command::CloseIssue {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
        },
        Command::DeleteOrClose {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
        },
        Command::RequestReviewers {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            logins: vec!["octocat".into()],
        },
        Command::AddAssignees {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            logins: vec!["octocat".into()],
        },
        Command::SetAssignees {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            logins: vec![],
        },
        Command::SetLabels {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            names: vec!["bug".into()],
        },
        Command::FetchRepoLabels {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Command::FetchRequestableReviewers {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Command::Refresh,
        Command::CleanWorktrees,
        Command::InspectWorktrees,
        Command::ScanCheckouts { roots: vec![] },
        Command::ScanCheckouts {
            roots: vec![std::path::PathBuf::from("/home/dev/code")],
        },
        Command::ImportLocalCheckout {
            path: std::path::PathBuf::from("/home/dev/code/acme/widget"),
            spawn_agent: Some("claude".into()),
        },
        Command::ImportLocalCheckout {
            path: std::path::PathBuf::from("/home/dev/code/acme/widget"),
            spawn_agent: None,
        },
        Command::DeleteOrphanedWorktree {
            path: std::path::PathBuf::from("/tmp/wt"),
            force: false,
        },
        Command::DeleteOrphanedWorktree {
            path: std::path::PathBuf::from("/tmp/wt-dirty"),
            force: true,
        },
        Command::FetchPrDetails {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Command::SyncWorkspace {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Command::InspectWorkspaceDiff {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            target: lazybox_ipc::WorkspaceDiffTarget::Session(sample_session_id(21)),
        },
        Command::KeepMergedWorkspace { session_key: key },
        Command::FetchScrollback {
            terminal_id: TerminalId(12),
        },
        Command::CheckAgentCliUpdates,
        Command::UpdateAgentClis,
        Command::SetNotes {
            session_key: "github:o/r#1".into(),
            notes: "check the flaky retry".into(),
        },
        Command::UpdateBranch {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Command::DeliverSnippet {
            terminal_id: TerminalId(12),
            snippet_key: "rev".into(),
            category: "Review".into(),
            body: "review the diff".into(),
            submit: true,
        },
        Command::SetUpdateDismissal {
            target: "release:v0.2.0".into(),
        },
        Command::ResumeAgent {
            terminal_id: TerminalId(12),
        },
        Command::ReauthenticateAgent {
            terminal_id: TerminalId(12),
            switch_account: true,
        },
        Command::CancelAgentReauthentication {
            terminal_id: TerminalId(12),
        },
        Command::RenameWorkspace {
            session_key: "github:o/r#1".into(),
            name: "spike-rate-limit".into(),
        },
        Command::RecreateWorktree {
            spawn: Box::new(lazybox_ipc::SpawnFallback {
                session_key: "github:o/r#1".into(),
                session_id: None,
                client_request_id: Some("recreate-1".into()),
                kind: TerminalKind::Agent("claude".into()),
                cwd: Some("/tmp".into()),
                model_alias: Some("L".into()),
                access: lazybox_ipc::AgentRunAccess::ReadOnly,
            }),
            initial_prompt: Some("fix the thing".into()),
            on_main: false,
            preserve_holder: Some("/tmp/holder".into()),
        },
        Command::ListErrors,
        Command::ClearErrors,
        Command::DeleteError {
            dedupe_key: "github|merge|rate limited".into(),
        },
        Command::GetResourcePosture,
        Command::RecoverAgentCredit {
            terminal_id: TerminalId(12),
            client_request_id: "credit-recovery-1".into(),
            continuation_prompt: "Continue the work you were doing.".into(),
        },
        Command::Shutdown,
    ]
}

fn all_events() -> Vec<Event> {
    let key: lazybox_core::SessionKey = "github:o/r#1".into();
    let principal_id = PrincipalId::local();
    let credential_metadata = ProviderCredentialMetadata {
        principal_id: principal_id.clone(),
        provider_id: "github".into(),
        source: "unit-test".into(),
        scopes: vec!["repo".into()],
        updated_at: sample_time(),
        expires_at: None,
    };
    vec![
        Event::Snapshot {
            workspaces: vec![sample_workspace()],
            terminals: vec![TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: key.clone(),
                kind: TerminalKind::Agent("claude".into()),
                replay: b"replay-bytes".to_vec(),
                last_seq: 42,
                replay_available: true,
                no_permission: true,
                on_main: true,
                model_label: Some("Opus".into()),
                prompt_history: vec![lazybox_ipc::UserPrompt {
                    text: "fix the flaky test".into(),
                    timestamp_ms: 1_700_000_000_000,
                    source: lazybox_ipc::PromptSource::Typed,
                }],
                composing_buffer: Some("half typed prompt".into()),
                agent_state: Some(AgentState::InputNeeded),
                authenticating: false,
            }],
            projects: vec![],
            recent_snippets: vec!["rev".into(), "pr".into()],
            dismissed_updates: vec!["release:v0.2.0".into()],
        },
        Event::ViewerIdentities {
            logins: vec![("github".into(), "octocat".into())],
        },
        Event::AutoFixPolicyConfig {
            enabled: true,
            opt_out_labels: vec!["no-auto-fix".into(), "do-not-lazybox".into()],
        },
        Event::ShellCommandConfig {
            command: "/bin/zsh".into(),
            configured: false,
        },
        Event::AgentAvailabilityConfig {
            agents: vec!["claude".into(), "codex".into()],
            default_agent: Some("codex".into()),
        },
        Event::WorkspaceUpserted(std::sync::Arc::new(sample_workspace())),
        Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(key.as_str())),
        Event::ProjectUpserted(Box::new(sample_project())),
        Event::ProjectRemoved(lazybox_core::ProjectKey::github("o", "r")),
        Event::WorkspaceOutOfScope {
            workspace_key: lazybox_core::WorkspaceKey::new(key.as_str()),
            label: "o/r#1".into(),
            title: Some("wire audit".into()),
            active_terminal_count: 2,
        },
        Event::WorkspaceMergePending {
            issue_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            issue_label: "o/r#1".into(),
            pr_label: "o/r#2".into(),
            active_terminal_count: 1,
        },
        Event::WorkspaceMerged {
            issue_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            pr_workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            issue_label: "o/r#1".into(),
            pr_label: "o/r#2".into(),
        },
        Event::PrMerged {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            pr_label: "o/r#2".into(),
        },
        Event::PrMergeFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            pr_label: "o/r#2".into(),
            reason: "required review".into(),
            conflict: false,
        },
        Event::IssueClosed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            issue_label: "o/r#1".into(),
        },
        Event::IssueCloseFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            issue_label: "o/r#1".into(),
            reason: "permission denied".into(),
        },
        Event::PrClosed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            pr_label: "o/r#2".into(),
        },
        Event::IssueDeleted {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            issue_label: "o/r#1".into(),
            fell_back_to_close: true,
        },
        Event::DeleteOrCloseFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
            label: "o/r#1".into(),
            reason: "permission denied".into(),
        },
        Event::MergedPrRemovable {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            label: "o/r#2".into(),
            terminal_state: RemovableTerminalState::Merged,
            active_terminal_count: 1,
            has_local_work: true,
        },
        Event::RemovalCancelled {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
        },
        Event::RepoLabels {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            labels: vec![lazybox_core::Label::with_color("bug", "d73a4a")],
        },
        Event::RequestableReviewers {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            logins: vec!["octocat".into()],
        },
        {
            let mut session = lazybox_core::WorkspaceSession::new(
                lazybox_core::WorkspaceKey::new(key.as_str()),
                lazybox_core::SessionKind::Shell,
                std::path::PathBuf::from("/tmp/wt"),
                sample_time(),
            );
            session.id = sample_session_id(5);
            Event::SessionCreated(Box::new(session))
        },
        Event::WorktreeProgress {
            session_key: key.clone(),
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Warned("using cached base".into()),
            origin: lazybox_ipc::SpawnOrigin::Interactive,
        },
        Event::WorktreeProgress {
            session_key: key.clone(),
            step: WorktreeStep::Clone,
            status: WorktreeStepStatus::Progress("Receiving objects: 42% (1200/2900)".into()),
            origin: lazybox_ipc::SpawnOrigin::Autonomous(lazybox_ipc::AutonomousTrigger::Label),
        },
        Event::SessionEnded {
            workspace_key: lazybox_core::WorkspaceKey::new(key.as_str()),
            session_id: sample_session_id(3),
        },
        Event::TerminalSpawned {
            terminal_id: TerminalId(2),
            session_key: key.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
            model_label: None,
        },
        Event::TerminalReplaced {
            old_terminal_id: TerminalId(2),
            terminal_id: TerminalId(3),
            session_key: key.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: true,
            model_label: Some("Large".into()),
            authenticating: true,
        },
        Event::TerminalOutput {
            terminal_id: TerminalId(2),
            bytes: b"ANSI: \x1b[31mred\x1b[0m".to_vec(),
            first_seq: 1,
            seq: 1,
        },
        Event::AgentAuthOutput {
            terminal_id: TerminalId(3),
            bytes: b"provider login".to_vec(),
            first_seq: 1,
            seq: 1,
        },
        Event::AgentAuthReplay {
            terminal_id: TerminalId(3),
            replay: b"provider login replay".to_vec(),
            seq: 2,
        },
        Event::TerminalResync {
            terminal_id: TerminalId(2),
            replay: b"full replay".to_vec(),
            seq: 9,
        },
        Event::TerminalResyncUnavailable {
            terminal_id: TerminalId(2),
        },
        Event::TerminalDelta {
            terminal_id: TerminalId(2),
            from_offset: 4096,
            to_offset: 4107,
            bytes: b"delta bytes".to_vec(),
        },
        Event::TerminalDeltaUnavailable {
            terminal_id: TerminalId(2),
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: Some(0),
            last_output: None,
        },
        Event::TerminalExited {
            terminal_id: TerminalId(2),
            exit_code: None,
            last_output: Some("boom: could not start".into()),
        },
        Event::TerminalFocusRequested {
            terminal_id: TerminalId(2),
        },
        Event::WorkspaceFocusRequested {
            session_key: key.clone(),
        },
        Event::TerminalsRebadged {
            from: "github:o/r#1".into(),
            to: "github:o/r#2".into(),
        },
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::InputNeeded,
        },
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::Working,
        },
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::Idle,
        },
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::Done,
        },
        Event::AgentState {
            session_key: key.clone(),
            terminal_id: TerminalId(2),
            state: AgentState::Exited { code: Some(9) },
        },
        Event::AgentRunStarted {
            request_id: AgentRunRequestId("request-1".into()),
            run_id: AgentRunId(9),
            session_key: key.clone(),
            session_id: Some(sample_session_id(4)),
            agent: "claude".into(),
            mode: AgentRuntimeMode::StreamJson,
        },
        Event::AgentRunStartFailed {
            request_id: AgentRunRequestId("request-2".into()),
            message: "spawn failed".into(),
        },
        Event::AgentRawJson {
            run_id: AgentRunId(9),
            json: r#"{"type":"assistant","message":{"content":[]}}"#.into(),
        },
        Event::AgentDebug {
            run_id: AgentRunId(9),
            message: "runtime stderr line".into(),
        },
        Event::AgentAssistantTextDelta {
            run_id: AgentRunId(9),
            delta: "hello".into(),
        },
        Event::AgentToolCallStarted {
            run_id: AgentRunId(9),
            call_id: "toolu-1".into(),
            name: "Edit".into(),
            input_json: Some(r#"{"file_path":"src/lib.rs"}"#.into()),
        },
        Event::AgentToolCallDelta {
            run_id: AgentRunId(9),
            call_id: "toolu-1".into(),
            delta_json: r#"{"old_string":"a"}"#.into(),
        },
        Event::AgentToolCallFinished {
            run_id: AgentRunId(9),
            call_id: "toolu-1".into(),
            output_json: Some(r#"{"ok":true}"#.into()),
            error: None,
        },
        Event::AgentToolCallFinished {
            run_id: AgentRunId(9),
            call_id: "toolu-2".into(),
            output_json: None,
            error: Some("permission denied".into()),
        },
        Event::AgentPermissionRequest {
            run_id: AgentRunId(9),
            request_id: "perm-1".into(),
            tool_name: "Bash".into(),
            input_json: Some(r#"{"command":"cargo test"}"#.into()),
            reason: Some("run tests".into()),
        },
        Event::AgentUserQuestion {
            run_id: AgentRunId(9),
            question_id: "q-1".into(),
            prompt: "Which branch should I use?".into(),
            choices: vec!["main".into(), "develop".into()],
            allow_freeform: true,
        },
        Event::AgentUsage {
            run_id: AgentRunId(9),
            usage: AgentUsage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                cache_creation_input_tokens: Some(3),
                cache_read_input_tokens: Some(4),
                cost_usd_micros: Some(1234),
            },
        },
        Event::AgentSessionUsage {
            agent_id: "codex".into(),
            usage: AgentUsage {
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(50),
                cost_usd_micros: Some(4321),
            },
        },
        Event::AgentProviderQuota {
            agent_id: "claude".into(),
            quota: ProviderQuota {
                five_hour: Some(QuotaWindow {
                    utilization_bp: 4512,
                    reset_at: Some(1_700_000_000),
                }),
                weekly: Some(QuotaWindow {
                    utilization_bp: 6000,
                    reset_at: None,
                }),
            },
        },
        Event::AgentTurnFinished {
            run_id: AgentRunId(9),
            result: Some("turn complete".into()),
            session_id: Some("claude-session-1".into()),
            error: None,
        },
        Event::AgentTurnFinished {
            run_id: AgentRunId(9),
            result: None,
            session_id: None,
            error: Some("max turns reached".into()),
        },
        Event::AgentRunFinished {
            run_id: AgentRunId(9),
            exit_code: Some(0),
            error: None,
        },
        Event::AgentRunFinished {
            run_id: AgentRunId(10),
            exit_code: None,
            error: Some("interrupted".into()),
        },
        Event::ProviderCredentialUpdated {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
            metadata: credential_metadata.clone(),
        },
        Event::ProviderCredentialRemoved {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
        },
        Event::ProviderCredentialsListed {
            principal_id: principal_id.clone(),
            credentials: vec![credential_metadata],
        },
        Event::ProviderError {
            source: "github".into(),
            message: "rate limited".into(),
            detail: String::new(),
            kind: String::new(),
        },
        Event::GithubRateLimitWait {
            remaining: 98,
            limit: 5000,
            reset_at: chrono::DateTime::parse_from_rfc3339("2026-07-30T07:23:22Z")
                .expect("valid fixture")
                .with_timezone(&chrono::Utc),
        },
        Event::PollCompleted {
            source: "github".into(),
            count: 3,
        },
        Event::PollProgress {
            source: "github".into(),
            message: "fetching reviews".into(),
        },
        Event::Notification {
            title: "hi".into(),
            body: "body".into(),
        },
        Event::CleanWorktreesCompleted {
            removed: 3,
            skipped: 1,
        },
        Event::WorktreesInspected {
            inspections: vec![],
        },
        Event::WorktreesInspected {
            inspections: vec![lazybox_ipc::WorktreeInspectionDto {
                path: std::path::PathBuf::from("/tmp/wt"),
                bare_path: Some(std::path::PathBuf::from("/tmp/repos/o/r.git")),
                branch: Some("feat".into()),
                session_id: Some("12345678".into()),
                reasons: vec!["untracked".into(), "branch-deleted-upstream".into()],
                size_bytes: 4096,
                last_modified_unix: Some(1_700_000_000),
                has_uncommitted_changes: true,
                has_unpushed_commits: false,
                is_safe_to_delete: false,
            }],
        },
        Event::WorkspaceDiffInspected {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            target: lazybox_ipc::WorkspaceDiffTarget::Session(sample_session_id(21)),
            agent_terminal_ids: vec![TerminalId(12)],
            diff: Some(lazybox_ipc::WorkspaceDiffDto {
                status: vec![" M src/lib.rs".into()],
                stat: vec![" src/lib.rs | 1 +".into()],
                truncated: false,
                files: vec![lazybox_ipc::DiffFileDto {
                    old_path: Some("src/lib.rs".into()),
                    path: "src/lib.rs".into(),
                    headers: vec!["diff --git a/src/lib.rs b/src/lib.rs".into()],
                    hunks: vec![lazybox_ipc::DiffHunkDto {
                        header: "@@ -1 +1 @@".into(),
                        old_start: 1,
                        new_start: 1,
                        lines: vec![lazybox_ipc::DiffLineDto {
                            kind: lazybox_ipc::DiffLineKindDto::Addition,
                            text: "+changed".into(),
                            old_line: None,
                            new_line: Some(1),
                        }],
                    }],
                }],
            }),
            error: None,
        },
        Event::CheckoutsDiscovered { checkouts: vec![] },
        Event::CheckoutsDiscovered {
            checkouts: vec![lazybox_ipc::DiscoveredCheckoutDto {
                path: std::path::PathBuf::from("/home/dev/code/acme/widget"),
                repo: Some("acme/widget".into()),
                branch: Some("main".into()),
                has_uncommitted_changes: true,
            }],
        },
        Event::OrphanedWorktreeDeleted {
            path: std::path::PathBuf::from("/tmp/wt"),
            ok: true,
            error: None,
        },
        Event::OrphanedWorktreeDeleted {
            path: std::path::PathBuf::from("/tmp/wt"),
            ok: false,
            error: Some("has uncommitted changes".into()),
        },
        Event::TerminalInputRejected {
            terminal_id: TerminalId(12),
            message: "write timed out".into(),
        },
        Event::CommandRejected {
            command: "Write".into(),
            message: "terminal command queue is full".into(),
        },
        Event::TerminalScrollback {
            terminal_id: TerminalId(12),
            replay: b"deep history\r\nlive bottom".to_vec(),
            seq: 42,
        },
        Event::AgentCliUpdatesChecked {
            statuses: vec![lazybox_ipc::AgentCliUpdateStatus {
                agent_id: "claude".into(),
                display_name: "Claude Code".into(),
                installed: Some("2.1.3".into()),
                latest: Some("2.1.4".into()),
                update_available: true,
                error: None,
                auto_update: true,
            }],
            manual: true,
        },
        Event::AgentCliUpdateFinished {
            agent_id: "codex".into(),
            display_name: "Codex".into(),
            ok: false,
            installed_before: Some("0.46.0".into()),
            installed_after: None,
            message: "brew upgrade --cask codex failed: exit 1".into(),
        },
        Event::RecoveredTerminalsRequireRestart {
            terminal_ids: vec![TerminalId(12), TerminalId(13)],
        },
        Event::BranchUpdated {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            pr_label: "o/r#2".into(),
        },
        Event::BranchUpdateFailed {
            workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#2"),
            pr_label: "o/r#2".into(),
            reason: "merge conflict".into(),
        },
        Event::SnippetDelivered {
            terminal_id: TerminalId(12),
            session_key: "github:o/r#1".into(),
            snippet_key: "rev".into(),
            prompt: Some(UserPrompt {
                text: "review the diff".into(),
                timestamp_ms: 1_700_000_000_000,
                source: PromptSource::Snippet {
                    key: "rev".into(),
                    category: "Review".into(),
                },
            }),
        },
        Event::CommandCompleted {
            client_request_id: "spawn-1".into(),
        },
        Event::CommandFailed {
            client_request_id: "close-1".into(),
            message: "backend kill failed".into(),
        },
        Event::AgentAuthRequired {
            terminal_id: TerminalId(12),
            agent_id: "codex".into(),
            display_name: "Codex".into(),
            reason: "provider sign-in expired".into(),
            other_session_count: 2,
            credentials_isolated: true,
        },
        Event::AgentAuthProgress {
            recovery_terminal_id: TerminalId(12),
            terminal_id: TerminalId(12),
            phase: lazybox_ipc::AgentAuthPhase::LoginInteractive,
        },
        Event::AgentAuthFinished {
            recovery_terminal_id: TerminalId(12),
            terminal_id: TerminalId(12),
            display_name: "Codex".into(),
            success: false,
            error: Some("provider login exited with status 1".into()),
        },
        Event::AgentResumeFallback {
            terminal_id: TerminalId(12),
            display_name: "Codex".into(),
        },
        Event::TerminalModelChanged {
            session_key: key.clone(),
            terminal_id: TerminalId(13),
            model_label: "gpt-5.5 · xhigh".into(),
        },
        Event::ErrorInbox {
            errors: vec![lazybox_ipc::ErrorInboxRecord {
                dedupe_key: "github|merge|rate limited".into(),
                source: "github".into(),
                severity: "retryable".into(),
                operation: Some("merge".into()),
                workspace_key: Some("github:o/r#1".into()),
                message: "✗ merge failed — rate limited".into(),
                raw: "GraphQL code=RATE_LIMITED".into(),
                count: 500,
                first_seen: sample_time(),
                last_seen: sample_time(),
            }],
        },
        Event::AgentUsageLimit {
            session_key: key.clone(),
            terminal_id: TerminalId(14),
            reset_hint: "3pm".into(),
        },
        Event::ResourcePosture(lazybox_ipc::ResourcePosture {
            live_agents: 12,
            agent_cap: Some(32),
            terminals: 19,
            log_bytes: Some(4_096),
            state_db_bytes: None,
            bus_lagged_events: 3,
            bus_lag_recoveries: 1,
            terminal_output_dropped: 0,
            terminal_resyncs: 0,
            inline_budget_violations: 2,
        }),
        Event::AgentCreditRecovery {
            terminal_id: TerminalId(12),
            client_request_id: "credit-recovery-1".into(),
            stage: lazybox_ipc::AgentCreditRecoveryStage::WaitingForComposer,
        },
        Event::AgentCreditExhausted {
            session_key: key,
            terminal_id: TerminalId(12),
            hint: "add credits or switch subscription".into(),
        },
    ]
}

/// Exhaustive discriminant projection. Adding a wire variant makes this test
/// module fail to compile until the variant is named here; the coverage test
/// below then fails until `all_commands` also carries a round-trip sample.
fn command_tag(command: &Command) -> &'static str {
    match command {
        Command::Subscribe => "Subscribe",
        Command::CreateSession { .. } => "CreateSession",
        Command::Spawn { .. } => "Spawn",
        Command::CancelSpawn { .. } => "CancelSpawn",
        Command::Write { .. } => "Write",
        Command::RecordUserMessage { .. } => "RecordUserMessage",
        Command::InjectPrompt { .. } => "InjectPrompt",
        Command::RecordComposingBuffer { .. } => "RecordComposingBuffer",
        Command::Resize { .. } => "Resize",
        Command::RequestTerminalResync { .. } => "RequestTerminalResync",
        Command::RequestTerminalDelta { .. } => "RequestTerminalDelta",
        Command::Close { .. } => "Close",
        Command::IngestHook { .. } => "IngestHook",
        Command::Kill { .. } => "Kill",
        Command::RemoveMergedWorkspace { .. } => "RemoveMergedWorkspace",
        Command::DeleteProject { .. } => "DeleteProject",
        Command::CollapseIntoPr { .. } => "CollapseIntoPr",
        Command::MarkRead { .. } => "MarkRead",
        Command::FocusWorkspace { .. } => "FocusWorkspace",
        Command::ActivateWorkspace { .. } => "ActivateWorkspace",
        Command::MarkActivityRead { .. } => "MarkActivityRead",
        Command::UnmarkActivityRead { .. } => "UnmarkActivityRead",
        Command::CreateWorkspace { .. } => "CreateWorkspace",
        Command::CreateProject { .. } => "CreateProject",
        Command::SetSessionLayout { .. } => "SetSessionLayout",
        Command::Snooze { .. } => "Snooze",
        Command::Unsnooze { .. } => "Unsnooze",
        Command::SetAutoMergeOnGreen { .. } => "SetAutoMergeOnGreen",
        Command::SetTrackMain { .. } => "SetTrackMain",
        Command::SetAutoFixPolicy { .. } => "SetAutoFixPolicy",
        Command::SetAutoFixPolicies { .. } => "SetAutoFixPolicies",
        Command::PostReply { .. } => "PostReply",
        Command::Refresh => "Refresh",
        Command::Shutdown => "Shutdown",
        Command::ConfirmMerge { .. } => "ConfirmMerge",
        Command::AdoptSessions { .. } => "AdoptSessions",
        Command::MergePr { .. } => "MergePr",
        Command::CloseIssue { .. } => "CloseIssue",
        Command::DeleteOrClose { .. } => "DeleteOrClose",
        Command::RequestReviewers { .. } => "RequestReviewers",
        Command::AddAssignees { .. } => "AddAssignees",
        Command::SetAssignees { .. } => "SetAssignees",
        Command::SetLabels { .. } => "SetLabels",
        Command::FetchRepoLabels { .. } => "FetchRepoLabels",
        Command::FetchRequestableReviewers { .. } => "FetchRequestableReviewers",
        Command::CleanWorktrees => "CleanWorktrees",
        Command::InspectWorktrees => "InspectWorktrees",
        Command::InspectWorkspaceDiff { .. } => "InspectWorkspaceDiff",
        Command::ScanCheckouts { .. } => "ScanCheckouts",
        Command::ImportLocalCheckout { .. } => "ImportLocalCheckout",
        Command::DeleteOrphanedWorktree { .. } => "DeleteOrphanedWorktree",
        Command::FetchPrDetails { .. } => "FetchPrDetails",
        Command::SyncWorkspace { .. } => "SyncWorkspace",
        Command::StartAgentRun { .. } => "StartAgentRun",
        Command::SendAgentInput { .. } => "SendAgentInput",
        Command::InterruptAgentRun { .. } => "InterruptAgentRun",
        Command::DecideAgentApproval { .. } => "DecideAgentApproval",
        Command::AnswerAgentQuestion { .. } => "AnswerAgentQuestion",
        Command::UpsertProviderCredential { .. } => "UpsertProviderCredential",
        Command::RemoveProviderCredential { .. } => "RemoveProviderCredential",
        Command::ListProviderCredentials { .. } => "ListProviderCredentials",
        Command::KeepMergedWorkspace { .. } => "KeepMergedWorkspace",
        Command::FetchScrollback { .. } => "FetchScrollback",
        Command::CheckAgentCliUpdates => "CheckAgentCliUpdates",
        Command::UpdateAgentClis => "UpdateAgentClis",
        Command::SetNotes { .. } => "SetNotes",
        Command::UpdateBranch { .. } => "UpdateBranch",
        Command::DeliverSnippet { .. } => "DeliverSnippet",
        Command::SetUpdateDismissal { .. } => "SetUpdateDismissal",
        Command::ResumeAgent { .. } => "ResumeAgent",
        Command::ReauthenticateAgent { .. } => "ReauthenticateAgent",
        Command::CancelAgentReauthentication { .. } => "CancelAgentReauthentication",
        Command::RenameWorkspace { .. } => "RenameWorkspace",
        Command::RecreateWorktree { .. } => "RecreateWorktree",
        Command::ListErrors => "ListErrors",
        Command::ClearErrors => "ClearErrors",
        Command::DeleteError { .. } => "DeleteError",
        Command::GetResourcePosture => "GetResourcePosture",
        Command::RecoverAgentCredit { .. } => "RecoverAgentCredit",
    }
}

/// Event-side companion to [`command_tag`]. Keep both exhaustive: bincode
/// accepts a locally-consistent encoder/decoder even when a new variant was
/// never exercised, which made the former "every variant" test a false
/// contract.
fn event_tag(event: &Event) -> &'static str {
    match event {
        Event::Snapshot { .. } => "Snapshot",
        Event::ViewerIdentities { .. } => "ViewerIdentities",
        Event::AutoFixPolicyConfig { .. } => "AutoFixPolicyConfig",
        Event::ShellCommandConfig { .. } => "ShellCommandConfig",
        Event::AgentAvailabilityConfig { .. } => "AgentAvailabilityConfig",
        Event::WorkspaceUpserted(_) => "WorkspaceUpserted",
        Event::WorkspaceRemoved(_) => "WorkspaceRemoved",
        Event::ProjectUpserted(_) => "ProjectUpserted",
        Event::ProjectRemoved(_) => "ProjectRemoved",
        Event::WorkspaceOutOfScope { .. } => "WorkspaceOutOfScope",
        Event::WorkspaceMergePending { .. } => "WorkspaceMergePending",
        Event::WorkspaceMerged { .. } => "WorkspaceMerged",
        Event::PrMerged { .. } => "PrMerged",
        Event::PrMergeFailed { .. } => "PrMergeFailed",
        Event::IssueClosed { .. } => "IssueClosed",
        Event::IssueCloseFailed { .. } => "IssueCloseFailed",
        Event::PrClosed { .. } => "PrClosed",
        Event::IssueDeleted { .. } => "IssueDeleted",
        Event::DeleteOrCloseFailed { .. } => "DeleteOrCloseFailed",
        Event::MergedPrRemovable { .. } => "MergedPrRemovable",
        Event::RemovalCancelled { .. } => "RemovalCancelled",
        Event::RepoLabels { .. } => "RepoLabels",
        Event::RequestableReviewers { .. } => "RequestableReviewers",
        Event::SessionCreated(_) => "SessionCreated",
        Event::WorktreeProgress { .. } => "WorktreeProgress",
        Event::SessionEnded { .. } => "SessionEnded",
        Event::TerminalSpawned { .. } => "TerminalSpawned",
        Event::TerminalReplaced { .. } => "TerminalReplaced",
        Event::TerminalOutput { .. } => "TerminalOutput",
        Event::AgentAuthOutput { .. } => "AgentAuthOutput",
        Event::AgentAuthReplay { .. } => "AgentAuthReplay",
        Event::TerminalResync { .. } => "TerminalResync",
        Event::TerminalResyncUnavailable { .. } => "TerminalResyncUnavailable",
        Event::TerminalDelta { .. } => "TerminalDelta",
        Event::TerminalDeltaUnavailable { .. } => "TerminalDeltaUnavailable",
        Event::TerminalExited { .. } => "TerminalExited",
        Event::TerminalFocusRequested { .. } => "TerminalFocusRequested",
        Event::WorkspaceFocusRequested { .. } => "WorkspaceFocusRequested",
        Event::TerminalsRebadged { .. } => "TerminalsRebadged",
        Event::AgentState { .. } => "AgentState",
        Event::ProviderError { .. } => "ProviderError",
        Event::GithubRateLimitWait { .. } => "GithubRateLimitWait",
        Event::PollCompleted { .. } => "PollCompleted",
        Event::PollProgress { .. } => "PollProgress",
        Event::Notification { .. } => "Notification",
        Event::CleanWorktreesCompleted { .. } => "CleanWorktreesCompleted",
        Event::WorktreesInspected { .. } => "WorktreesInspected",
        Event::WorkspaceDiffInspected { .. } => "WorkspaceDiffInspected",
        Event::CheckoutsDiscovered { .. } => "CheckoutsDiscovered",
        Event::OrphanedWorktreeDeleted { .. } => "OrphanedWorktreeDeleted",
        Event::AgentRunStarted { .. } => "AgentRunStarted",
        Event::AgentRunStartFailed { .. } => "AgentRunStartFailed",
        Event::AgentRawJson { .. } => "AgentRawJson",
        Event::AgentDebug { .. } => "AgentDebug",
        Event::AgentAssistantTextDelta { .. } => "AgentAssistantTextDelta",
        Event::AgentToolCallStarted { .. } => "AgentToolCallStarted",
        Event::AgentToolCallDelta { .. } => "AgentToolCallDelta",
        Event::AgentToolCallFinished { .. } => "AgentToolCallFinished",
        Event::AgentPermissionRequest { .. } => "AgentPermissionRequest",
        Event::AgentUserQuestion { .. } => "AgentUserQuestion",
        Event::AgentUsage { .. } => "AgentUsage",
        Event::AgentSessionUsage { .. } => "AgentSessionUsage",
        Event::AgentProviderQuota { .. } => "AgentProviderQuota",
        Event::AgentTurnFinished { .. } => "AgentTurnFinished",
        Event::AgentRunFinished { .. } => "AgentRunFinished",
        Event::ProviderCredentialUpdated { .. } => "ProviderCredentialUpdated",
        Event::ProviderCredentialRemoved { .. } => "ProviderCredentialRemoved",
        Event::ProviderCredentialsListed { .. } => "ProviderCredentialsListed",
        Event::TerminalInputRejected { .. } => "TerminalInputRejected",
        Event::CommandRejected { .. } => "CommandRejected",
        Event::TerminalScrollback { .. } => "TerminalScrollback",
        Event::AgentCliUpdatesChecked { .. } => "AgentCliUpdatesChecked",
        Event::AgentCliUpdateFinished { .. } => "AgentCliUpdateFinished",
        Event::RecoveredTerminalsRequireRestart { .. } => "RecoveredTerminalsRequireRestart",
        Event::BranchUpdated { .. } => "BranchUpdated",
        Event::BranchUpdateFailed { .. } => "BranchUpdateFailed",
        Event::SnippetDelivered { .. } => "SnippetDelivered",
        Event::CommandCompleted { .. } => "CommandCompleted",
        Event::CommandFailed { .. } => "CommandFailed",
        Event::AgentAuthRequired { .. } => "AgentAuthRequired",
        Event::AgentAuthProgress { .. } => "AgentAuthProgress",
        Event::AgentAuthFinished { .. } => "AgentAuthFinished",
        Event::AgentResumeFallback { .. } => "AgentResumeFallback",
        Event::TerminalModelChanged { .. } => "TerminalModelChanged",
        Event::ErrorInbox { .. } => "ErrorInbox",
        Event::AgentUsageLimit { .. } => "AgentUsageLimit",
        Event::WorkspaceCreated { .. } => "WorkspaceCreated",
        Event::ResourcePosture(_) => "ResourcePosture",
        Event::AgentCreditRecovery { .. } => "AgentCreditRecovery",
        Event::AgentCreditExhausted { .. } => "AgentCreditExhausted",
    }
}

#[test]
fn round_trip_corpus_covers_every_wire_variant() {
    let command_tags: std::collections::BTreeSet<_> =
        all_commands().iter().map(command_tag).collect();
    let event_tags: std::collections::BTreeSet<_> = all_events().iter().map(event_tag).collect();

    assert_eq!(
        command_tags.len(),
        79,
        "Command gained/lost a variant: update the exhaustive tag and add a corpus sample",
    );
    assert_eq!(
        event_tags.len(),
        89,
        "Event gained/lost a variant: update the exhaustive tag and add a corpus sample",
    );
}

/// Round-trip every Command through bincode. Any new variant added to
/// the enum must show up in `all_commands` or this test fails — that's
/// the point.
#[test]
fn command_bincode_round_trip() {
    for cmd in all_commands() {
        let config = bincode::config::legacy();
        let bytes = bincode::serde::encode_to_vec(&cmd, config).expect("serialize");
        let (back, consumed): (Command, usize) =
            bincode::serde::decode_from_slice(&bytes, config).expect("deserialize");
        assert_eq!(consumed, bytes.len());
        // Debug-equality since the wire types intentionally don't
        // derive PartialEq (Sessions carry timestamps we can't compare
        // structurally). Debug output uniqueness is the contract.
        assert_eq!(format!("{cmd:?}"), format!("{back:?}"));
    }
}

#[test]
fn provider_credential_command_debug_redacts_token() {
    let cmd = Command::UpsertProviderCredential {
        principal_id: PrincipalId::local(),
        credential: ProviderCredentialInput {
            provider_id: "github".into(),
            token: "ghp_do_not_log".into(),
            source: "unit-test".into(),
            scopes: vec![],
            expires_at: None,
        },
    };

    let debug = format!("{cmd:?}");

    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("ghp_do_not_log"));
}

#[test]
fn event_bincode_round_trip() {
    for ev in all_events() {
        let config = bincode::config::legacy();
        let bytes = bincode::serde::encode_to_vec(&ev, config).expect("serialize");
        let (back, consumed): (Event, usize) = bincode::serde::decode_from_slice(&bytes, config)
            .unwrap_or_else(|error| panic!("deserialize {ev:?}: {error}"));
        assert_eq!(consumed, bytes.len());
        assert_eq!(format!("{ev:?}"), format!("{back:?}"));
    }
}

/// Framed write → framed read over a tokio duplex stream returns the
/// same message. Exercises the socket transport without actually
/// touching the filesystem or kernel sockets.
#[tokio::test]
async fn socket_framing_round_trip() {
    use lazybox_ipc::socket::{read_frame, write_frame};

    let (mut a, mut b) = duplex(64 * 1024);

    // Alice sends, Bob receives.
    tokio::spawn(async move {
        for cmd in all_commands() {
            write_frame(&mut a, &cmd).await.expect("write");
        }
        // Drop on exit → closes the pipe → Bob's read_frame returns None.
    });

    let mut seen = 0usize;
    while let Some(cmd) = read_frame::<_, Command>(&mut b).await.expect("read") {
        // Serialize again; if we got it we should be able to re-emit.
        let _bytes =
            bincode::serde::encode_to_vec(&cmd, bincode::config::legacy()).expect("reserialize");
        seen += 1;
    }
    assert_eq!(seen, all_commands().len());
}

/// Frames larger than `MAX_FRAME_BYTES` must error cleanly instead of
/// allocating a huge buffer. Simulates a malicious or corrupted peer.
#[tokio::test]
async fn socket_rejects_oversized_frames() {
    use lazybox_ipc::MAX_FRAME_BYTES;
    use lazybox_ipc::socket::read_frame;
    use tokio::io::AsyncWriteExt;

    let (mut a, mut b) = duplex(64);
    let bad_len = MAX_FRAME_BYTES + 1;

    // Writer is the adversary — emits a length prefix claiming more
    // bytes than we allow, then drops.
    tokio::spawn(async move {
        let _ = a.write_all(&bad_len.to_be_bytes()).await;
    });

    let result: Result<Option<Command>, _> = read_frame(&mut b).await;
    assert!(
        matches!(result, Err(lazybox_ipc::socket::FrameError::TooLarge(n)) if n == bad_len),
        "expected TooLarge, got {result:?}"
    );
}

/// A clean EOF (peer drops between frames) returns Ok(None), not an
/// error. That's how the daemon distinguishes orderly client shutdown
/// from a transport fault.
#[tokio::test]
async fn socket_clean_eof_is_none() {
    use lazybox_ipc::socket::read_frame;

    let (a, mut b) = duplex(64);
    drop(a);
    let result: Result<Option<Command>, _> = read_frame(&mut b).await;
    assert!(
        matches!(result, Ok(None)),
        "expected Ok(None), got {result:?}"
    );
}

/// Zero-byte message (empty bytes payload) round-trips. Guard against
/// edge cases in the framing arithmetic.
#[tokio::test]
async fn socket_zero_byte_payload_works() {
    use lazybox_ipc::socket::{read_frame, write_frame};
    let (mut a, mut b) = duplex(64);
    let msg = Command::Write {
        terminal_id: TerminalId(1),
        bytes: vec![],
        intent: TerminalInputIntent::Compose,
    };
    write_frame(&mut a, &msg).await.expect("write");
    drop(a);
    let got: Option<Command> = read_frame(&mut b).await.expect("read");
    let got = got.expect("one message");
    assert_eq!(format!("{got:?}"), format!("{msg:?}"));
}

/// Non-trivial binary payloads (ANSI escape sequences, UTF-8) survive.
/// Terminal output carries both.
#[tokio::test]
async fn socket_binary_terminal_output_round_trip() {
    use lazybox_ipc::socket::{read_frame, write_frame};
    let (mut a, mut b) = duplex(64 * 1024);

    let nasty: Vec<u8> = (0..=255).collect();
    let msg = Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: nasty.clone(),
        first_seq: 99,
        seq: 99,
    };
    write_frame(&mut a, &msg).await.expect("write");
    drop(a);
    let got: Option<Event> = read_frame(&mut b).await.expect("read");
    if let Some(Event::TerminalOutput {
        bytes,
        first_seq,
        seq,
        ..
    }) = got
    {
        assert_eq!(bytes, nasty);
        assert_eq!(first_seq, 99);
        assert_eq!(seq, 99);
    } else {
        panic!("expected TerminalOutput, got {got:?}");
    }
}

// ── Protocol handshake ─────────────────────────────────────────────────
//
// The 8-byte preamble (magic + wire fingerprint) is exchanged before
// any frames. These tests pin the success path, the rejection of
// fingerprint skew in both directions, garbage preambles, and the EOF
// a pre-handshake daemon produces.

mod handshake {
    use lazybox_ipc::socket::{HandshakeError, client_handshake, server_handshake};
    use lazybox_ipc::{BUILD_VERSION, PROTOCOL_FINGERPRINT, PROTOCOL_MAGIC};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    fn preamble(fingerprint: u32) -> [u8; 8] {
        let mut p = [0u8; 8];
        p[..4].copy_from_slice(&PROTOCOL_MAGIC);
        p[4..].copy_from_slice(&fingerprint.to_le_bytes());
        p
    }

    /// The build-version frame written after a matching preamble: u16 LE
    /// length + UTF-8 bytes.
    fn build_frame(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u16).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    #[tokio::test]
    async fn succeeds_when_fingerprints_match() {
        let (client, server) = duplex(512);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);
        let server_task = tokio::spawn(async move { server_handshake(&mut srd, &mut swr).await });
        let peer = client_handshake(&mut crd, &mut cwr)
            .await
            .expect("client handshake");
        let client_seen = server_task.await.expect("join").expect("server handshake");
        // Both sides exchange this binary's build and agree it matches.
        assert_eq!(peer.build, BUILD_VERSION);
        assert!(peer.build_matches());
        assert_eq!(client_seen.build, BUILD_VERSION);
    }

    /// A peer with the same wire fingerprint but a different build
    /// connects successfully — the skew is reported to the caller, not
    /// rejected. This is the stale-daemon case the fingerprint can't
    /// catch: only non-wire sources changed between the two builds.
    #[tokio::test]
    async fn same_fingerprint_different_build_connects_and_reports_skew() {
        let (client, server) = duplex(512);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        let fake_daemon = tokio::spawn(async move {
            let mut got = [0u8; 8];
            srd.read_exact(&mut got).await.expect("client preamble");
            assert_eq!(got, preamble(PROTOCOL_FINGERPRINT));
            swr.write_all(&preamble(PROTOCOL_FINGERPRINT))
                .await
                .expect("reply preamble");
            swr.write_all(&build_frame("9.9.9+deadbeef"))
                .await
                .expect("reply build");
            // Drain the client's own build frame so the duplex doesn't stall.
            let mut len = [0u8; 2];
            srd.read_exact(&mut len).await.expect("client build len");
            let mut buf = vec![0u8; u16::from_le_bytes(len) as usize];
            srd.read_exact(&mut buf).await.expect("client build body");
        });

        let peer = client_handshake(&mut crd, &mut cwr)
            .await
            .expect("handshake must succeed on a build skew");
        assert_eq!(peer.build, "9.9.9+deadbeef");
        assert!(!peer.build_matches());
        fake_daemon.await.expect("join");
    }

    #[tokio::test]
    async fn server_rejects_newer_client_but_still_replies() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        cwr.write_all(&preamble(PROTOCOL_FINGERPRINT.wrapping_add(1)))
            .await
            .expect("send fake preamble");
        let err = server_handshake(&mut srd, &mut swr)
            .await
            .expect_err("must reject");
        assert!(matches!(
            err,
            HandshakeError::FingerprintMismatch { peer, ours }
                if peer == PROTOCOL_FINGERPRINT.wrapping_add(1) && ours == PROTOCOL_FINGERPRINT
        ));

        // The server replied with its own preamble before closing, so
        // the mismatched client can render the clear mismatch error.
        let mut reply = [0u8; 8];
        crd.read_exact(&mut reply).await.expect("server reply");
        assert_eq!(reply, preamble(PROTOCOL_FINGERPRINT));
    }

    #[tokio::test]
    async fn client_rejects_fingerprint_skewed_daemon() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        let fake_daemon = tokio::spawn(async move {
            let mut got = [0u8; 8];
            srd.read_exact(&mut got).await.expect("client preamble");
            assert_eq!(got, preamble(PROTOCOL_FINGERPRINT));
            swr.write_all(&preamble(PROTOCOL_FINGERPRINT.wrapping_add(7)))
                .await
                .expect("reply");
        });
        let err = client_handshake(&mut crd, &mut cwr)
            .await
            .expect_err("must reject");
        assert!(matches!(
            err,
            HandshakeError::FingerprintMismatch { peer, ours }
                if peer == PROTOCOL_FINGERPRINT.wrapping_add(7) && ours == PROTOCOL_FINGERPRINT
        ));
        // The message tells the user what to do, not just that bytes
        // disagreed.
        assert!(err.to_string().contains("restart the daemon"));
        fake_daemon.await.expect("join");
    }

    #[tokio::test]
    async fn server_rejects_garbage_preamble_without_replying() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        cwr.write_all(b"GARBAGE!").await.expect("send garbage");
        let err = server_handshake(&mut srd, &mut swr)
            .await
            .expect_err("must reject");
        assert!(matches!(err, HandshakeError::BadMagic(m) if &m == b"GARB"));

        // No reply preamble for a non-lazybox peer: once the server
        // side drops, the client read hits EOF with zero bytes seen.
        drop(srd);
        drop(swr);
        let mut buf = [0u8; 8];
        assert!(crd.read_exact(&mut buf).await.is_err());
    }

    #[tokio::test]
    async fn client_rejects_garbage_reply() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        let (mut srd, mut swr) = tokio::io::split(server);

        let fake_daemon = tokio::spawn(async move {
            let mut got = [0u8; 8];
            srd.read_exact(&mut got).await.expect("client preamble");
            swr.write_all(b"NOTLZBX!").await.expect("reply");
        });
        let err = client_handshake(&mut crd, &mut cwr)
            .await
            .expect_err("must reject");
        assert!(matches!(err, HandshakeError::BadMagic(_)));
        fake_daemon.await.expect("join");
    }

    #[tokio::test]
    async fn client_surfaces_pre_handshake_daemon_as_clear_error() {
        let (client, server) = duplex(64);
        let (mut crd, mut cwr) = tokio::io::split(client);
        // A pre-handshake daemon reads our preamble as an oversized
        // frame length and closes without replying — modeled here by
        // dropping the server end outright.
        drop(server);
        let err = client_handshake(&mut crd, &mut cwr)
            .await
            .expect_err("must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("predates the protocol handshake"),
            "error must mention the pre-handshake-daemon case, got: {msg}"
        );
    }
}
