//! E2B sandbox lifecycle driver.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    BoxHandle, BoxStatus, CommandRunner, PowerState, SandboxError, SandboxProvider, SandboxSpec,
    Tunnel,
};

const API_BASE: &str = "https://api.e2b.app";
const PROVIDER: &str = "e2b";
const SERVER_ALIVE_INTERVAL: u64 = 30;
const SERVER_ALIVE_COUNT_MAX: u64 = 3;
const PROBE_CONNECT_TIMEOUT: u64 = 8;

/// Driver for E2B sandboxes.
#[derive(Debug, Clone)]
pub struct E2bProvider {
    pub template: String,
    pub timeout_seconds: u32,
    pub user: String,
    pub remote_socket: String,
    pub local_socket: PathBuf,
    pub runner: Arc<dyn CommandRunner>,
    api_key: Option<String>,
    api_base: String,
    client: reqwest::Client,
}

impl E2bProvider {
    /// Build a provider using the `E2B_API_KEY` environment variable.
    pub fn from_env(
        template: String,
        timeout_seconds: u32,
        user: String,
        remote_socket: String,
        local_socket: PathBuf,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        let api_key = std::env::var("E2B_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty());
        Self {
            template,
            timeout_seconds,
            user,
            remote_socket,
            local_socket,
            runner,
            api_key,
            api_base: API_BASE.to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn api_key(&self) -> Result<&str, SandboxError> {
        self.api_key.as_deref().ok_or_else(|| {
            SandboxError::Config(
                "E2B credentials not configured: export E2B_API_KEY before using the e2b sandbox provider"
                    .to_string(),
            )
        })
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, SandboxError> {
        Ok(self
            .client
            .request(method, format!("{}{path}", self.api_base))
            .header("X-API-Key", self.api_key()?))
    }

    async fn json<T: DeserializeOwned>(
        &self,
        request: RequestBuilder,
        operation: &'static str,
        expected: &[StatusCode],
    ) -> Result<T, SandboxError> {
        let response = request
            .send()
            .await
            .map_err(|error| SandboxError::ApiTransport {
                provider: PROVIDER,
                operation,
                detail: error.to_string(),
            })?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| SandboxError::ApiTransport {
                provider: PROVIDER,
                operation,
                detail: error.to_string(),
            })?;
        if !expected.contains(&status) {
            return Err(SandboxError::Api {
                provider: PROVIDER,
                operation,
                status: status.as_u16(),
                detail: String::from_utf8_lossy(&bytes).trim().to_string(),
            });
        }
        serde_json::from_slice(&bytes).map_err(|error| SandboxError::Parse {
            what: "E2B API response",
            detail: error.to_string(),
        })
    }

    async fn empty(
        &self,
        request: RequestBuilder,
        operation: &'static str,
        expected: &[StatusCode],
    ) -> Result<(), SandboxError> {
        let response = request
            .send()
            .await
            .map_err(|error| SandboxError::ApiTransport {
                provider: PROVIDER,
                operation,
                detail: error.to_string(),
            })?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| SandboxError::ApiTransport {
                provider: PROVIDER,
                operation,
                detail: error.to_string(),
            })?;
        if expected.contains(&status) {
            return Ok(());
        }
        Err(SandboxError::Api {
            provider: PROVIDER,
            operation,
            status: status.as_u16(),
            detail: String::from_utf8_lossy(&bytes).trim().to_string(),
        })
    }

    async fn list_named(&self, name: &str) -> Result<Vec<ListedSandbox>, SandboxError> {
        let request = self.request(Method::GET, "/v2/sandboxes")?.query(&[
            ("state", "running"),
            ("state", "paused"),
            ("metadata", &format!("lazybox_name={name}")),
        ]);
        self.json(request, "list sandboxes", &[StatusCode::OK])
            .await
    }

    async fn detail(&self, id: &str) -> Result<SandboxDetail, SandboxError> {
        let request = self.request(Method::GET, &format!("/sandboxes/{id}"))?;
        self.json(request, "get sandbox", &[StatusCode::OK]).await
    }

    fn handle(&self, sandbox: &ListedSandbox) -> BoxHandle {
        BoxHandle {
            provider: PROVIDER.to_string(),
            id: sandbox.sandbox_id.clone(),
            region: "global".to_string(),
            zone: self.template.clone(),
            project: String::new(),
            power_state: sandbox.state.power(),
            last_active: (sandbox.state == E2bState::Running).then(Utc::now),
        }
    }

    fn ssh_destination(&self, handle: &BoxHandle) -> String {
        format!("{}@{}", self.user, handle.id)
    }

    fn ssh_options(&self) -> Vec<String> {
        vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(),
            "UserKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(),
            format!("ConnectTimeout={PROBE_CONNECT_TIMEOUT}"),
            "-o".to_string(),
            "ProxyCommand=websocat --binary -B 65536 - wss://8081-%h.e2b.app".to_string(),
        ]
    }

    fn reachable_probe_command(&self, handle: &BoxHandle) -> (String, Vec<String>) {
        let mut args = self.ssh_options();
        args.push(self.ssh_destination(handle));
        args.push("true".to_string());
        ("ssh".to_string(), args)
    }

    /// SSH command that rebuilds and restarts the daemon at `sha`.
    pub fn rebuild_command(&self, handle: &BoxHandle, sha: &str) -> (String, Vec<String>) {
        let mut args = self.ssh_options();
        args.push(self.ssh_destination(handle));
        let target = shell_quote(sha);
        let current_check = if sha.is_empty() {
            String::new()
        } else {
            format!("test \"$(cat /etc/lazybox/build-sha 2>/dev/null)\" = {target} || ")
        };
        args.push(format!(
            "{current_check}sudo env LAZYBOX_SERVICE_MODE=direct LAZYBOX_USER=user \
             LAZYBOX_SRC_DIR=/opt/lazybox/git \
             /usr/local/bin/lazybox-build.sh {target}"
        ));
        ("ssh".to_string(), args)
    }

    async fn stamp_daemon(&self, handle: &BoxHandle, sha: &str) -> Result<(), SandboxError> {
        let (program, args) = self.rebuild_command(handle, sha);
        self.runner.run(&program, &args, &[]).await.map(|_| ())
    }

    fn tunnel(&self, handle: &BoxHandle, ports: &[u16]) -> Tunnel {
        let mut args = self.ssh_options();
        args.extend([
            "-N".to_string(),
            "-T".to_string(),
            "-o".to_string(),
            "ExitOnForwardFailure=yes".to_string(),
            "-o".to_string(),
            format!("ServerAliveInterval={SERVER_ALIVE_INTERVAL}"),
            "-o".to_string(),
            format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}"),
            "-L".to_string(),
            format!("{}:{}", self.local_socket.display(), self.remote_socket),
        ]);
        for port in ports {
            args.push("-L".to_string());
            args.push(format!("127.0.0.1:{port}:127.0.0.1:{port}"));
        }
        args.push(self.ssh_destination(handle));
        Tunnel {
            program: "ssh".to_string(),
            args,
            env: Vec::new(),
            local_socket: self.local_socket.clone(),
            ports: ports.to_vec(),
        }
    }
}

impl SandboxProvider for E2bProvider {
    fn id(&self) -> &str {
        PROVIDER
    }

    async fn check_auth(&self) -> Result<(), SandboxError> {
        let request = self
            .request(Method::GET, "/v2/sandboxes")?
            .query(&[("limit", "1")]);
        let _: Vec<ListedSandbox> = self
            .json(request, "authenticate", &[StatusCode::OK])
            .await?;
        Ok(())
    }

    async fn ensure(&self, spec: &SandboxSpec) -> Result<BoxHandle, SandboxError> {
        if let Some(existing) = self.list_named(&spec.name).await?.first() {
            let mut handle = self.handle(existing);
            if !handle.power_state.is_running() {
                self.start(&handle).await?;
                handle.observe(PowerState::Running, Utc::now());
            }
            if spec.install_lazybox {
                self.stamp_daemon(&handle, &spec.lazybox_git_sha).await?;
            }
            return Ok(handle);
        }

        let body = NewSandbox {
            template_id: &self.template,
            timeout: self.timeout_seconds,
            auto_pause: true,
            auto_pause_memory: true,
            metadata: HashMap::from([
                ("lazybox_name", spec.name.as_str()),
                ("lazybox_sha", spec.lazybox_git_sha.as_str()),
            ]),
        };
        let request = self.request(Method::POST, "/sandboxes")?.json(&body);
        let created: CreatedSandbox = self
            .json(request, "create sandbox", &[StatusCode::CREATED])
            .await?;
        let listed = ListedSandbox {
            sandbox_id: created.sandbox_id,
            state: E2bState::Running,
        };
        let handle = self.handle(&listed);
        if spec.install_lazybox {
            self.stamp_daemon(&handle, &spec.lazybox_git_sha).await?;
        }
        Ok(handle)
    }

    async fn start(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let request = self
            .request(Method::POST, &format!("/sandboxes/{}/connect", handle.id))?
            .json(&ConnectSandbox {
                timeout: self.timeout_seconds,
            });
        let _: CreatedSandbox = self
            .json(
                request,
                "resume sandbox",
                &[StatusCode::OK, StatusCode::CREATED],
            )
            .await?;
        Ok(())
    }

    async fn stop(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let request = self
            .request(Method::POST, &format!("/sandboxes/{}/pause", handle.id))?
            .json(&PauseSandbox { memory: true });
        self.empty(request, "pause sandbox", &[StatusCode::NO_CONTENT])
            .await
    }

    async fn status(&self, handle: &BoxHandle) -> Result<BoxStatus, SandboxError> {
        let power = self.detail(&handle.id).await?.state.power();
        let reachable = if power.is_running() {
            let (program, args) = self.reachable_probe_command(handle);
            self.runner.run(&program, &args, &[]).await.is_ok()
        } else {
            false
        };
        Ok(BoxStatus { power, reachable })
    }

    async fn connect(&self, handle: &BoxHandle, ports: &[u16]) -> Result<Tunnel, SandboxError> {
        if !self.detail(&handle.id).await?.state.power().is_running() {
            self.start(handle).await?;
        }
        Ok(self.tunnel(handle, ports))
    }

    async fn destroy(&self, handle: &BoxHandle) -> Result<(), SandboxError> {
        let request = self.request(Method::DELETE, &format!("/sandboxes/{}", handle.id))?;
        self.empty(request, "destroy sandbox", &[StatusCode::NO_CONTENT])
            .await
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum E2bState {
    Running,
    Paused,
}

impl E2bState {
    fn power(self) -> PowerState {
        match self {
            Self::Running => PowerState::Running,
            Self::Paused => PowerState::Stopped,
        }
    }
}

#[derive(Deserialize)]
struct ListedSandbox {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
    state: E2bState,
}

#[derive(Deserialize)]
struct SandboxDetail {
    state: E2bState,
}

#[derive(Deserialize)]
struct CreatedSandbox {
    #[serde(rename = "sandboxID")]
    sandbox_id: String,
}

#[derive(Serialize)]
struct NewSandbox<'a> {
    #[serde(rename = "templateID")]
    template_id: &'a str,
    timeout: u32,
    #[serde(rename = "autoPause")]
    auto_pause: bool,
    #[serde(rename = "autoPauseMemory")]
    auto_pause_memory: bool,
    metadata: HashMap<&'static str, &'a str>,
}

#[derive(Serialize)]
struct ConnectSandbox {
    timeout: u32,
}

#[derive(Serialize)]
struct PauseSandbox {
    memory: bool,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandFuture;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug)]
    struct OkRunner;

    impl CommandRunner for OkRunner {
        fn run<'a>(
            &'a self,
            _program: &'a str,
            _args: &'a [String],
            _env: &'a [(String, String)],
        ) -> CommandFuture<'a, String> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[derive(Debug, Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl CommandRunner for RecordingRunner {
        fn run<'a>(
            &'a self,
            program: &'a str,
            args: &'a [String],
            _env: &'a [(String, String)],
        ) -> CommandFuture<'a, String> {
            let program = program.to_string();
            let args = args.to_vec();
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("recording runner lock")
                    .push((program, args));
                Ok(String::new())
            })
        }
    }

    struct MockResponse {
        status: &'static str,
        body: &'static str,
    }

    async fn mock_api(
        responses: Vec<MockResponse>,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "client closed before request headers");
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let mut chunk = [0u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "client closed before request body");
                    request.extend_from_slice(&chunk[..read]);
                }
                captured
                    .lock()
                    .expect("request capture lock")
                    .push(String::from_utf8_lossy(&request).to_string());
                let reply = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), requests, task)
    }

    fn provider_at(api_base: String, runner: Arc<dyn CommandRunner>) -> E2bProvider {
        E2bProvider {
            api_base,
            runner,
            ..provider()
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            provider: PROVIDER.to_string(),
            name: "lazybox-sbx-test".to_string(),
            project: String::new(),
            region: "global".to_string(),
            zone: String::new(),
            deployment: crate::Deployment::default_recipe().unwrap(),
            install_lazybox: true,
            lazybox_git_sha: "abc123".to_string(),
        }
    }

    fn provider() -> E2bProvider {
        E2bProvider {
            template: "lazybox-e2b".to_string(),
            timeout_seconds: 3600,
            user: "lazybox".to_string(),
            remote_socket: ".lazybox/run/daemon.sock".to_string(),
            local_socket: PathBuf::from("/tmp/lazybox.sock"),
            runner: Arc::new(OkRunner),
            api_key: Some("test-key".to_string()),
            api_base: "http://127.0.0.1".to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn handle(state: PowerState) -> BoxHandle {
        BoxHandle {
            provider: PROVIDER.to_string(),
            id: "sbx_123".to_string(),
            region: "global".to_string(),
            zone: "lazybox-e2b".to_string(),
            project: String::new(),
            power_state: state,
            last_active: None,
        }
    }

    #[test]
    fn missing_api_key_is_actionable() {
        let mut provider = provider();
        provider.api_key = None;
        let error = provider.api_key().unwrap_err().to_string();
        assert!(error.contains("E2B_API_KEY"), "{error}");
    }

    #[test]
    fn full_memory_pause_body_is_explicit() {
        assert_eq!(
            serde_json::to_value(PauseSandbox { memory: true }).unwrap(),
            serde_json::json!({ "memory": true })
        );
    }

    #[test]
    fn create_body_enables_memory_preserving_auto_pause() {
        let body = NewSandbox {
            template_id: "lazybox-e2b",
            timeout: 3600,
            auto_pause: true,
            auto_pause_memory: true,
            metadata: HashMap::from([("lazybox_name", "box"), ("lazybox_sha", "abc")]),
        };
        assert_eq!(
            serde_json::to_value(body).unwrap(),
            serde_json::json!({
                "templateID": "lazybox-e2b",
                "timeout": 3600,
                "autoPause": true,
                "autoPauseMemory": true,
                "metadata": { "lazybox_name": "box", "lazybox_sha": "abc" }
            })
        );
    }

    #[test]
    fn tunnel_forwards_socket_and_ports_over_e2b_websocket() {
        let tunnel = provider().tunnel(&handle(PowerState::Running), &[3000, 8082]);
        assert_eq!(tunnel.program, "ssh");
        assert!(
            tunnel
                .args
                .iter()
                .any(|arg| arg.contains("wss://8081-%h.e2b.app"))
        );
        assert!(
            tunnel
                .args
                .contains(&"/tmp/lazybox.sock:.lazybox/run/daemon.sock".to_string())
        );
        assert!(
            tunnel
                .args
                .contains(&"127.0.0.1:3000:127.0.0.1:3000".to_string())
        );
        assert_eq!(
            tunnel.args.last().map(String::as_str),
            Some("lazybox@sbx_123")
        );
    }

    #[test]
    fn rebuild_uses_direct_service_mode_and_quotes_sha() {
        let (_, args) = provider().rebuild_command(&handle(PowerState::Running), "abc'123");
        let remote = args.last().expect("remote command");
        assert!(remote.contains("LAZYBOX_SERVICE_MODE=direct"), "{remote}");
        assert!(remote.contains("'abc'\"'\"'123'"), "{remote}");
    }

    #[test]
    fn e2b_states_map_to_provider_states() {
        assert_eq!(E2bState::Running.power(), PowerState::Running);
        assert_eq!(E2bState::Paused.power(), PowerState::Stopped);
    }

    #[tokio::test]
    async fn ensure_creates_a_memory_preserving_sandbox_and_stamps_the_daemon() {
        let (base, requests, server) = mock_api(vec![
            MockResponse {
                status: "200 OK",
                body: "[]",
            },
            MockResponse {
                status: "201 Created",
                body: r#"{"sandboxID":"sbx_created"}"#,
            },
        ])
        .await;
        let runner = Arc::new(RecordingRunner::default());
        let provider = provider_at(base, runner.clone());

        let created = provider.ensure(&spec()).await.unwrap();
        server.await.unwrap();

        assert_eq!(created.id, "sbx_created");
        assert_eq!(created.power_state, PowerState::Running);
        let requests = requests.lock().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /v2/sandboxes?state=running&state=paused&metadata=lazybox_name%3Dlazybox-sbx-test"
            ),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("x-api-key: test-key\r\n"));
        assert!(requests[1].starts_with("POST /sandboxes HTTP/1.1"));
        let create_body = requests[1].split("\r\n\r\n").nth(1).unwrap();
        let create: serde_json::Value = serde_json::from_str(create_body).unwrap();
        assert_eq!(create["autoPause"], true);
        assert_eq!(create["autoPauseMemory"], true);
        assert_eq!(create["metadata"]["lazybox_sha"], "abc123");
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ssh");
        assert!(calls[0].1.last().unwrap().contains("abc123"));
    }

    #[tokio::test]
    async fn lifecycle_uses_resume_pause_status_connect_and_destroy_endpoints() {
        let (base, requests, server) = mock_api(vec![
            MockResponse {
                status: "201 Created",
                body: r#"{"sandboxID":"sbx_123"}"#,
            },
            MockResponse {
                status: "204 No Content",
                body: "",
            },
            MockResponse {
                status: "200 OK",
                body: r#"{"state":"running"}"#,
            },
            MockResponse {
                status: "200 OK",
                body: r#"{"state":"paused"}"#,
            },
            MockResponse {
                status: "201 Created",
                body: r#"{"sandboxID":"sbx_123"}"#,
            },
            MockResponse {
                status: "204 No Content",
                body: "",
            },
        ])
        .await;
        let provider = provider_at(base, Arc::new(OkRunner));
        let handle = handle(PowerState::Stopped);

        provider.start(&handle).await.unwrap();
        provider.stop(&handle).await.unwrap();
        let status = provider.status(&handle).await.unwrap();
        let tunnel = provider.connect(&handle, &[3000]).await.unwrap();
        provider.destroy(&handle).await.unwrap();
        server.await.unwrap();

        assert_eq!(
            status,
            BoxStatus {
                power: PowerState::Running,
                reachable: true
            }
        );
        assert_eq!(tunnel.ports, vec![3000]);
        let requests = requests.lock().unwrap();
        assert!(requests[0].starts_with("POST /sandboxes/sbx_123/connect HTTP/1.1"));
        assert!(requests[1].starts_with("POST /sandboxes/sbx_123/pause HTTP/1.1"));
        assert_eq!(
            requests[1].split("\r\n\r\n").nth(1),
            Some(r#"{"memory":true}"#)
        );
        assert!(requests[2].starts_with("GET /sandboxes/sbx_123 HTTP/1.1"));
        assert!(requests[3].starts_with("GET /sandboxes/sbx_123 HTTP/1.1"));
        assert!(requests[4].starts_with("POST /sandboxes/sbx_123/connect HTTP/1.1"));
        assert!(requests[5].starts_with("DELETE /sandboxes/sbx_123 HTTP/1.1"));
    }

    #[tokio::test]
    async fn ensure_resumes_an_existing_paused_sandbox_instead_of_creating_one() {
        let (base, requests, server) = mock_api(vec![
            MockResponse {
                status: "200 OK",
                body: r#"[{"sandboxID":"sbx_existing","state":"paused"}]"#,
            },
            MockResponse {
                status: "201 Created",
                body: r#"{"sandboxID":"sbx_existing"}"#,
            },
        ])
        .await;
        let runner = Arc::new(RecordingRunner::default());
        let provider = provider_at(base, runner);

        let existing = provider.ensure(&spec()).await.unwrap();
        server.await.unwrap();

        assert_eq!(existing.id, "sbx_existing");
        assert_eq!(existing.power_state, PowerState::Running);
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "no create request for an existing sandbox"
        );
        assert!(requests[1].starts_with("POST /sandboxes/sbx_existing/connect HTTP/1.1"));
    }
}
