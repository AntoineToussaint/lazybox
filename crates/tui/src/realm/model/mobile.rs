//! Minimal mobile adapter: sessions, new session, and one full-width terminal.
use super::{ChoicePayload, Id, ModalFlow, Model, PaneFocus, SessionRunner};
use crate::realm::components::{
    choice::Choice, mobile_confirm::MobileConfirm, mobile_rail::RailAction,
    mobile_sessions::SessionRow,
};
use crate::realm::presentation::Presentation;
use lazybox_ipc::{AgentState, Command, TerminalKind};
use tuirealm::event::{Key, KeyEvent, KeyModifiers};
use tuirealm::terminal::TerminalAdapter;

impl<T: TerminalAdapter> Model<T> {
    /// Choose the UI for this client only; no shared configuration is changed.
    pub fn with_presentation(mut self, presentation: Presentation) -> Self {
        self.presentation = presentation;
        self
    }

    pub(super) fn refresh_mobile_sessions(&mut self) {
        let rows = self
            .terminals
            .terminal_summaries()
            .into_iter()
            .map(|terminal| {
                let workspace = self.sidebar.workspace_by_key(&terminal.session_key);
                let title = workspace
                    .map(|w| w.name.clone())
                    .unwrap_or_else(|| terminal.session_key.to_string());
                let runner = match &terminal.kind {
                    TerminalKind::Agent(id) => id.as_str(),
                    TerminalKind::Shell => "shell",
                    TerminalKind::LogTail { .. } => "logs",
                };
                let state = if terminal.exited {
                    "stopped"
                } else {
                    match (&terminal.kind, &terminal.state) {
                        (TerminalKind::Shell | TerminalKind::LogTail { .. }, _) => "running",
                        (_, AgentState::Working) => "working",
                        (_, AgentState::Idle) => "ready",
                        (_, AgentState::Done) => "done",
                        (_, AgentState::Exited { .. }) => "stopped",
                        (_, AgentState::AwaitingReset) => "waiting",
                        _ => "needs input",
                    }
                };
                let repo = workspace
                    .and_then(|w| w.project_key.as_ref())
                    .filter(|key| **key != lazybox_core::ProjectKey::local(Self::SCRATCH_PROJECT))
                    .and_then(|key| self.projects.get(key))
                    .map(|p| p.name.as_str());
                let detail = match repo {
                    Some(repo) => format!("{state} · {runner} · {repo}"),
                    None => format!("{state} · {runner}"),
                };
                SessionRow {
                    terminal_id: terminal.id,
                    session_key: terminal.session_key,
                    title,
                    detail,
                    attention: state == "needs input",
                    runner: runner.to_string(),
                    state: matches!(terminal.kind, TerminalKind::Agent(_))
                        .then_some(terminal.state),
                    exited: terminal.exited,
                }
            })
            .collect();
        self.mobile_sessions.update(rows);
    }

    pub(super) fn open_mobile_sessions(&mut self) {
        self.refresh_mobile_sessions();
        self.mobile_rail.open(self.mobile_sessions.rows());
        if let Some(id) = self.terminals.focused_terminal_id() {
            self.mobile_rail.highlight_initial(id);
        }
        self.redraw = true;
    }

    fn open_mobile_selection(&mut self) {
        self.refresh_mobile_sessions();
        let Some(row) = self.mobile_sessions.selected().cloned() else {
            return;
        };
        // Reveal the workspace even if the desktop sidebar's lens hides it.
        // Cursor motion in the mobile list itself never changes this focus.
        if self.sidebar.reveal_workspace_key(&row.session_key) {
            self.sync_panes();
        } else {
            self.terminals.set_active_session(Some(row.session_key));
        }
        self.terminals.focus_terminal(row.terminal_id);
        self.mobile_rail.close();
        self.set_focus(PaneFocus::Terminals);
        self.redraw = true;
    }

    fn new_mobile_session(&mut self) {
        let mut rows = vec![(
            "No repository".to_string(),
            ChoicePayload::Text("scratch".into()),
        )];
        let mut repos: Vec<_> = self
            .projects
            .values()
            .filter(|p| p.key.source_prefix() == "github" || p.root_dir.is_some())
            .map(|p| (p.display_name(), ChoicePayload::Project(p.key.clone())))
            .collect();
        repos.sort_by_key(|row| row.0.to_lowercase());
        rows.extend(repos);
        self.mount_modal(
            Id::MobileNewSession,
            Choice::single("Choose an area · repository optional", rows)
                .title("New session · area")
                .label(|(label, _)| label.clone())
                .payload_for(|(_, payload)| payload.clone()),
        );
    }

    pub(super) fn mobile_new_session_picked(&mut self, picks: &[ChoicePayload]) -> Vec<Command> {
        let project = match picks.first() {
            Some(ChoicePayload::Text(value)) if value == "scratch" => {
                lazybox_core::ProjectKey::local(Self::SCRATCH_PROJECT)
            }
            Some(ChoicePayload::Project(key)) if self.projects.contains_key(key) => key.clone(),
            _ => return Vec::new(),
        };
        let area = self
            .projects
            .get(&project)
            .map(|p| p.display_name())
            .unwrap_or_else(|| "No repository".into());
        let mut agents = self.agents.clone();
        let default_agent = self.sidebar.default_agent();
        // Put the configured default first without mutating it when a different
        // agent is chosen for this one chat. The enabled list is authoritative.
        agents.sort_by_key(|agent| agent != default_agent);
        agents.dedup();
        let mut rows: Vec<_> = agents
            .into_iter()
            .map(|agent| {
                let label = if agent == default_agent {
                    format!("{agent} (default)")
                } else {
                    agent.clone()
                };
                (label, ChoicePayload::Text(agent))
            })
            .collect();
        // A typed optional value keeps Shell distinct from all possible agent IDs.
        rows.push(("Shell".into(), ChoicePayload::OptText(None)));
        self.set_modal_flow(ModalFlow::MobileRunner { project });
        // Leave the area picker mounted underneath so Escape returns to the
        // same cursor/scroll position. No create commands precede the final pick.
        self.mount_modal(
            Id::MobileRunner,
            Choice::single(format!("{area} · choose what to run"), rows)
                .title("New session · run")
                .label(|(label, _)| label.clone())
                .payload_for(|(_, payload)| payload.clone()),
        );
        Vec::new()
    }

    pub(super) fn mobile_runner_picked(&mut self, picks: &[ChoicePayload]) -> Vec<Command> {
        let runner = match picks.first() {
            Some(ChoicePayload::Text(agent)) if self.agents.contains(agent) => {
                SessionRunner::Agent(agent.clone())
            }
            Some(ChoicePayload::OptText(None)) => SessionRunner::Shell,
            _ => return Vec::new(),
        };
        let Some(ModalFlow::MobileRunner { project }) = self.modal_flow.take() else {
            return Vec::new();
        };
        self.mobile_rail.close();
        self.pop_modal();
        if self.top_modal() == Some(&Id::MobileNewSession) {
            self.pop_modal();
        }
        if project == lazybox_core::ProjectKey::local(Self::SCRATCH_PROJECT) {
            self.start_chat_with_runner_cmds(runner)
        } else if self.projects.contains_key(&project) {
            let name = self.next_chat_name(&project);
            self.create_workspace_with_runner_cmds(project, name, runner)
        } else {
            self.flash_error("That area is no longer available. Choose an area again.");
            Vec::new()
        }
    }

    fn rename_mobile_selection(&mut self) {
        if let Some(row) = self.mobile_sessions.selected() {
            self.mount_rename_workspace_input(row.session_key.clone());
        }
    }

    fn delete_mobile_selection(&mut self) {
        let Some(row) = self.mobile_sessions.selected() else {
            return;
        };
        let terminal_id = row.terminal_id;
        let question = format!(
            "Delete {} · {} (session {})?\n\nThis stops this session and removes it from the list. Other sessions and workspace files are kept.\n\nPress y to delete, or n to cancel.",
            row.title, row.runner, terminal_id.0
        );
        self.set_modal_flow(ModalFlow::MobileDeleteSession { terminal_id });
        self.mount_modal(Id::MobileDeleteSession, MobileConfirm::new(question));
    }

    pub(super) fn mobile_link_picked(&mut self, picks: &[ChoicePayload]) {
        self.pop_modal();
        if let Some(ChoicePayload::Text(url)) = picks.first() {
            super::helpers::emit_clipboard_copy(url);
            self.apply_mobile_rail_action(RailAction::Close);
            self.flash_hint("Link sent to terminal clipboard");
        }
    }

    fn copy_mobile_link(&mut self, terminal_id: lazybox_ipc::TerminalId) {
        let Some(urls) = self.terminals.urls_for(terminal_id) else {
            self.flash_info("That session has ended");
            return;
        };
        let prompt = if urls.is_empty() {
            "No links in this terminal view. Scroll to the link first. Enter/Esc back"
        } else {
            "Enter copies the full link · Esc back"
        };
        self.mount_modal(
            Id::MobileLinks,
            Choice::single(prompt, urls.into_iter().rev().collect())
                .title("Copy link")
                .label(|url: &String| url.clone())
                .payload_for(|url: &String| ChoicePayload::Text(url.clone())),
        );
    }

    fn apply_mobile_rail_action(&mut self, action: RailAction) {
        match action {
            RailAction::None => (),
            RailAction::Close => {
                self.mobile_rail.close();
                if self.terminals.focused_terminal_id().is_some() {
                    self.set_focus(PaneFocus::Terminals);
                } else if self.focus != PaneFocus::Terminals {
                    self.open_mobile_selection();
                }
            }
            RailAction::Prioritize { source, target } => {
                self.refresh_mobile_sessions();
                if self.mobile_sessions.prioritize(source, target) {
                    let order = self.mobile_sessions.saved_order();
                    lazybox_config::Config::save_with_async(move |config| {
                        config.ui.mobile_session_order = order;
                    });
                } else {
                    self.flash_info("That session has ended");
                }
                self.mobile_rail.update(self.mobile_sessions.rows());
                self.mobile_rail.highlight_initial(source);
            }
            RailAction::New => self.new_mobile_session(),
            RailAction::Links(id) => self.copy_mobile_link(id),
            RailAction::Quit => self.quit = true,
            RailAction::Rename(id) | RailAction::Delete(id) => {
                let delete = matches!(action, RailAction::Delete(_));
                self.refresh_mobile_sessions();
                if self
                    .mobile_sessions
                    .rows()
                    .iter()
                    .any(|r| r.terminal_id == id)
                {
                    self.mobile_sessions.select(id);
                    if delete {
                        self.delete_mobile_selection();
                    } else {
                        self.rename_mobile_selection();
                    }
                }
            }
            RailAction::Select(id) => {
                self.mobile_rail.close();
                self.refresh_mobile_sessions();
                // The captured target can disappear while the overlay is open.
                // Never allow the list's nearby fallback to select another session.
                if self
                    .mobile_sessions
                    .rows()
                    .iter()
                    .any(|r| r.terminal_id == id)
                {
                    self.mobile_sessions.select(id);
                    self.open_mobile_selection();
                } else {
                    self.flash_info("That session has ended");
                }
            }
        }
        self.redraw = true;
    }

    /// Progress must not take keyboard focus from an already visible terminal.
    /// Keep the minimal profile on the session list until the spawn lands.
    pub(super) fn mobile_worktree_progress(
        &mut self,
        session_key: &lazybox_core::SessionKey,
        status: &lazybox_ipc::WorktreeStepStatus,
    ) -> bool {
        if self.presentation != Presentation::Mobile {
            return false;
        }
        if let lazybox_ipc::WorktreeStepStatus::Failed(message) = status {
            use crate::realm::components::error::{Accent, ErrorModal};
            self.mount_modal(
                Id::Error,
                ErrorModal::new(
                    "New session",
                    Accent::error("Could not start"),
                    message.clone(),
                ),
            );
        } else {
            self.flash_info(format!("Starting {session_key}…"));
        }
        true
    }

    pub(super) fn mobile_modal_key(&mut self, key: &KeyEvent) -> bool {
        if self.presentation != Presentation::Mobile {
            return false;
        }
        if key.code == Key::Char('?')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.top_modal() == Some(&Id::Setup)
            && self.setup.runner.is_none()
        {
            self.mount_help_ask();
            return true;
        }
        false
    }

    fn scroll_mobile_history(&mut self, delta: isize) {
        if let Some(id) = self.terminals.focused_terminal_id() {
            self.terminal_selection = None;
            let _outcome = self.terminals.scroll_terminal(id, delta);
            if let Some(terminal_id) = self.terminals.take_scrollback_fetch() {
                self.send_cmd(Command::FetchScrollback { terminal_id });
            }
            self.redraw = true;
        }
    }

    /// Live mobile terminals own ordinary keys as input, just like a paste.
    /// A slow frame must not erase that input. Management keys and exited
    /// panes retain the stale-action guard (Enter can restart an exited pane).
    pub(super) fn mobile_retains_buffered_key(&self, event: &crossterm::event::Event) -> bool {
        use crossterm::event::{Event, KeyCode, KeyModifiers};
        let Event::Key(key) = event else {
            return false;
        };
        if self.presentation != Presentation::Mobile
            || !self.modal_stack.is_empty()
            || self.mobile_rail.is_open()
            || self.focus != PaneFocus::Terminals
            || (key.modifiers == KeyModifiers::CONTROL
                && matches!(key.code, KeyCode::Char('t' | 'g')))
        {
            return false;
        }
        self.terminals.terminal_summaries().iter().any(|terminal| {
            Some(terminal.id) == self.terminals.focused_terminal_id() && !terminal.exited
        })
    }

    pub(super) fn mobile_key(&mut self, key: &KeyEvent) -> bool {
        if self.presentation != Presentation::Mobile {
            return false;
        }
        if key.code == Key::Char('g') && key.modifiers == KeyModifiers::CONTROL {
            self.open_settings();
            return true;
        }
        if key.code == Key::Char('t') && key.modifiers == KeyModifiers::CONTROL {
            if self.key_kind != crossterm::event::KeyEventKind::Repeat {
                if self.mobile_rail.is_open() || self.focus != PaneFocus::Terminals {
                    self.apply_mobile_rail_action(RailAction::Close);
                } else {
                    self.open_mobile_sessions();
                }
                self.redraw = true;
            }
            return true;
        }
        if !self.mobile_rail.is_open() && self.focus == PaneFocus::Terminals {
            if key.code == Key::Char('d') && key.modifiers == KeyModifiers::CONTROL {
                self.terminal_selection = None;
                let _outcome = self.terminals.scroll_to_bottom();
                self.redraw = true;
                return true;
            }
            let mut ct = crate::realm::keymap::realm_key_to_crossterm(key);
            ct.kind = self.key_kind;
            let mut commands = Vec::new();
            self.terminals.handle_key_direct(ct, &mut commands);
            self.flush_dispatched_cmds(commands);
            self.redraw = true;
            return true;
        }
        if !self.mobile_rail.is_open() {
            self.refresh_mobile_sessions();
            self.mobile_rail.initialize(self.mobile_sessions.rows());
        }
        let action = self.mobile_rail.key(key);
        if let Some(id) = self.mobile_rail.highlighted() {
            self.mobile_sessions.select(id);
        }
        self.apply_mobile_rail_action(action);
        true
    }

    pub(super) fn mobile_mouse(&mut self, m: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        if self.presentation != Presentation::Mobile {
            return false;
        }
        let area = self.layout.last_area;
        let sessions = self.mobile_rail.is_open() || self.focus != PaneFocus::Terminals;
        if sessions {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if m.row == area.bottom().saturating_sub(1) =>
                {
                    // Same pinned actions in the overlay and startup portal.
                    let action = self
                        .mobile_rail
                        .footer_key(m.column.saturating_sub(area.x))
                        .map(|key| self.mobile_rail.key(&key))
                        .unwrap_or(RailAction::None);
                    self.apply_mobile_rail_action(action);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(id) = self.mobile_rail.at(m.column, m.row) {
                        let action = self.mobile_rail.choose(id);
                        self.apply_mobile_rail_action(action);
                    } else if self.mobile_rail.is_open()
                        && !self.mobile_rail.contains(m.column, m.row)
                    {
                        self.apply_mobile_rail_action(RailAction::Close);
                    }
                }
                MouseEventKind::ScrollDown => self.mobile_rail.move_highlight(1),
                MouseEventKind::ScrollUp => self.mobile_rail.move_highlight(-1),
                _ => (),
            }
            self.redraw = true;
            return true;
        }
        // The entire mobile screen belongs to the one running terminal,
        // including its narrow rail and chrome. Touch wheel coordinates can
        // stay at the edge throughout a swipe.
        if matches!(
            m.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.scroll_mobile_history(if m.kind == MouseEventKind::ScrollUp {
                -3
            } else {
                3
            });
            return true;
        }
        let (body, _) = super::split_for_footer(area);
        let (_, body) = crate::realm::presentation::mobile_header(body);
        let (rail, _) = crate::realm::presentation::mobile_terminal(body);
        if rail.contains((m.column, m.row).into())
            || m.row == area.y
            || m.row == area.bottom().saturating_sub(1)
        {
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.open_mobile_sessions();
            }
            return true;
        }
        matches!(m.kind, MouseEventKind::Down(MouseButton::Right))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::{
        Project, ProjectKey, SessionKey, SessionLayout, TileTree, Workspace, WorkspaceKey,
    };
    use lazybox_ipc::{Event, TerminalId, channel};
    use tuirealm::{
        event::KeyModifiers,
        ratatui::layout::{Rect, Size},
        terminal::TestTerminalAdapter,
    };

    fn fixture() -> (
        Model<TestTerminalAdapter>,
        lazybox_ipc::Connection,
        SessionKey,
    ) {
        let (client, server) = channel::pair();
        let mut m = Model::new_for_test(client, Size::new(39, 18))
            .unwrap()
            .with_presentation(Presentation::Mobile);
        m.set_agents(vec!["codex".into()]);
        let workspace = Workspace::empty(
            WorkspaceKey::new("scratch:phone"),
            "phone",
            chrono::Utc::now(),
        );
        let key = SessionKey::from(&workspace.key);
        m.handle_daemon_event(Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));
        m.sidebar.focus_workspace_key(&key);
        m.sync_panes();
        (m, server, key)
    }
    fn spawn(m: &mut Model<TestTerminalAdapter>, key: SessionKey, id: u64) {
        m.handle_daemon_event(Event::TerminalSpawned {
            terminal_id: TerminalId(id),
            session_key: key,
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
            agent_state: Some(AgentState::Idle),
        });
        m.sync_panes();
    }
    fn key(c: char) -> KeyEvent {
        KeyEvent::from(Key::Char(c))
    }
    fn list_key() -> KeyEvent {
        KeyEvent::new(Key::Char('t'), KeyModifiers::CONTROL)
    }

    #[test]
    fn priority_and_enter_use_highlight_without_touching_running_terminal() {
        let _env = super::super::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct ConfigHome(Option<std::ffi::OsString>, tempfile::TempDir);
        impl Drop for ConfigHome {
            fn drop(&mut self) {
                let _ =
                    lazybox_config::Config::flush_pending_saves(std::time::Duration::from_secs(5));
                // SAFETY: the crate's ENV_LOCK is held until after this guard drops.
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("LAZYBOX_HOME", value),
                        None => std::env::remove_var("LAZYBOX_HOME"),
                    }
                }
                lazybox_config::Config::invalidate_cache();
            }
        }
        let home = ConfigHome(
            std::env::var_os("LAZYBOX_HOME"),
            tempfile::tempdir().unwrap(),
        );
        // SAFETY: serialize home changes with the other model config tests.
        unsafe {
            std::env::set_var("LAZYBOX_HOME", home.1.path());
        }
        lazybox_config::Config::invalidate_cache();
        let (mut m, mut server, workspace) = fixture();
        for id in [7, 8, 9] {
            spawn(&mut m, workspace.clone(), id);
        }
        m.terminals.focus_terminal(TerminalId(7));
        m.set_focus(PaneFocus::Terminals);
        m.view();
        while server.rx.try_recv().is_ok() {}
        m.dispatch_key(rail_key());
        for c in ['j', 'j', 'p', 'a'] {
            m.dispatch_key(key(c));
        }
        m.view();
        assert_eq!(
            m.mobile_sessions
                .rows()
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [9, 7, 8]
        );
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        assert!(m.mobile_rail.is_open());
        assert_eq!(m.mobile_rail.highlighted(), Some(TerminalId(9)));
        assert!(lazybox_config::Config::flush_pending_saves(
            std::time::Duration::from_secs(5)
        ));
        let saved = lazybox_config::Config::load().unwrap();
        let (mut fresh, _connection, fresh_workspace) = fixture();
        fresh.apply_client_config(&saved);
        // Attachment streams terminal events, potentially in another order.
        // Empty and partial rosters must not discard the remaining priorities.
        fresh.refresh_mobile_sessions();
        for id in [8, 7, 9] {
            spawn(&mut fresh, fresh_workspace.clone(), id);
            fresh.refresh_mobile_sessions();
        }
        assert_eq!(
            fresh
                .mobile_sessions
                .rows()
                .iter()
                .map(|r| r.terminal_id.0)
                .collect::<Vec<_>>(),
            [9, 7, 8]
        );
        fresh.dispatch_key(KeyEvent::from(Key::Enter));
        assert_eq!(fresh.terminals.focused_terminal_id(), Some(TerminalId(9)));
        m.dispatch_key(key('p'));
        m.dispatch_key(key('j'));
        m.dispatch_key(KeyEvent::from(Key::Esc));
        assert!(!m.mobile_rail.is_prioritizing());
        assert!(m.mobile_rail.is_open());
        assert_eq!(m.mobile_sessions.rows()[0].terminal_id, TerminalId(9));
        m.dispatch_key(key('j'));
        m.dispatch_key(KeyEvent::from(Key::Enter));
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(8)));
        assert!(!m.mobile_rail.is_open());
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                !matches!(
                    cmd,
                    Command::Write { .. } | Command::Close { .. } | Command::Resize { .. }
                ),
                "{cmd:?}"
            );
        }
    }

    #[test]
    fn rename_clear_requires_save_and_escape_preserves_original() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        let original_name = m.sidebar.selected_workspace().unwrap().name.clone();
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(rail_key());
        m.dispatch_key(key('r'));
        assert_eq!(m.top_modal(), Some(&Id::RenameWorkspace));
        while server.rx.try_recv().is_ok() {}
        m.dispatch_modal_key(KeyEvent::new(Key::Char('x'), KeyModifiers::CONTROL));
        m.dispatch_modal_key(KeyEvent::from(Key::Enter));
        assert_eq!(m.top_modal(), Some(&Id::RenameWorkspace));
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert_eq!(m.sidebar.selected_workspace().unwrap().name, original_name);
        m.dispatch_key(key('r'));
        assert_eq!(m.top_modal(), Some(&Id::RenameWorkspace));
        m.dispatch_modal_key(KeyEvent::new(Key::Char('x'), KeyModifiers::CONTROL));
        for c in "box".chars() {
            m.dispatch_modal_key(key(c));
        }
        m.dispatch_modal_key(KeyEvent::from(Key::Enter));
        assert!(m.top_modal().is_none());
        let commands: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
        assert!(
            matches!(commands.as_slice(), [Command::RenameWorkspace { name, .. }] if name == "box")
        );
    }

    #[test]
    fn terminal_typing_is_literal_and_control_t_opens_sessions() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        m.set_focus(PaneFocus::Terminals);
        while server.rx.try_recv().is_ok() {}
        for c in "njksraf?m]]".chars() {
            m.dispatch_key(key(c));
        }
        m.dispatch_key(KeyEvent::new(Key::Char('u'), KeyModifiers::CONTROL));
        let mut text = Vec::new();
        while let Ok(cmd) = server.rx.try_recv() {
            if let Command::Write { bytes, .. } = cmd {
                text.extend(bytes);
            }
        }
        assert_eq!(text, b"njksraf?m]]\x15");
        m.dispatch_key(list_key());
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(m.mobile_rail.is_open());
        assert!(m.modal_stack.is_empty());
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(!matches!(cmd, Command::Write { .. }));
        }
    }

    #[test]
    fn mobile_links_empty_view_shows_a_dismissible_sheet() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(list_key());
        while server.rx.try_recv().is_ok() {}
        m.dispatch_key(key('/'));
        assert_eq!(m.top_modal(), Some(&Id::MobileLinks));
        m.dispatch_modal_key(KeyEvent::from(Key::Enter));
        assert!(m.top_modal().is_none());
        assert!(m.mobile_rail.is_open());
        assert!(server.rx.try_recv().is_err());
    }

    #[test]
    fn mobile_links_target_the_hovered_session_without_sending_terminal_input() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace.clone(), 7);
        spawn(&mut m, workspace, 8);
        for (id, url) in [
            (7, "https://first.example/path"),
            (8, "https://second.example/path"),
        ] {
            m.handle_daemon_event(Event::TerminalOutput {
                terminal_id: TerminalId(id),
                bytes: std::sync::Arc::<[u8]>::from(format!("{url}\r\n").into_bytes()),
                first_seq: 1,
                seq: 1,
                cols: 20,
                rows: 12,
            });
        }
        m.terminals.focus_terminal(TerminalId(7));
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(list_key());
        m.dispatch_key(KeyEvent::from(Key::Down));
        while server.rx.try_recv().is_ok() {}
        m.dispatch_key(key('/'));
        assert_eq!(m.top_modal(), Some(&Id::MobileLinks));
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert!(m.mobile_rail.is_open());
        assert_eq!(m.mobile_rail.highlighted(), Some(TerminalId(8)));
        m.dispatch_key(key('/'));
        m.dispatch_modal_key(KeyEvent::from(Key::Enter));
        assert!(m.top_modal().is_none());
        assert!(!m.mobile_rail.is_open());
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(!matches!(
                cmd,
                Command::Write { .. } | Command::Resize { .. }
            ));
        }
        // Outside Sessions, slash is ordinary terminal input.
        m.dispatch_key(key('/'));
        assert!(
            std::iter::from_fn(|| server.rx.try_recv().ok())
                .any(|cmd| matches!(cmd, Command::Write { bytes, .. } if bytes == b"/"))
        );
    }

    #[test]
    fn control_d_returns_from_deep_touch_scrollback_without_writing_to_the_session() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        m.set_focus(PaneFocus::Terminals);
        m.handle_daemon_event(Event::TerminalOutput {
            terminal_id: TerminalId(7),
            bytes: std::sync::Arc::<[u8]>::from(
                (0..200)
                    .map(|i| format!("history {i}\r\n"))
                    .collect::<String>()
                    .into_bytes(),
            ),
            first_seq: 1,
            seq: 1,
            cols: 38,
            rows: 16,
        });
        m.view();
        let offset = |m: &Model<TestTerminalAdapter>| -> u64 {
            m.terminals
                .scrollbar_summary()
                .unwrap()
                .split_whitespace()
                .find_map(|part| part.strip_prefix("offset="))
                .unwrap()
                .parse()
                .unwrap()
        };
        let bottom = offset(&m);
        assert!(bottom > 60, "fixture must have deep history");
        for _ in 0..20 {
            m.handle_mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp,
                column: 0,
                row: 5,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
        }
        assert!(offset(&m) + 50 < bottom, "touch reports must scroll far up");
        while server.rx.try_recv().is_ok() {}
        for _ in 0..2 {
            m.dispatch_key(KeyEvent::new(Key::Char('d'), KeyModifiers::CONTROL));
            assert_eq!(offset(&m), bottom, "one Ctrl-D reaches the live bottom");
            while let Ok(cmd) = server.rx.try_recv() {
                assert!(
                    !matches!(cmd, Command::Write { .. }),
                    "Ctrl-D must not send EOF"
                );
            }
        }
    }

    #[test]
    fn sessions_cancel_preserves_terminal_and_letters_switch_the_exact_split() {
        let (mut m, _server, workspace) = fixture();
        spawn(&mut m, workspace.clone(), 7);
        spawn(&mut m, workspace, 8);
        m.terminals.set_layout(SessionLayout::Splits {
            tree: TileTree::VSplit {
                top: Box::new(TileTree::Leaf { terminal_id: 7 }),
                bottom: Box::new(TileTree::Leaf { terminal_id: 8 }),
                ratio: 50,
            },
            focused: vec![0],
        });
        m.terminals.focus_terminal(TerminalId(7));
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(list_key());
        m.dispatch_key(KeyEvent::from(Key::Down));
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        m.dispatch_key(KeyEvent::from(Key::Esc));
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        m.dispatch_key(list_key());
        m.dispatch_key(KeyEvent::from(Key::Down));
        m.dispatch_key(key('b'));
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(8)));
    }

    #[test]
    fn mobile_startup_progress_never_captures_terminal_input() {
        use lazybox_ipc::{SpawnOrigin, WorktreeStep, WorktreeStepStatus};
        let (mut m, mut server, workspace) = fixture();
        m.route_worktree_progress(
            workspace.clone(),
            WorktreeStep::Setup,
            WorktreeStepStatus::Done,
            SpawnOrigin::Interactive,
        );
        assert!(m.top_modal().is_none());
        spawn(&mut m, workspace.clone(), 7);
        m.set_focus(PaneFocus::Terminals);
        // A late progress event must not remount a keyboard-grabbing checklist.
        m.route_worktree_progress(
            workspace,
            WorktreeStep::Setup,
            WorktreeStepStatus::Done,
            SpawnOrigin::Interactive,
        );
        assert!(m.top_modal().is_none());
        while server.rx.try_recv().is_ok() {}
        m.dispatch_key(key('a'));
        assert!(
            matches!(server.rx.try_recv(), Ok(Command::Write { terminal_id: TerminalId(7), bytes, .. }) if bytes == b"a")
        );
        m.dispatch_key(list_key());
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(m.mobile_rail.is_open());
    }

    #[test]
    fn mobile_startup_failure_remains_readable_and_dismissible() {
        let (mut m, _server, workspace) = fixture();
        m.route_worktree_progress(
            workspace,
            lazybox_ipc::WorktreeStep::Setup,
            lazybox_ipc::WorktreeStepStatus::Failed("Agent is unavailable".into()),
            lazybox_ipc::SpawnOrigin::Interactive,
        );
        assert_eq!(m.top_modal(), Some(&Id::Error));
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert!(m.top_modal().is_none());
    }

    #[test]
    fn mobile_list_ignores_desktop_feature_shortcuts() {
        let (mut m, mut server, _) = fixture();
        while server.rx.try_recv().is_ok() {}
        for c in "afxmh,?".chars() {
            m.dispatch_key(key(c));
        }
        m.dispatch_key(KeyEvent::from(Key::Tab));
        assert!(m.modal_stack.is_empty());
        assert_eq!(m.focus, PaneFocus::Sidebar);
        assert!(server.rx.try_recv().is_err());
    }

    #[test]
    fn cancel_creation_preserves_session_and_emits_no_create() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(list_key());
        while server.rx.try_recv().is_ok() {}
        m.dispatch_key(key('n'));
        assert_eq!(m.top_modal(), Some(&Id::MobileNewSession));
        m.dispatch_modal_key(KeyEvent::from(Key::Enter));
        assert_eq!(m.top_modal(), Some(&Id::MobileRunner));
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert_eq!(m.top_modal(), Some(&Id::MobileNewSession));
        assert!(m.modal_flow.is_none());
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert!(m.modal_stack.is_empty());
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(!matches!(
                cmd,
                Command::CreateWorkspace { .. }
                    | Command::CreateProject { .. }
                    | Command::Spawn { .. }
            ));
        }
    }

    #[test]
    fn no_repo_creation_bootstraps_scratch_then_starts_default_agent() {
        let (mut m, mut server, _) = fixture();
        m.new_mobile_session();
        assert!(
            m.mobile_new_session_picked(&[ChoicePayload::Text("scratch".into())])
                .is_empty()
        );
        let cmds = m.mobile_runner_picked(&[ChoicePayload::Text("codex".into())]);
        assert!(matches!(cmds.as_slice(),[Command::CreateProject {name}] if name=="scratch"));
        while server.rx.try_recv().is_ok() {}
        let project = Project::new(ProjectKey::local("scratch"), "scratch", chrono::Utc::now());
        m.handle_daemon_event(Event::ProjectUpserted(Box::new(project)));
        let mut created = false;
        while let Ok(cmd) = server.rx.try_recv() {
            if let Command::CreateWorkspace {
                project_key,
                spawn_agent,
                scratch,
                ..
            } = cmd
            {
                assert_eq!(project_key, ProjectKey::local("scratch"));
                assert_eq!(spawn_agent.as_deref(), Some("codex"));
                assert!(scratch);
                created = true;
            }
        }
        assert!(created);
    }

    #[test]
    fn repository_creation_uses_the_chosen_project_and_correlation_id() {
        let (mut m, _server, _) = fixture();
        let project = Project::github("team", "repo", chrono::Utc::now());
        let project_key = project.key.clone();
        m.handle_daemon_event(Event::ProjectUpserted(Box::new(project)));
        m.set_agents(vec!["codex".into(), "claude".into()]);
        m.new_mobile_session();
        assert!(
            m.mobile_new_session_picked(&[ChoicePayload::Project(project_key.clone())])
                .is_empty()
        );
        let cmds = m.mobile_runner_picked(&[ChoicePayload::Text("claude".into())]);
        assert!(
            matches!(cmds.as_slice(),[Command::CreateWorkspace {project_key:key,spawn_agent:Some(agent),client_request_id:Some(_),..}] if key==&project_key && agent=="claude")
        );
        assert_eq!(m.sidebar.default_agent(), "codex");
        assert!(m.modal_stack.is_empty());
        assert!(m.modal_flow.is_none());
    }

    fn hover_other_chat(m: &mut Model<TestTerminalAdapter>, original: SessionKey) -> SessionKey {
        spawn(m, original, 7);
        let workspace =
            Workspace::empty(WorkspaceKey::new("other-chat"), "main", chrono::Utc::now());
        let target = SessionKey::from(&workspace.key);
        m.handle_daemon_event(Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));
        spawn(m, target.clone(), 9);
        m.terminals.focus_terminal(TerminalId(7));
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(list_key());
        m.dispatch_key(KeyEvent::from(Key::Down));
        assert_eq!(m.mobile_sessions.selected().unwrap().session_key, target);
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        target
    }

    #[test]
    fn rename_targets_hovered_chat_and_cancel_preserves_current_terminal() {
        let (mut m, _server, original) = fixture();
        let target = hover_other_chat(&mut m, original);
        m.dispatch_key(key('r'));
        assert_eq!(m.top_modal(), Some(&Id::RenameWorkspace));
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert!(m.modal_flow.is_none());
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        m.dispatch_key(key('r'));
        let cmds = m.handle_input_submitted("  Phone plan  ".into());
        assert!(
            matches!(cmds.as_slice(), [Command::RenameWorkspace { session_key, name }]
            if session_key == &target && name == "Phone plan")
        );
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
    }

    #[test]
    fn shell_creation_waits_for_correlated_workspace_and_starts_exactly_once() {
        let (mut m, mut server, _) = fixture();
        m.set_agents(vec![]);
        m.new_mobile_session();
        assert!(
            m.mobile_new_session_picked(&[ChoicePayload::Text("scratch".into())])
                .is_empty()
        );
        let cmds = m.mobile_runner_picked(&[ChoicePayload::OptText(None)]);
        assert!(matches!(cmds.as_slice(), [Command::CreateProject { name }] if name == "scratch"));
        while server.rx.try_recv().is_ok() {}
        m.handle_daemon_event(Event::ProjectUpserted(Box::new(Project::new(
            ProjectKey::local("scratch"),
            "scratch",
            chrono::Utc::now(),
        ))));
        let cmds: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
        let create_id = cmds
            .iter()
            .find_map(|cmd| match cmd {
                Command::CreateWorkspace {
                    spawn_agent,
                    client_request_id,
                    ..
                } => {
                    assert!(spawn_agent.is_none());
                    client_request_id.clone()
                }
                _ => None,
            })
            .expect("correlated workspace create");
        let key = WorkspaceKey::new("daemon-allocated-collision-4");
        let workspace = Workspace::empty(key.clone(), "main", chrono::Utc::now());
        m.handle_daemon_event(Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));
        while server.rx.try_recv().is_ok() {}
        m.handle_daemon_event(Event::WorkspaceCreated {
            client_request_id: "other-client".into(),
            workspace_key: key.clone(),
        });
        assert!(server.rx.try_recv().is_err());
        m.handle_daemon_event(Event::WorkspaceCreated {
            client_request_id: create_id.clone(),
            workspace_key: key.clone(),
        });
        let cmds: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
        let spawn_id = cmds
            .iter()
            .find_map(|cmd| match cmd {
                Command::Spawn {
                    session_key,
                    kind,
                    client_request_id,
                    ..
                } => {
                    assert_eq!(session_key, &SessionKey::from(&key));
                    assert!(matches!(kind, TerminalKind::Shell));
                    client_request_id.clone()
                }
                _ => None,
            })
            .expect("correlated shell spawn");
        assert_ne!(create_id, spawn_id);
        m.handle_daemon_event(Event::WorkspaceCreated {
            client_request_id: create_id.clone(),
            workspace_key: key.clone(),
        });
        m.handle_daemon_event(Event::CommandCompleted {
            client_request_id: create_id,
        });
        assert!(
            server.rx.try_recv().is_err(),
            "duplicate creation acknowledgement must not spawn twice"
        );
        assert!(m.pending_workspace_creates.contains_key(&spawn_id));
        m.handle_daemon_event(Event::CommandFailed {
            client_request_id: spawn_id,
            message: "shell unavailable".into(),
        });
        assert!(m.pending_workspace_creates.is_empty());
        assert!(m.spawn_follow_to.is_none());
        assert!(
            m.status
                .notice
                .as_ref()
                .unwrap()
                .message
                .contains("shell failed to start")
        );
    }

    #[test]
    fn mobile_hit_rects_and_header_click_open_only_the_session_list() {
        let (mut m, _server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        let area = Rect::new(0, 0, 39, 18);
        m.layout.last_area = area;
        m.set_focus(PaneFocus::Sidebar);
        assert_eq!(
            m.effective_pane_rects(area),
            (Rect::new(0, 1, 39, 16), Rect::default(), Rect::default())
        );
        m.set_focus(PaneFocus::Terminals);
        m.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(m.mobile_rail.is_open());
        assert!(m.modal_stack.is_empty());
    }
    fn rail_key() -> KeyEvent {
        KeyEvent::new(Key::Char('t'), KeyModifiers::CONTROL)
    }

    #[test]
    fn rail_switches_exact_terminal_without_forwarding_keys_or_paste_or_resizing() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace.clone(), 7);
        spawn(&mut m, workspace, 8);
        m.terminals.focus_terminal(TerminalId(7));
        m.set_focus(PaneFocus::Terminals);
        m.view();
        let geometry = m.effective_pane_rects(Rect::new(0, 0, 39, 18));
        assert_eq!(geometry.2, Rect::new(1, 1, 38, 16));
        while server.rx.try_recv().is_ok() {}
        m.dispatch_key(rail_key());
        m.handle_paste("should never reach the shell");
        m.dispatch_key(key('?'));
        m.view();
        assert!(m.mobile_rail.is_open());
        assert_eq!(m.effective_pane_rects(Rect::new(0, 0, 39, 18)), geometry);
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(
                !matches!(cmd, Command::Write { .. } | Command::Resize { .. }),
                "{cmd:?}"
            );
        }
        m.dispatch_key(key('b'));
        assert!(!m.mobile_rail.is_open());
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(8)));
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(!matches!(cmd, Command::Write { .. }));
        }
        for dismiss in [
            KeyEvent::from(Key::Esc),
            rail_key(),
            KeyEvent::from(Key::Enter),
        ] {
            m.dispatch_key(rail_key());
            m.dispatch_key(dismiss);
            assert!(!m.mobile_rail.is_open());
            assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(8)));
        }
    }

    #[test]
    fn rail_letters_keep_their_targets_when_the_live_roster_changes() {
        let (mut m, _server, workspace) = fixture();
        spawn(&mut m, workspace.clone(), 7);
        spawn(&mut m, workspace.clone(), 8);
        m.terminals.focus_terminal(TerminalId(7));
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(rail_key());
        spawn(&mut m, workspace.clone(), 1);
        m.dispatch_key(key('b'));
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(8)));
        m.dispatch_key(rail_key()); // a=1, b=7, c=8
        m.terminals.close_terminal(TerminalId(7), &mut Vec::new());
        m.handle_daemon_event(Event::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: Some(0),
            last_output: None,
        });
        m.dispatch_key(key('b'));
        assert_eq!(
            m.terminals.focused_terminal_id(),
            Some(TerminalId(8)),
            "a vanished target must not select the next row"
        );
    }

    #[test]
    fn rail_status_tracks_each_terminal_independently() {
        let (mut m, _server, workspace) = fixture();
        spawn(&mut m, workspace.clone(), 7);
        spawn(&mut m, workspace.clone(), 8);
        m.refresh_mobile_sessions();
        assert_eq!(m.mobile_sessions.rows()[0].indicator().0, "○");
        for (state, glyph) in [
            (AgentState::Working, "●"),
            (AgentState::InputNeeded, "!"),
            (AgentState::Done, "✓"),
        ] {
            m.handle_daemon_event(Event::AgentState {
                session_key: workspace.clone(),
                terminal_id: TerminalId(7),
                state,
            });
            m.refresh_mobile_sessions();
            assert_eq!(m.mobile_sessions.rows()[0].indicator().0, glyph);
            assert_eq!(m.mobile_sessions.rows()[1].indicator().0, "○");
        }
    }

    #[test]
    fn delete_requires_explicit_yes_and_only_closes_the_captured_terminal() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace.clone(), 7);
        spawn(&mut m, workspace, 8);
        m.terminals.focus_terminal(TerminalId(7));
        m.set_focus(PaneFocus::Terminals);
        m.dispatch_key(list_key());
        m.dispatch_key(KeyEvent::from(Key::Down));
        while server.rx.try_recv().is_ok() {}
        for cancel in [
            KeyEvent::from(Key::Esc),
            KeyEvent::from(Key::Enter),
            key('n'),
        ] {
            m.dispatch_key(key('x'));
            assert_eq!(m.top_modal(), Some(&Id::MobileDeleteSession));
            m.dispatch_modal_key(key('x')); // held/repeated opening key is harmless
            assert_eq!(m.top_modal(), Some(&Id::MobileDeleteSession));
            m.dispatch_modal_key(cancel);
            assert!(m.top_modal().is_none());
            assert!(m.modal_flow.is_none());
        }
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(!matches!(cmd, Command::Close { .. }));
        }
        m.dispatch_key(key('x'));
        m.mobile_sessions.select(TerminalId(7)); // live cursor cannot retarget the prompt
        m.dispatch_modal_key(key('y'));
        let mut closed = Vec::new();
        while let Ok(cmd) = server.rx.try_recv() {
            if let Command::Close { terminal_id, .. } = cmd {
                closed.push(terminal_id);
            }
        }
        assert_eq!(closed, vec![TerminalId(8)]);
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        assert_eq!(m.focus, PaneFocus::Terminals);
        assert!(m.mobile_rail.is_open());
        m.handle_daemon_event(Event::TerminalExited {
            terminal_id: TerminalId(8),
            exit_code: Some(0),
            last_output: None,
        });
        m.refresh_mobile_sessions();
        assert_eq!(
            m.mobile_sessions.len(),
            1,
            "confirmed agent close must remove the frozen pane too"
        );
        assert_eq!(m.mobile_sessions.rows()[0].terminal_id, TerminalId(7));
    }

    #[test]
    fn deleting_an_already_exited_terminal_removes_it_locally() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        m.handle_daemon_event(Event::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: Some(1),
            last_output: Some("failed".into()),
        });
        m.open_mobile_sessions();
        assert_eq!(m.mobile_sessions.len(), 1);
        while server.rx.try_recv().is_ok() {}
        m.dispatch_key(key('x'));
        m.dispatch_modal_key(key('y'));
        assert_eq!(m.mobile_sessions.len(), 0);
        while let Ok(cmd) = server.rx.try_recv() {
            assert!(!matches!(cmd, Command::Close { .. }));
        }
    }

    #[test]
    fn rail_mouse_dismissal_is_consumed_and_does_not_click_the_terminal() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace, 7);
        m.set_focus(PaneFocus::Terminals);
        m.view();
        let mouse = |column, row| crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(m.mobile_mouse(mouse(0, 1)));
        assert!(m.mobile_rail.is_open());
        m.view();
        while server.rx.try_recv().is_ok() {}
        assert!(m.mobile_mouse(mouse(35, 5)));
        assert!(!m.mobile_rail.is_open());
        assert!(server.rx.try_recv().is_err());
    }
    #[test]
    fn paste_after_a_mobile_split_switch_reaches_the_selected_terminal() {
        let (mut m, mut server, workspace) = fixture();
        spawn(&mut m, workspace.clone(), 7);
        spawn(&mut m, workspace, 8);
        m.terminals.set_layout(SessionLayout::Splits {
            tree: TileTree::VSplit {
                top: Box::new(TileTree::Leaf { terminal_id: 7 }),
                bottom: Box::new(TileTree::Leaf { terminal_id: 8 }),
                ratio: 50,
            },
            focused: vec![0],
        });
        m.set_focus(PaneFocus::Terminals);
        assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
        // Framing follows the focused program's DECSET 2004 mode, not
        // the last-spawned split's mode or the host terminal's mode.
        for (seq, bracketed) in [(1, false), (2, true)] {
            for (id, enabled) in [(7, bracketed), (8, !bracketed)] {
                let mode = if enabled {
                    b"\x1b[?2004h"
                } else {
                    b"\x1b[?2004l"
                };
                m.terminals.on_daemon_event(&Event::TerminalOutput {
                    terminal_id: TerminalId(id),
                    bytes: std::sync::Arc::<[u8]>::from(mode.to_vec()),
                    first_seq: seq,
                    seq,
                    cols: 0,
                    rows: 0,
                });
            }
            while server.rx.try_recv().is_ok() {}
            m.handle_paste("hello");
            let mut writes = Vec::new();
            while let Ok(cmd) = server.rx.try_recv() {
                if let Command::Write {
                    terminal_id, bytes, ..
                } = cmd
                {
                    writes.push((terminal_id, bytes));
                }
            }
            let expected = if bracketed {
                b"\x1b[200~hello\x1b[201~".to_vec()
            } else {
                b"hello".to_vec()
            };
            assert_eq!(writes, vec![(TerminalId(7), expected)]);
        }
    }
    #[test]
    fn portal_and_overlay_share_actions_and_control_g_opens_settings() {
        for overlay in [false, true] {
            let (mut m, mut server, workspace) = fixture();
            spawn(&mut m, workspace.clone(), 7);
            spawn(&mut m, workspace, 8);
            m.terminals.focus_terminal(TerminalId(7));
            if overlay {
                m.set_focus(PaneFocus::Terminals);
                m.dispatch_key(rail_key());
            } else {
                m.set_focus(PaneFocus::Sidebar);
            }
            m.view();
            m.dispatch_key(KeyEvent::from(Key::Down));
            m.dispatch_key(key('r'));
            assert_eq!(m.top_modal(), Some(&Id::RenameWorkspace));
            m.dispatch_modal_key(KeyEvent::from(Key::Esc));
            m.dispatch_key(key('x'));
            assert_eq!(m.top_modal(), Some(&Id::MobileDeleteSession));
            m.dispatch_modal_key(KeyEvent::from(Key::Esc));
            m.dispatch_key(key('n'));
            assert_eq!(m.top_modal(), Some(&Id::MobileNewSession));
            m.dispatch_modal_key(KeyEvent::from(Key::Esc));
            assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(7)));
            m.dispatch_key(key('b'));
            assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(8)));
            assert!(!m.mobile_rail.is_open());
            while server.rx.try_recv().is_ok() {}
            m.cache_persisted_setup(lazybox_core::PersistedSetup::default());
            m.dispatch_key(KeyEvent::new(Key::Char('g'), KeyModifiers::CONTROL));
            assert!(!m.mobile_rail.is_open());
            assert_eq!(m.top_modal(), Some(&Id::Setup));
            while let Ok(cmd) = server.rx.try_recv() {
                assert!(!matches!(cmd, Command::Write { .. }));
            }
            m.dispatch_modal_key(key('?'));
            assert_eq!(m.top_modal(), Some(&Id::HelpAsk));
            m.dispatch_modal_key(KeyEvent::from(Key::Esc));
            assert_eq!(m.top_modal(), Some(&Id::Setup));
            m.dispatch_modal_key(KeyEvent::from(Key::Esc));
            m.dispatch_key(rail_key());
            m.dispatch_key(KeyEvent::from(Key::Enter));
            assert!(!m.mobile_rail.is_open());
            assert_eq!(m.terminals.focused_terminal_id(), Some(TerminalId(8)));
            m.dispatch_key(rail_key());
            m.dispatch_key(KeyEvent::new(Key::Char('q'), KeyModifiers::CONTROL));
            assert!(m.quit);
        }
    }
}
