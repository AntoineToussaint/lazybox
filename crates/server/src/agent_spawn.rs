//! Spawn an agent PTY with an attached LLM proxy.
//!
//! ## Why a proxy per agent spawn
//!
//! Each agent spawn gets its own dedicated proxy instance. That makes
//! attribution unambiguous: every request that hits that proxy's
//! listen port belongs to that session — no header tricks, no shared
//! routing tables. The cost is one tokio task + one TCP listener per
//! running agent. Cheap; bounded by the number of agents the user is
//! currently running.
//!
//! ## Env injection
//!
//! `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` point the agent at our
//! proxy. `X-Lazybox-Session` is added via `ANTHROPIC_DEFAULT_HEADERS`
//! where the agent supports it; otherwise we fall back on the
//! per-spawn proxy binding as the attribution mechanism (the proxy
//! stamps records with the `session_tag` we hand it at startup, not
//! from the request header).

use crate::pty::{DaemonPty, PtyError};
use lazybox_core::SessionKey;
use lazybox_llm_proxy::{ProxyConfig, ProxyError, ProxyRecord, ProxyServer};
use portable_pty::PtySize;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Which LLM vendor an agent's proxy should target. Determines the
/// env var name we set on the spawned command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyProvider {
    Anthropic,
    OpenAI,
}

impl ProxyProvider {
    pub fn env_var(&self) -> &'static str {
        match self {
            ProxyProvider::Anthropic => "ANTHROPIC_BASE_URL",
            ProxyProvider::OpenAI => "OPENAI_BASE_URL",
        }
    }
}

/// Configuration for one agent spawn.
#[derive(Debug)]
pub struct AgentSpawnConfig {
    pub session_key: SessionKey,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub size: PtySize,
    /// Additional env vars beyond what we inject. Usually empty; the
    /// caller can override if needed (PATH tweaks, etc.).
    pub extra_env: HashMap<String, String>,
    /// Proxy target. When `None`, the agent runs without a proxy
    /// (shell spawns, log tails). When `Some`, we start a proxy and
    /// inject the corresponding env var.
    pub proxy: Option<ProxyTarget>,
}

/// Upstream + provider for the proxy. `upstream` is the real URL
/// (`https://api.anthropic.com`, `https://api.openai.com`).
#[derive(Debug, Clone)]
pub struct ProxyTarget {
    pub provider: ProxyProvider,
    pub upstream: String,
}

/// Handle returned by `spawn_with_proxy`. The caller owns all three
/// pieces: the PTY (for keystrokes in, output out), the proxy (for
/// lifecycle control), and the records receiver (for streaming
/// `ProxyRecord`s out to clients via `Event::ProxyRecord`).
pub struct AgentSpawn {
    pub pty: DaemonPty,
    /// `None` for spawns without a proxy (shells, log tails).
    pub proxy: Option<ProxyServer>,
    /// `None` for spawns without a proxy.
    pub records: Option<mpsc::Receiver<ProxyRecord>>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentSpawnError {
    #[error("pty: {0}")]
    Pty(#[from] PtyError),
    #[error("proxy: {0}")]
    Proxy(#[from] ProxyError),
}

/// Spawn an agent. If `config.proxy` is `Some`, first start a proxy
/// pointed at the requested upstream, then inject the proxy's listen
/// URL into the spawn's env as `ANTHROPIC_BASE_URL` /
/// `OPENAI_BASE_URL`.
///
/// The caller is responsible for:
///   - Polling `records` and forwarding each record as
///     `Event::ProxyRecord` through the IPC server channel.
///   - Shutting down the returned `ProxyServer` when the PTY exits
///     (otherwise the proxy keeps its port bound forever).
///
/// Inject the proxy base-URL env var when a proxy was attached.
/// Pure helper — pulled out of `spawn_with_proxy` so tests can verify
/// env injection without starting a real PTY or proxy.
///
/// `extra_env` is mutated in place; the function returns it for
/// fluency.
pub fn inject_proxy_env(
    extra_env: &mut HashMap<String, String>,
    proxy: Option<&ProxyTarget>,
    proxy_listen_url: Option<&str>,
) {
    if let (Some(target), Some(url)) = (proxy, proxy_listen_url) {
        extra_env.insert(target.provider.env_var().to_string(), url.to_string());
    }
}

pub async fn spawn_with_proxy(mut config: AgentSpawnConfig) -> Result<AgentSpawn, AgentSpawnError> {
    let (proxy, records) = match &config.proxy {
        Some(target) => {
            let proxy_config = ProxyConfig {
                listen: "127.0.0.1:0".parse().expect("static addr"),
                ..ProxyConfig::default()
            };
            let (server, rx) = ProxyServer::start(proxy_config, target.upstream.clone()).await?;
            let url = format!("http://{}", server.addr());
            tracing::info!(
                "proxy started for {} on {url} → {}",
                config.session_key.as_str(),
                target.upstream
            );
            inject_proxy_env(&mut config.extra_env, Some(target), Some(&url));
            (Some(server), Some(rx))
        }
        None => (None, None),
    };

    let env: Vec<(String, String)> = config.extra_env.into_iter().collect();
    let pty = DaemonPty::spawn(&config.argv, config.size, config.cwd.as_ref(), env)?;

    Ok(AgentSpawn {
        pty,
        proxy,
        records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_name_matches_provider() {
        assert_eq!(ProxyProvider::Anthropic.env_var(), "ANTHROPIC_BASE_URL");
        assert_eq!(ProxyProvider::OpenAI.env_var(), "OPENAI_BASE_URL");
    }
}
