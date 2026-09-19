mod common;

use lazybox_core::{FloatingWorkspaceKind, SessionId, SessionKind, Workspace, WorkspaceKey};
use lazybox_server::{ServerConfig, workspace};

fn read(config: &ServerConfig, key: &WorkspaceKey) -> Workspace {
    serde_json::from_str(
        &config
            .store
            .get_workspace(key)
            .unwrap()
            .unwrap()
            .workspace_json
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn floating_folder_and_session_survive_reuse_and_archive_preserves_user_files() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let config = ServerConfig::in_memory();
        let key = workspace::floating::create(&config, "Thinking", FloatingWorkspaceKind::Thinking)
            .unwrap();
        let path = lazybox_core::paths::sandbox_dir(key.as_str());
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        let record = read(&config, &key);
        assert!(record.local);
        assert!(record.primary_task().is_none());
        assert!(record.linked_checkout.is_none());

        let (first_path, first_id, on_main) =
            workspace::floating::resolve_session(&config, &key, None, SessionKind::Shell)
                .await
                .unwrap();
        assert_eq!(first_path, path);
        assert!(!on_main);
        let (_, resumed_id, _) =
            workspace::floating::resolve_session(&config, &key, Some(first_id), SessionKind::Shell)
                .await
                .unwrap();
        assert_eq!(first_id, resumed_id);
        assert_eq!(read(&config, &key).sessions.len(), 1);
        std::fs::write(path.join("notes.md"), "Keep this plan").unwrap();
        assert!(workspace::delete_workspace(&config, &key).await.is_some());
        assert_eq!(
            std::fs::read_to_string(path.join("notes.md")).unwrap(),
            "Keep this plan"
        );
        assert!(
            workspace::floating::resolve_session(&config, &key, None, SessionKind::Shell)
                .await
                .is_err()
        );
        let replacement =
            workspace::floating::create(&config, "Thinking", FloatingWorkspaceKind::Thinking)
                .unwrap();
        assert_ne!(
            replacement, key,
            "an archived directory must never be reused as an empty workspace"
        );
    })
    .await
    .expect("floating lifecycle completed");
}

#[tokio::test]
async fn unknown_workspace_and_stale_session_never_materialize_a_folder() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let config = ServerConfig::in_memory();
        let unknown = WorkspaceKey::new("sandbox-unknown-float");
        assert!(
            workspace::floating::resolve_session(&config, &unknown, None, SessionKind::Shell)
                .await
                .is_err()
        );
        assert!(!lazybox_core::paths::sandbox_dir(unknown.as_str()).exists());
        let key =
            workspace::floating::create(&config, "stale", FloatingWorkspaceKind::Thinking).unwrap();
        let path = lazybox_core::paths::sandbox_dir(key.as_str());
        std::fs::remove_dir(&path).unwrap();
        assert!(
            workspace::floating::resolve_session(
                &config,
                &key,
                Some(SessionId::new()),
                SessionKind::Shell
            )
            .await
            .is_err()
        );
        assert!(!path.exists());
    })
    .await
    .expect("stale floating requests completed");
}

#[test]
fn coordination_purpose_and_overrides_survive_serialization() {
    let config = ServerConfig::in_memory();
    let key = workspace::floating::create(
        &config,
        "Release planning",
        FloatingWorkspaceKind::Coordination,
    )
    .unwrap();
    let record = read(&config, &key);
    let mut cfg = lazybox_config::Config::default();
    assert_eq!(record.role, Some(lazybox_core::Role::Coordinator));
    let prompt = workspace::floating::coordination_prompt(&record, &cfg).unwrap();
    assert!(prompt.contains("minimum necessary number of issues"));
    assert!(prompt.contains("producer/consumer contracts"));
    assert!(prompt.contains("epic as the plan of record"));
    cfg.agent.coordination_prompt = Some("Our coordination procedure".into());
    assert_eq!(
        workspace::floating::coordination_prompt(&record, &cfg).as_deref(),
        Some("Our coordination procedure")
    );
    cfg.agent.coordination_prompt = Some(String::new());
    assert!(workspace::floating::coordination_prompt(&record, &cfg).is_none());
}

#[test]
fn persisted_legacy_sandbox_migrates_without_authorizing_an_unknown_key() {
    let config = ServerConfig::in_memory();
    let key = WorkspaceKey::new("sandbox-legacy-floating");
    std::fs::create_dir_all(lazybox_core::paths::sandbox_dir(key.as_str())).unwrap();
    let mut record = Workspace::empty(key.clone(), "", chrono::Utc::now());
    record.local = true;
    config
        .store
        .save_workspace(&lazybox_store::WorkspaceRecord {
            key: key.to_string(),
            created_at: record.created_at,
            workspace_json: Some(serde_json::to_string(&record).unwrap()),
        })
        .unwrap();
    workspace::migrate_legacy_sandbox(&config);
    assert_eq!(
        read(&config, &key).floating,
        Some(FloatingWorkspaceKind::Thinking)
    );
    workspace::migrate_legacy_sandbox(&config);
    assert_eq!(
        read(&config, &key).floating,
        Some(FloatingWorkspaceKind::Thinking)
    );
}
