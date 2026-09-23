//! Phone-sized render and workflow regressions, using the production model.
mod common;
use lazybox_ipc::channel;
use lazybox_tui::realm::presentation::Presentation;
use lazybox_tui::realm::{
    Id, Model,
    components::{
        choice::Choice,
        error::{Accent, ErrorModal},
        input::Input,
        loading::Loading,
        splash::Splash,
    },
};
use tuirealm::{
    event::{Key, KeyEvent, KeyModifiers},
    ratatui::layout::Size,
    terminal::{TerminalAdapter, TestTerminalAdapter},
};

fn model(w: u16, h: u16, profile: Presentation) -> Model<TestTerminalAdapter> {
    let (client, _server) = channel::pair();
    Model::new_for_test(client, Size::new(w, h))
        .unwrap()
        .with_presentation(profile)
}
fn screen(m: &mut Model<TestTerminalAdapter>) -> String {
    m.view();
    let b = m.terminal.raw().backend().buffer().clone();
    (0..b.area.height)
        .map(|y| {
            (0..b.area.width)
                .map(|x| b[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn key(c: char) -> KeyEvent {
    KeyEvent::from(Key::Char(c))
}

#[test]
fn minimal_mobile_can_start_without_a_repository_or_setup() {
    let mut m = model(39, 18, Presentation::Mobile);
    let out = screen(&mut m);
    assert!(out.contains("No sessions yet"), "{out}");
    assert!(
        !out.contains("Activity") && !out.contains("Settings"),
        "{out}"
    );
    m.dispatch_key(key('n'));
    assert_eq!(m.top_modal(), Some(&Id::MobileNewSession));
    let out = screen(&mut m);
    assert!(out.contains("No repository"), "{out}");
    assert!(!out.contains("Project…"), "{out}");
    m.dispatch_modal_key(KeyEvent::from(Key::Esc));
    assert!(m.top_modal().is_none());
}

#[test]
fn mobile_runner_picker_preserves_area_on_back_and_always_offers_shell() {
    for (w, h) in [(32, 12), (39, 18)] {
        let mut m = model(w, h, Presentation::Mobile);
        m.set_agents(vec!["codex".into(), "claude".into()]);
        m.handle_daemon_event(lazybox_ipc::Event::ProjectUpserted(Box::new(
            lazybox_core::Project::github("team", "repo", chrono::Utc::now()),
        )));
        m.dispatch_key(key('n'));
        m.dispatch_modal_key(key('j'));
        let area = screen(&mut m);
        m.dispatch_modal_key(KeyEvent::from(Key::Enter));
        assert_eq!(m.top_modal(), Some(&Id::MobileRunner));
        let out = screen(&mut m);
        for label in ["codex", "claude", "Shell", "Enter", "Esc"] {
            assert!(out.contains(label), "{w}x{h}: {out}");
        }
        if w == 39 {
            insta::assert_snapshot!("mobile_runner_39x18", out);
        }
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert_eq!(m.top_modal(), Some(&Id::MobileNewSession));
        assert_eq!(screen(&mut m), area, "back preserves the area selection");
        m.set_agents(vec![]);
        m.dispatch_modal_key(KeyEvent::from(Key::Enter));
        let out = screen(&mut m);
        assert!(out.contains("Shell"), "{out}");
        assert!(!out.contains("codex"), "{out}");
    }
}

#[test]
fn desktop_n_does_not_open_mobile_creation() {
    let mut m = model(120, 40, Presentation::Desktop);
    m.dispatch_key(key('n'));
    assert_ne!(m.top_modal(), Some(&Id::MobileNewSession));
}

#[test]
fn onboarding_and_name_inputs_keep_controls_visible_at_phone_sizes() {
    for (w, h) in [(32, 12), (39, 18), (60, 20)] {
        let mut m = model(w, h, Presentation::Mobile);
        m.mount_modal(Id::Splash, Splash::new());
        let out = screen(&mut m);
        assert!(out.contains("Enter setup"), "{w}x{h}: {out}");
        m.mount_modal(Id::Setup,Choice::multi("Choose which agents should run in your workspace. Full installation guidance is available here.",vec!["Claude","Codex","Shell"]).label(|s|s.to_string()));
        let out = screen(&mut m);
        assert!(
            out.contains("Space pick") && out.contains("Enter next"),
            "{w}x{h}: {out}"
        );
        m.dispatch_modal_key(key('h'));
        let out = screen(&mut m);
        assert!(out.contains("h/Esc back"), "{out}");
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
        assert_eq!(m.top_modal(), Some(&Id::Setup));
        m.mount_modal(
            Id::RenameWorkspace,
            Input::new("A long workspace rename prompt which needs to wrap on the phone")
                .with_input("very-long-session-name-with-a-visible-END"),
        );
        let out = screen(&mut m);
        assert!(
            out.contains("Enter save") && out.contains("END"),
            "{w}x{h}: {out}"
        );
        m.mount_modal(
            Id::Error,
            ErrorModal::new(
                "GitHub",
                Accent::error("offline"),
                "A long error message.\n".repeat(30),
            ),
        );
        let out = screen(&mut m);
        assert!(out.contains("Enter/Esc close"), "{out}");
        let (loading, _sender) = Loading::pending("Discovering repositories");
        m.mount_modal(Id::Setup, loading);
        let out = screen(&mut m);
        assert!(out.contains("Esc cancel"), "{out}");
    }
}

#[test]
fn tiny_mobile_viewports_do_not_panic() {
    for (w, h) in [(0, 0), (1, 1), (20, 4)] {
        let mut m = model(w, h, Presentation::Mobile);
        m.mount_modal(
            Id::Setup,
            Choice::single("Choose", vec!["one"]).label(|s| s.to_string()),
        );
        screen(&mut m);
    }
}

#[test]
fn minimal_mobile_list_and_creation_snapshots() {
    let mut m = model(39, 18, Presentation::Mobile);
    insta::assert_snapshot!("mobile_empty_sessions_39x18", screen(&mut m));
    m.dispatch_key(key('n'));
    insta::assert_snapshot!("mobile_new_session_39x18", screen(&mut m));
}

#[test]
fn session_list_shows_live_status_and_omits_inbox_only_workspaces() {
    use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
    use lazybox_ipc::{AgentState, Event, TerminalId, TerminalKind};
    let mut m = model(39, 18, Presentation::Mobile);
    let mut workspace = Workspace::empty(WorkspaceKey::new("phone"), "phone", chrono::Utc::now());
    workspace.name = "Phone notes".into();
    let sk = SessionKey::from(&workspace.key);
    m.handle_daemon_event(Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));
    assert!(screen(&mut m).contains("No sessions yet"));
    m.handle_daemon_event(Event::TerminalSpawned {
        terminal_id: TerminalId(3),
        session_key: sk,
        kind: TerminalKind::Agent("codex".into()),
        no_permission: false,
        on_main: false,
        model_label: None,
        agent_state: Some(AgentState::Working),
    });
    let out = screen(&mut m);
    assert!(
        out.contains("Phone notes") && out.contains("working · codex"),
        "{out}"
    );
    insta::assert_snapshot!("mobile_sessions_39x18", out);
    m.dispatch_key(KeyEvent::from(Key::Enter));
    assert_eq!(m.focus(), lazybox_tui::realm::model::PaneFocus::Terminals);
    let out = screen(&mut m);
    assert!(out.contains("Ctrl-T Sessions | Phone notes"), "{out}");
    m.dispatch_key(KeyEvent::new(Key::Char('t'), KeyModifiers::CONTROL));
    assert_eq!(m.focus(), lazybox_tui::realm::model::PaneFocus::Terminals);
    assert!(m.top_modal().is_none());
}

#[test]
fn mobile_picker_details_reveal_the_complete_selected_repo_name() {
    let mut m = model(32, 12, Presentation::Mobile);
    m.mount_modal(
        Id::Setup,
        Choice::single(
            "Pick a repository",
            vec!["team/very-long-shared-prefix-important-suffix"],
        )
        .label(|s| s.to_string()),
    );
    screen(&mut m);
    m.dispatch_modal_key(key('h'));
    let out = screen(&mut m);
    let body = out
        .lines()
        .filter(|line| line.starts_with('│'))
        .map(|line| line.trim_matches('│').trim())
        .collect::<String>();
    assert!(
        body.contains("team/very-long-shared-prefix-important-suffix"),
        "{out}"
    );
}

#[test]
fn grouped_mobile_setup_shows_bulk_selection_state_and_controls() {
    let mut m = model(39, 18, Presentation::Mobile);
    m.mount_modal(
        Id::Setup,
        Choice::multi(
            "Choose inbox items",
            vec!["Authored PRs", "Review requests", "Assigned issues"],
        )
        .title("Setup · filters")
        .label(|s| s.to_string())
        .section_for(|s| {
            if *s == "Assigned issues" {
                "Issues"
            } else {
                "Pull Requests"
            }
        })
        .selected_mask(vec![true, true, false]),
    );
    m.dispatch_modal_key(key('k'));
    let out = screen(&mut m);
    assert!(out.contains("▸ [x] Pull Requests"), "{out}");
    assert!(out.contains("[-] All items"), "{out}");
    assert!(out.contains("Space clear group"), "{out}");
    insta::assert_snapshot!("mobile_setup_bulk_39x18", out);
    m.dispatch_modal_key(key(' '));
    let out = screen(&mut m);
    assert!(out.contains("▸ [ ] Pull Requests"), "{out}");
    assert!(out.contains("[ ] Authored PRs"), "{out}");
    assert!(out.contains("Space select group"), "{out}");
}

#[test]
fn all_items_control_works_through_model_on_mobile_and_desktop() {
    for (w, h, presentation) in [
        (32, 12, Presentation::Mobile),
        (120, 40, Presentation::Desktop),
    ] {
        let mut m = model(w, h, presentation);
        m.mount_modal(
            Id::Setup,
            Choice::multi("Choose tools", vec!["Claude", "Codex", "Unavailable"])
                .label(|s| s.to_string())
                .selectable(|s| *s != "Unavailable")
                .selected_mask(vec![true, false, false]),
        );
        m.dispatch_modal_key(key('g'));
        let out = screen(&mut m);
        assert!(out.contains("▸ [-] All items"), "{out}");
        assert!(out.contains("Space select all"), "{out}");
        m.dispatch_modal_key(key(' '));
        let out = screen(&mut m);
        assert!(out.contains("▸ [x] All items"), "{out}");
        assert!(out.contains("[x] Codex"), "{out}");
        assert!(out.contains("[·] Unavailable"), "{out}");
        m.dispatch_modal_key(key(' '));
        let out = screen(&mut m);
        assert!(out.contains("▸ [ ] All items"), "{out}");
        assert!(out.contains("[ ] Claude"), "{out}");
        assert!(out.contains("[ ] Codex"), "{out}");
    }
}

#[test]
fn new_session_offers_configured_repositories_before_they_have_inbox_items() {
    let mut m = model(39, 18, Presentation::Mobile);
    m.cache_persisted_setup(lazybox_core::PersistedSetup {
        enabled_providers: ["github".to_string()].into_iter().collect(),
        enabled_agents: ["codex".to_string()].into_iter().collect(),
        provider_filters: Default::default(),
        selected_scopes: [(
            "github".to_string(),
            ["github:team/phone-repo".to_string()].into_iter().collect(),
        )]
        .into_iter()
        .collect(),
    });
    m.handle_daemon_event(lazybox_ipc::Event::Snapshot {
        workspaces: vec![],
        terminals: vec![],
        projects: vec![],
        recent_snippets: vec![],
        dismissed_updates: vec![],
    });
    m.dispatch_key(key('n'));
    let out = screen(&mut m);
    assert!(
        out.contains("No repository") && out.contains("team/phone-repo"),
        "{out}"
    );
    m.dispatch_modal_key(KeyEvent::from(Key::Esc));
    assert!(m.top_modal().is_none());
}

#[test]
fn mobile_rail_and_delete_confirmation_fit_phone_screens() {
    use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
    use lazybox_ipc::{AgentState, Event, TerminalId, TerminalKind};
    for (w, h) in [(39, 18), (32, 12), (1, 1), (0, 0)] {
        let mut m = model(w, h, Presentation::Mobile);
        let workspace = Workspace::empty(
            WorkspaceKey::new("phone"),
            "Phone notes",
            chrono::Utc::now(),
        );
        let sk = SessionKey::from(&workspace.key);
        m.handle_daemon_event(Event::WorkspaceUpserted(std::sync::Arc::new(workspace)));
        for (id, state) in [
            (1, AgentState::Working),
            (2, AgentState::InputNeeded),
            (3, AgentState::Done),
        ] {
            m.handle_daemon_event(Event::TerminalSpawned {
                terminal_id: TerminalId(id),
                session_key: sk.clone(),
                kind: TerminalKind::Agent("codex".into()),
                no_permission: false,
                on_main: false,
                model_label: None,
                agent_state: Some(state),
            });
        }
        m.dispatch_key(KeyEvent::from(Key::Enter));
        let collapsed = screen(&mut m);
        m.dispatch_key(KeyEvent::new(Key::Char('t'), KeyModifiers::CONTROL));
        let expanded = screen(&mut m);
        if w > 1 {
            assert!(
                expanded.contains("● a") && expanded.contains("! b") && expanded.contains("✓ c"),
                "{expanded}"
            );
            assert!(expanded.contains("Enter/Esc close"), "{expanded}");
        }
        if w == 39 {
            insta::assert_snapshot!("mobile_rail_collapsed_39x18", collapsed);
            insta::assert_snapshot!("mobile_rail_expanded_39x18", expanded);
        }
        m.dispatch_key(key('b'));
        m.dispatch_key(KeyEvent::new(Key::Char('t'), KeyModifiers::CONTROL));
        m.dispatch_key(key('x'));
        assert_eq!(m.top_modal(), Some(&Id::MobileDeleteSession));
        let confirm = screen(&mut m);
        if w > 1 {
            assert!(
                confirm.contains("y delete") && confirm.contains("Esc cancel"),
                "{confirm}"
            );
        }
        if w == 39 {
            insta::assert_snapshot!("mobile_delete_39x18", confirm);
        }
        m.dispatch_modal_key(KeyEvent::from(Key::Esc));
    }
}

#[test]
fn mobile_global_settings_and_help_fit_and_return_to_sessions() {
    let mut m = model(32, 12, Presentation::Mobile);
    let mut setup = lazybox_core::PersistedSetup::default();
    setup.enabled_providers.insert("github".into());
    setup.enabled_agents.insert("codex".into());
    m.cache_persisted_setup(setup);
    m.dispatch_key(KeyEvent::new(Key::Char('g'), KeyModifiers::CONTROL));
    let out = screen(&mut m);
    assert!(
        out.contains("Providers") && out.contains("Add / remove repos"),
        "{out}"
    );
    insta::assert_snapshot!("mobile_settings_32x12", out);
    m.dispatch_modal_key(key('l'));
    let out = screen(&mut m);
    assert!(out.contains("Agents"), "{out}");
    m.dispatch_modal_key(key('l'));
    let out = screen(&mut m);
    assert!(
        out.contains("Appearance") && out.contains("Change theme"),
        "{out}"
    );
    m.dispatch_modal_key(key('?'));
    assert_eq!(m.top_modal(), Some(&Id::HelpAsk));
    let out = screen(&mut m);
    insta::assert_snapshot!("mobile_help_32x12", out);
    m.dispatch_modal_key(KeyEvent::from(Key::Esc));
    assert_eq!(m.top_modal(), Some(&Id::Setup));
    m.dispatch_modal_key(KeyEvent::from(Key::Esc));
    assert!(m.top_modal().is_none());
}
