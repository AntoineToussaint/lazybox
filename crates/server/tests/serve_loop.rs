//! Smoke tests for the daemon's serve loop. We drive it over
//! `ipc::channel::pair` — zero serialization, zero sockets — so tests
//! are fast and deterministic.

use lazybox_ipc::{
    AgentInputMessage, AgentRunId, AgentRuntimeMode, Command, Event, PrincipalId,
    ProviderCredentialInput, channel,
};
use lazybox_server::{Server, ServerConfig};

#[tokio::test]
async fn subscribe_yields_snapshot() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Subscribe).unwrap();
    let evt = client.recv().await.expect("daemon responds");
    match evt {
        Event::Snapshot {
            workspaces,
            terminals,
            ..
        } => {
            // Contract under test: Subscribe ALWAYS replies with a
            // Snapshot before any live events. With no workspaces
            // persisted and no terminals spawned, both lists are
            // empty.
            assert!(workspaces.is_empty());
            assert!(terminals.is_empty());
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}
#[tokio::test]
async fn shutdown_closes_loop_cleanly() {
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client.send(Command::Shutdown).unwrap();
    // Drop client to unblock the channel close path.
    drop(client);
    // If Shutdown isn't honored the task would hang here forever; the
    // test timeout is our backstop but a clean exit is the contract.
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("daemon exits promptly on Shutdown")
        .unwrap();
}
#[tokio::test]
async fn start_agent_run_unknown_agent_reports_error() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client
        .send(Command::StartAgentRun {
            session_key: "test:ws".into(),
            session_id: None,
            agent: "does-not-exist".into(),
            mode: AgentRuntimeMode::StreamJson,
            cwd: None,
            initial_input: None,
        })
        .unwrap();

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderError { message, .. } => {
            assert!(message.contains("no agent registered"));
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}
#[tokio::test]
async fn send_agent_input_unknown_run_reports_error() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });

    client
        .send(Command::SendAgentInput {
            run_id: AgentRunId(99),
            message: AgentInputMessage {
                text: Some("hello".into()),
                json: None,
            },
        })
        .unwrap();

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderError { message, .. } => {
            assert!(message.contains("unknown agent run"));
        }
        other => panic!("expected ProviderError, got {other:?}"),
    }
}
#[tokio::test]
async fn provider_credential_commands_return_metadata_without_secrets() {
    let (mut client, server) = channel::pair();
    tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });
    let principal_id = PrincipalId::new("alice");

    client
        .send(Command::UpsertProviderCredential {
            principal_id: principal_id.clone(),
            credential: ProviderCredentialInput {
                provider_id: "github".into(),
                token: "ghp_do_not_log".into(),
                source: "unit-test".into(),
                scopes: vec!["repo".into()],
                expires_at: None,
            },
        })
        .unwrap();

    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    assert!(!format!("{evt:?}").contains("ghp_do_not_log"));
    match evt {
        Event::ProviderCredentialUpdated {
            principal_id: event_principal,
            provider_id,
            metadata,
        } => {
            assert_eq!(event_principal, principal_id);
            assert_eq!(provider_id, "github");
            assert_eq!(metadata.principal_id, principal_id);
            assert_eq!(metadata.provider_id, "github");
            assert_eq!(metadata.source, "unit-test");
            assert_eq!(metadata.scopes, vec!["repo"]);
        }
        other => panic!("expected ProviderCredentialUpdated, got {other:?}"),
    }

    client
        .send(Command::ListProviderCredentials {
            principal_id: principal_id.clone(),
        })
        .unwrap();
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderCredentialsListed {
            principal_id: event_principal,
            credentials,
        } => {
            assert_eq!(event_principal, principal_id);
            assert_eq!(credentials.len(), 1);
            assert_eq!(credentials[0].provider_id, "github");
        }
        other => panic!("expected ProviderCredentialsListed, got {other:?}"),
    }

    client
        .send(Command::RemoveProviderCredential {
            principal_id: principal_id.clone(),
            provider_id: "github".into(),
        })
        .unwrap();
    let evt = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("daemon responds")
        .expect("got event");
    match evt {
        Event::ProviderCredentialRemoved {
            principal_id: event_principal,
            provider_id,
        } => {
            assert_eq!(event_principal, principal_id);
            assert_eq!(provider_id, "github");
        }
        other => panic!("expected ProviderCredentialRemoved, got {other:?}"),
    }
}
#[tokio::test]
async fn client_drop_terminates_daemon_loop() {
    // The daemon is a long-running service but a single-client loop
    // should exit when its client drops; multi-client handling comes
    // later with a multiplexer.
    let (client, server) = channel::pair();
    let handle = tokio::spawn(async move {
        Server::new(ServerConfig::in_memory())
            .serve(server)
            .await
            .unwrap();
    });
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("daemon exits when client drops")
        .unwrap();
}
