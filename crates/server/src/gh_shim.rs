//! Routing a session's `gh` through the daemon (#1801).
//!
//! #1799 gave a spawned session the tracker record the daemon had already
//! paid for. That helps an agent that *reads* it. The calls it doesn't
//! cover — `gh issue view`, `gh api`, `gh search`, and every mutation — still
//! went straight at GitHub: no dedupe between sessions, no per-session pacing,
//! and no way for the daemon to learn that a record had changed. Twenty issues
//! closed with `gh` inside workspaces were still showing open forty minutes
//! later, because the only path from "closed" to "the row says closed" was a
//! sweep, and the fleet had spent the budget the sweep needed.
//!
//! This module is the daemon half of the fix. A shim on the session's PATH
//! asks before it spends and reports after, so:
//!
//! - an identical read inside [`lazybox_config::GhShimConfig::read_cache_ttl`]
//!   is answered from the daemon's read cache and costs nothing upstream;
//! - each session draws from its own token bucket, and reads are refused
//!   outright while the governor's reserve is breached, so a fan-out paces
//!   itself instead of racing the poller for the last of the budget;
//! - a mutation's effect is written onto the cached row immediately, which is
//!   the only part of this that works with the budget at zero.
//!
//! What the daemon deliberately does **not** do is re-render `gh`'s output
//! from its own [`lazybox_core::Task`]. The cache stores the bytes `gh`
//! printed, keyed by the invocation, because an agent parses that output and
//! a lookalike rendering would diverge from it silently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lazybox_core::{SessionKey, TaskState};
use lazybox_ipc::gh_shim::{GhCallKind, GhChangeKind, GhRecordChange, GhVerdict};

use crate::ServerConfig;

/// Bucket key for `gh` run outside any lazybox session — a plain shell, or a
/// helper the user invoked by hand. They share one bucket rather than going
/// unmetered: the budget they spend is the same budget.
const UNSESSIONED: &str = "-";

/// Ceiling on the wait handed back in [`GhVerdict::Throttle`]. The shim sleeps
/// on it, so an honest "come back in 43 minutes" (a breached reserve waiting on
/// the hourly window) must not become a 43-minute hang inside the agent's shell
/// — the shim gives up well before this and tells the agent why, with the
/// escape hatch named.
const MAX_THROTTLE_WAIT: u64 = 300;

/// Ceiling on [`lazybox_config::GhShimConfig::read_cache_ttl`].
///
/// The config value is user-supplied and the cache evicts by age, so an
/// unclamped TTL is an unbounded retention window: `read_cache_ttl: 24h`
/// keeps every distinct read of a whole fleet-day resident in the daemon.
/// Ten minutes is already far past the point where serving a cached answer is
/// defensible for a record that changes.
const MAX_READ_CACHE_TTL: Duration = Duration::from_secs(600);

/// Hard caps on the read cache, independent of the TTL.
///
/// Age alone bounds nothing when the arrival rate is the variable: the fleet's
/// read rate scales with how many agents are running, and the per-entry cap
/// is 128 KiB. These two make the cache's worst case a number that can be
/// stated (~32 MB) rather than one that depends on how busy the box is.
const MAX_CACHE_ENTRIES: usize = 512;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// One session's token bucket over `gh` invocations.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// One read's answer, kept just long enough for the rest of the fleet to ask
/// the same question.
#[derive(Debug)]
struct CachedRead {
    stdout: String,
    at: Instant,
}

/// Per-session quota and the cross-session read cache. In memory only: a
/// restarted daemon has no business vouching for output it cannot re-derive,
/// and a fresh set of full buckets is the safe direction to be wrong in.
#[derive(Debug, Default)]
pub struct GhShimState {
    buckets: HashMap<String, Bucket>,
    cache: HashMap<String, CachedRead>,
}

impl GhShimState {
    /// Refill `key`'s bucket to the present and take one token, or report how
    /// long until one is available.
    fn take_token(
        &mut self,
        key: &str,
        burst: u32,
        refill_per_min: f64,
        now: Instant,
    ) -> Result<(), u64> {
        let capacity = f64::from(burst.max(1));
        let per_sec = (refill_per_min / 60.0).max(f64::MIN_POSITIVE);
        let bucket = self.buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_sec).min(capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        Err(((1.0 - bucket.tokens) / per_sec).ceil().max(1.0) as u64)
    }

    /// `key`'s cached stdout, if it was stored within `ttl`. Expired entries
    /// are dropped on the way past, which is all the eviction this needs: an
    /// entry is only ever re-read by a key that is asked again.
    fn cached(&mut self, key: &str, ttl: Duration, now: Instant) -> Option<String> {
        let entry = self.cache.get(key)?;
        if now.saturating_duration_since(entry.at) <= ttl {
            return Some(entry.stdout.clone());
        }
        self.cache.remove(key);
        None
    }

    fn store(&mut self, key: String, stdout: String, ttl: Duration, now: Instant) {
        self.cache
            .retain(|_, entry| now.saturating_duration_since(entry.at) <= ttl);
        self.cache
            .insert(key.clone(), CachedRead { stdout, at: now });
        self.evict_to_capacity(&key);
    }

    /// Drop the oldest entries until the cache is inside both caps. Oldest
    /// first because the newest answer is the one the fleet is most likely to
    /// ask for again, and the oldest is closest to expiring anyway.
    ///
    /// `keep` is the entry the caller just stored, and it is never the one
    /// evicted. Without that, two stores landing on the same `Instant` tie on
    /// the eviction key and `min_by_key` may pick either — so a store could
    /// discard the very answer it had just paid a GitHub call for, at random.
    fn evict_to_capacity(&mut self, keep: &str) {
        let mut bytes: usize = self.cache.values().map(|entry| entry.stdout.len()).sum();
        while self.cache.len() > MAX_CACHE_ENTRIES || bytes > MAX_CACHE_BYTES {
            let Some(oldest) = self
                .cache
                .iter()
                .filter(|(key, _)| key.as_str() != keep)
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            if let Some(evicted) = self.cache.remove(&oldest) {
                bytes = bytes.saturating_sub(evicted.stdout.len());
            }
        }
    }

    /// Drop every cached read scoped to `repo` — or every read at all when the
    /// mutation's repo could not be named.
    ///
    /// Invalidating the whole repo rather than the one record is deliberate: a
    /// closed issue also falsifies the `gh issue list` that included it, and
    /// that list does not mention the number. Over-invalidating costs a re-read
    /// inside one TTL; under-invalidating makes the dedupe itself the reason
    /// the fleet sees stale state, which is the bug this exists to fix.
    fn invalidate_repo(&mut self, repo: Option<&str>) {
        let Some(repo) = repo.map(str::to_lowercase) else {
            self.cache.clear();
            return;
        };
        self.cache
            .retain(|key, _| !key.to_lowercase().starts_with(&repo));
    }
}

/// Whether the shared GitHub budget is into the reserve that scheduled polling
/// depends on, and when it recovers.
///
/// The governor lets *interactive* work spend past the reserve on purpose — a
/// user's keypress must not be stranded with budget left. Agent `gh` is not
/// that: it is the background burn that emptied the budget in the first place,
/// so it yields here instead. Mutations are exempt (see [`GhCallKind`]).
fn reserve_breached(config: &ServerConfig) -> Option<(String, u64)> {
    let client = config.poll.cached_gh_client()?;
    let now = chrono::Utc::now();
    client
        .rate_snapshot()
        .resources
        .into_iter()
        .filter(|resource| resource.remaining <= resource.reserve && resource.reset_at > now)
        .map(|resource| {
            let wait = resource
                .reset_at
                .signed_duration_since(now)
                .to_std()
                .unwrap_or_default()
                .as_secs()
                .max(1);
            (
                format!(
                    "GitHub {} budget is at {}/{}, inside the {} lazybox's poller reserves",
                    resource.resource, resource.remaining, resource.limit, resource.reserve,
                ),
                wait,
            )
        })
        .max_by_key(|(_, wait)| *wait)
}

/// Decide one `gh` invocation: serve it from the read cache, admit it, or hold
/// it back.
pub fn admit(
    config: &ServerConfig,
    session_key: Option<&SessionKey>,
    kind: GhCallKind,
    read_key: Option<&str>,
) -> GhVerdict {
    // `Config::load` is mtime-cached, so re-reading it per invocation costs a
    // stat and picks up an edit without a daemon restart.
    let cfg = lazybox_config::Config::load()
        .unwrap_or_default()
        .providers
        .github
        .gh_shim;
    // A shim left on PATH by a previous spawn must not start refusing work
    // because the feature was switched off under it.
    if !cfg.enabled {
        return GhVerdict::Allow;
    }
    let now = Instant::now();
    let mut state = config.poll.gh_shim.lock();

    // A cache hit spends no token: it costs neither GitHub nor the poller, so
    // charging for it would throttle the very behaviour this exists to reward.
    if let Some(key) = read_key
        && let Some(stdout) = state.cached(key, cfg.read_cache_ttl.min(MAX_READ_CACHE_TTL), now)
    {
        return GhVerdict::Cached { stdout };
    }

    drop(state);

    // Checked before the token is taken: a call held back at the reserve has
    // not happened, and charging it quota would make the session pay twice for
    // one deferred read.
    if kind != GhCallKind::Mutation
        && let Some((reason, wait)) = reserve_breached(config)
    {
        return GhVerdict::Throttle {
            wait_secs: wait.min(MAX_THROTTLE_WAIT),
            reason,
        };
    }

    let bucket_key = session_key.map_or(UNSESSIONED, SessionKey::as_str);
    if let Err(wait) = config.poll.gh_shim.lock().take_token(
        bucket_key,
        cfg.session_burst,
        cfg.session_refill_per_min,
        now,
    ) {
        return GhVerdict::Throttle {
            wait_secs: wait.min(MAX_THROTTLE_WAIT),
            reason: format!(
                "this session has spent its gh quota ({} calls burst, {}/min sustained)",
                cfg.session_burst, cfg.session_refill_per_min,
            ),
        };
    }
    GhVerdict::Allow
}

/// Record what an admitted invocation did: fill the read cache, and apply a
/// mutation's effect to the cached row.
pub async fn completed(
    config: &ServerConfig,
    read_key: Option<String>,
    stdout: Option<String>,
    change: Option<GhRecordChange>,
) {
    if let (Some(key), Some(stdout)) = (read_key, stdout) {
        let ttl = lazybox_config::Config::load()
            .unwrap_or_default()
            .providers
            .github
            .gh_shim
            .read_cache_ttl
            .min(MAX_READ_CACHE_TTL);
        config
            .poll
            .gh_shim
            .lock()
            .store(key, stdout, ttl, Instant::now());
    }
    if let Some(change) = change {
        apply_change(config, &change).await;
    }
}

/// Write a mutation's effect onto the daemon's cached row.
///
/// This is the half that works with the budget at zero: the session already
/// knows the record is closed, so nothing here reads GitHub. The poller is
/// still woken as the backstop that reconciles everything a local state flip
/// cannot know — a comment's text, a new record's body, an edit.
async fn apply_change(config: &ServerConfig, change: &GhRecordChange) {
    let Some(id) =
        lazybox_core::task_ref::parse_task_ref(&change.reference, change.repo.as_deref())
    else {
        tracing::debug!(
            reference = %change.reference,
            "gh shim: change signal names a record that does not parse",
        );
        return;
    };
    config
        .poll
        .gh_shim
        .lock()
        .invalidate_repo(lazybox_core::task_ref::github_repo_of(&id));

    let state = match change.kind {
        GhChangeKind::Closed => Some(TaskState::Closed),
        GhChangeKind::Reopened => Some(TaskState::Open),
        GhChangeKind::Merged => Some(TaskState::Merged),
        // Nothing local to synthesize: a comment or an edit changes content
        // only GitHub holds. Reported as the poller's job rather than guessed.
        GhChangeKind::Touched => None,
    };
    let Some(state) = state else {
        config.poll.wake(true);
        return;
    };

    let store = config.store.clone();
    let wanted = id.clone();
    // Prefiltered on the raw JSON before decoding, per the rule
    // `task_cache::workspaces_matching` documents: decoding every row parses
    // each workspace's whole activity feed, and a session closing issues in a
    // loop lands here once per close. The task's id key appears verbatim in
    // any row holding it, so the filter is a conservative superset and the
    // exact `hierarchy_task_ids` comparison below rejects the extras.
    let holders = tokio::task::spawn_blocking(move || {
        let needle = wanted.key.clone();
        Ok::<_, lazybox_store::StoreError>(
            crate::task_cache::workspaces_matching(store.as_ref(), std::slice::from_ref(&needle))?
                .into_iter()
                .filter(|ws| ws.hierarchy_task_ids().any(|task| task == &wanted))
                .map(|ws| ws.key)
                .collect::<Vec<_>>(),
        )
    })
    .await;
    let holders = match holders {
        Ok(Ok(keys)) => keys,
        Ok(Err(error)) => {
            tracing::warn!(%error, "gh shim: could not scan workspaces for a change signal");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "gh shim: workspace scan task failed");
            return;
        }
    };

    for key in holders {
        crate::polling::apply_known_record_state(config, &key, &id, state).await;
    }
    // The flip covers lifecycle only. A sweep still owns everything else the
    // mutation touched (labels, comments, the merge commit), so ask for one.
    config.poll.wake(true);
}

/// Install the `gh` shim and its escape hatch into `dir`, returning the
/// directory when a shim is actually in place.
///
/// Returns `None` when there is no real `gh` to wrap: shimming a command the
/// box does not have would turn "gh: command not found" into a confusing
/// lazybox error, and the PATH entry would be pure noise.
pub fn install(dir: &Path, launcher: &Path, path_env: Option<&str>) -> Option<PathBuf> {
    let real = real_gh(dir, path_env)?;
    std::fs::create_dir_all(dir).ok()?;
    write_script(
        &dir.join("gh"),
        &format!(
            "#!/bin/sh\n# {} — lazybox gh shim (#1801): routes reads, quota and\n\
             # change signals through the daemon. Escape hatches: `gh.real`, or\n\
             # {}=0 in the environment.\nexec {} gh \"$@\"\n",
            lazybox_ipc::gh_shim::SHIM_MARKER,
            lazybox_ipc::gh_shim::SHIM_OPT_OUT_ENV,
            shell_quote(&launcher.to_string_lossy()),
        ),
    )?;
    write_script(
        &dir.join("gh.real"),
        &format!(
            "#!/bin/sh\n# Real gh, unshimmed (#1801).\nexec {} \"$@\"\n",
            shell_quote(&real.to_string_lossy()),
        ),
    )?;
    Some(dir.to_path_buf())
}

/// The first executable `gh` on `path_env` that is not the shim itself.
///
/// Two independent guards, because either one alone has a hole. Skipping
/// `shim_dir` handles the normal case, but that path is derived from
/// configuration: when `LAZYBOX_GH_SHIM_DIR` is absent the caller falls back
/// to `<home>/shims`, and a `LAZYBOX_HOME` naming a different profile than the
/// shim actually on `PATH` makes the two disagree — at which point a
/// directory-only guard resolves `gh` straight back to the shim and the
/// session fork-bombs. So a candidate carrying [`lazybox_ipc::gh_shim::SHIM_MARKER`] is rejected on
/// content as well, which holds whatever the paths say.
///
/// Neither guard is trusted to be sufficient: the caller also counts depth
/// (see [`lazybox_ipc::gh_shim::SHIM_DEPTH_ENV`]) so recursion stays bounded
/// even if both miss.
pub fn real_gh(shim_dir: &Path, path_env: Option<&str>) -> Option<PathBuf> {
    let path = match path_env {
        Some(path) => path.to_string(),
        None => std::env::var("PATH").ok()?,
    };
    path.split(':')
        .filter(|segment| !segment.is_empty())
        .map(Path::new)
        .filter(|dir| *dir != shim_dir)
        .map(|dir| dir.join("gh"))
        .find(|candidate| is_executable(candidate) && !is_shim_script(candidate))
}

/// Whether `path` is one of lazybox's own generated shims.
///
/// Reads only the head of the file: the marker is on the first line, and a
/// `gh` binary is ~50 MB that must not be slurped to answer this.
fn is_shim_script(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 256];
    let Ok(read) = file.read(&mut head) else {
        return false;
    };
    String::from_utf8_lossy(&head[..read]).contains(lazybox_ipc::gh_shim::SHIM_MARKER)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Single-quote `value` for `sh`. Paths under `$HOME` routinely contain
/// spaces, and an unquoted `exec` would split one into two arguments.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Write `body` to `path` as an executable script, replacing what was there.
fn write_script(path: &Path, body: &str) -> Option<()> {
    // Rewritten unconditionally rather than diffed: the launcher path changes
    // when the profile does, and a stale shim points at a binary that may not
    // exist. It is a few hundred bytes, once per daemon boot.
    std::fs::write(path, body).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).ok()?;
    }
    Some(())
}

/// Where the shims live: `<home>/shims`.
pub fn shim_dir() -> PathBuf {
    lazybox_core::paths::home().join("shims")
}

/// The `gh` a spawned session should find first, or `None` when there is no
/// shim to put on its PATH. A pure read — the install happens once at boot.
pub fn installed_shim() -> Option<PathBuf> {
    let dir = shim_dir();
    is_executable(&dir.join("gh")).then_some(dir)
}

/// Install (or, when the feature is off, remove) the shim for this daemon.
///
/// Called once at boot, never per spawn: the spawn path only reads
/// [`installed_shim`]. Switching `gh_shim.enabled` off *removes* the script
/// rather than leaving it behind, because a shim left on PATH is still the
/// thing sessions run — a config knob that only stops taking effect on the
/// next spawn is not an off switch.
pub fn install_for_daemon() -> Option<PathBuf> {
    let dir = shim_dir();
    if !lazybox_config::Config::load()
        .unwrap_or_default()
        .providers
        .github
        .gh_shim
        .enabled
    {
        let _ = std::fs::remove_file(dir.join("gh"));
        return None;
    }
    let stable = lazybox_core::paths::stable_exe_path();
    let launcher = if stable.is_file() {
        stable
    } else {
        std::env::current_exe().ok()?
    };
    install(&dir, &launcher, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::{
        CiStatus, Label, Mergeable, ReviewStatus, Task, TaskId, TaskKind, TaskRole, WorkspaceKey,
    };
    use lazybox_ipc::Event;

    fn issue(key: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: key.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{}", key.replace('#', "/issues/")),
            repo: key.rsplit_once('#').map(|(repo, _)| repo.to_string()),
            branch: None,
            base_branch: None,
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![Label::new("bug")],
            reviewers: vec![],
            reviews: vec![],
            approval_policy: Default::default(),
            assignees: vec![],
            author: "someone".into(),
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Unknown,
            is_behind_base: false,
            merge_blocked: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            closes_issues: vec![],
            linked_tasks: vec![],
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
            parent: None,
            kind: Some(TaskKind::Issue),
            priority: None,
            state_label: None,
        }
    }

    fn state() -> GhShimState {
        GhShimState::default()
    }

    #[test]
    fn a_session_burst_is_paced_once_the_bucket_empties() {
        let mut shim = state();
        let start = Instant::now();
        // Burst of 3, refilling one a minute.
        for call in 0..3 {
            assert_eq!(shim.take_token("s", 3, 1.0, start), Ok(()), "call {call}");
        }
        let wait = shim
            .take_token("s", 3, 1.0, start)
            .expect_err("the fourth call has no token");
        assert!(
            (50..=60).contains(&wait),
            "waited {wait}s for a 1/min refill"
        );
        // Another session is untouched: the quota is per session, so one
        // fan-out cannot pace the rest of the fleet.
        assert_eq!(shim.take_token("other", 3, 1.0, start), Ok(()));
        // And the bucket does refill.
        assert_eq!(
            shim.take_token("s", 3, 1.0, start + Duration::from_secs(61)),
            Ok(())
        );
    }

    #[test]
    fn the_bucket_never_refills_past_its_burst() {
        let mut shim = state();
        let start = Instant::now();
        assert_eq!(shim.take_token("s", 2, 60.0, start), Ok(()));
        // An hour idle must not bank an hour of tokens.
        let much_later = start + Duration::from_secs(3600);
        assert_eq!(shim.take_token("s", 2, 60.0, much_later), Ok(()));
        assert_eq!(shim.take_token("s", 2, 60.0, much_later), Ok(()));
        assert!(shim.take_token("s", 2, 60.0, much_later).is_err());
    }

    #[test]
    fn an_identical_read_is_served_from_the_first_sessions_answer() {
        let mut shim = state();
        let now = Instant::now();
        let ttl = Duration::from_secs(90);
        assert_eq!(shim.cached("k", ttl, now), None, "nothing cached yet");
        shim.store("k".into(), "issue body".into(), ttl, now);
        assert_eq!(shim.cached("k", ttl, now), Some("issue body".into()));
        // A different question is a different key.
        assert_eq!(shim.cached("other", ttl, now), None);
        // And the answer expires rather than going stale forever.
        assert_eq!(shim.cached("k", ttl, now + Duration::from_secs(91)), None);
    }

    #[test]
    fn the_cache_is_bounded_by_entry_count_regardless_of_ttl() {
        // The regression: `store` evicted by age only, so a configured
        // `read_cache_ttl: 24h` retained every distinct read of a fleet-day.
        let mut shim = state();
        let start = Instant::now();
        let forever = Duration::from_secs(24 * 3600);
        let total = MAX_CACHE_ENTRIES + 50;
        for n in 0..total {
            // A distinct instant per store, as real arrivals have — eviction
            // orders by age, so a fixture that ties every timestamp would be
            // asserting on which of several equally-old entries got picked.
            let at = start + Duration::from_millis(n as u64);
            shim.store(format!("o/r\u{1f}{n}"), "body".into(), forever, at);
        }
        let now = start + Duration::from_millis(total as u64);
        assert_eq!(shim.cache.len(), MAX_CACHE_ENTRIES);
        // Oldest evicted first, so the most recently asked questions survive
        // and the first ones asked are the ones that went.
        assert!(
            shim.cached(&format!("o/r\u{1f}{}", total - 1), forever, now)
                .is_some()
        );
        assert_eq!(shim.cached("o/r\u{1f}0", forever, now), None);
        assert_eq!(shim.cached("o/r\u{1f}49", forever, now), None);
    }

    #[test]
    fn the_cache_is_bounded_by_bytes_too() {
        // Few enough entries to pass the count cap, large enough to blow the
        // byte cap — the shape a fleet reading `gh issue list --json` makes.
        let mut shim = state();
        let now = Instant::now();
        let forever = Duration::from_secs(24 * 3600);
        let big = "x".repeat(128 * 1024);
        for n in 0..400 {
            shim.store(
                format!("o/r\u{1f}{n}"),
                big.clone(),
                forever,
                now + Duration::from_millis(n),
            );
        }
        let bytes: usize = shim.cache.values().map(|entry| entry.stdout.len()).sum();
        assert!(bytes <= MAX_CACHE_BYTES, "cache held {bytes} bytes");
        assert!(shim.cache.len() < 400, "eviction must have happened");
    }

    #[tokio::test]
    async fn a_configured_ttl_cannot_outlive_the_clamp() {
        // A 24h TTL in config must not make a 24h-old answer servable.
        let config = ServerConfig::in_memory();
        let key = "gh\u{1f}ambient\u{1f}o/r\u{1f}issue\u{1f}view\u{1f}1";
        let stored_at = Instant::now();
        config.poll.gh_shim.lock().store(
            key.into(),
            "stale".into(),
            Duration::from_secs(24 * 3600),
            stored_at,
        );
        // Past the clamp, inside the configured TTL.
        let later = stored_at + MAX_READ_CACHE_TTL + Duration::from_secs(1);
        assert_eq!(
            config.poll.gh_shim.lock().cached(
                key,
                Duration::from_secs(24 * 3600).min(MAX_READ_CACHE_TTL),
                later
            ),
            None,
        );
    }

    #[test]
    fn a_mutation_drops_the_reads_scoped_to_its_repo() {
        let mut shim = state();
        let now = Instant::now();
        let ttl = Duration::from_secs(90);
        shim.store(
            "acme/widget\u{1f}issue\u{1f}view\u{1f}12".into(),
            "open".into(),
            ttl,
            now,
        );
        shim.store(
            "acme/widget\u{1f}issue\u{1f}list".into(),
            "12 open".into(),
            ttl,
            now,
        );
        shim.store(
            "acme/other\u{1f}issue\u{1f}view\u{1f}12".into(),
            "open".into(),
            ttl,
            now,
        );
        shim.invalidate_repo(Some("acme/widget"));
        assert_eq!(
            shim.cached("acme/widget\u{1f}issue\u{1f}view\u{1f}12", ttl, now),
            None,
            "serving the pre-close answer would make the dedupe the source of staleness",
        );
        assert_eq!(
            shim.cached("acme/widget\u{1f}issue\u{1f}list", ttl, now),
            None,
            "a list that included the record is falsified too, and never names it",
        );
        assert!(
            shim.cached("acme/other\u{1f}issue\u{1f}view\u{1f}12", ttl, now)
                .is_some(),
            "another repo's reads are untouched",
        );
        // A mutation whose repo could not be named invalidates everything
        // rather than guessing which scope it fell in.
        shim.invalidate_repo(None);
        assert_eq!(
            shim.cached("acme/other\u{1f}issue\u{1f}view\u{1f}12", ttl, now),
            None
        );
    }

    #[tokio::test]
    async fn admit_serves_the_cache_without_spending_a_token() {
        let config = ServerConfig::in_memory();
        let key = "acme/widget\u{1f}issue\u{1f}view\u{1f}12";
        config.poll.gh_shim.lock().store(
            key.into(),
            "cached body".into(),
            Duration::from_secs(90),
            Instant::now(),
        );
        let session = SessionKey::from("github-acme-widget-12");
        for _ in 0..200 {
            match admit(&config, Some(&session), GhCallKind::Read, Some(key)) {
                GhVerdict::Cached { stdout } => assert_eq!(stdout, "cached body"),
                other => panic!("a cached read must never be throttled: {other:?}"),
            }
        }
        // Two hundred served reads left the bucket untouched, so the session
        // can still make a real call.
        assert_eq!(
            admit(&config, Some(&session), GhCallKind::Read, Some("miss")),
            GhVerdict::Allow
        );
    }

    #[tokio::test]
    async fn a_completed_read_answers_the_next_session() {
        let config = ServerConfig::in_memory();
        let key = "acme/widget\u{1f}issue\u{1f}view\u{1f}12".to_string();
        let first = SessionKey::from("session-a");
        let second = SessionKey::from("session-b");
        assert_eq!(
            admit(&config, Some(&first), GhCallKind::Read, Some(&key)),
            GhVerdict::Allow,
            "the first asker pays",
        );
        completed(&config, Some(key.clone()), Some("#12 open\n".into()), None).await;
        assert_eq!(
            admit(&config, Some(&second), GhCallKind::Read, Some(&key)),
            GhVerdict::Cached {
                stdout: "#12 open\n".into()
            },
            "the second session must not pay for the same question",
        );
    }

    #[tokio::test]
    async fn closing_a_pr_flips_its_row_without_reading_github() {
        let config = ServerConfig::in_memory();
        let mut task = issue("acme/widget#12");
        task.kind = Some(TaskKind::Pr);
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        crate::polling::upsert(&config, task).await;
        assert_eq!(
            crate::polling::load_workspace(&config, &key)
                .expect("row")
                .pr
                .expect("pr")
                .state,
            TaskState::Open
        );

        let mut events = config.bus.subscribe();
        completed(
            &config,
            None,
            None,
            Some(GhRecordChange {
                reference: "acme/widget#12".into(),
                repo: None,
                kind: GhChangeKind::Closed,
            }),
        )
        .await;

        // No provider client is configured on this daemon, so nothing here
        // *could* have read GitHub: the flip is derived from what the session
        // already knew, which is the whole point of the signal.
        let pr = crate::polling::load_workspace(&config, &key)
            .expect("row")
            .pr
            .expect("pr");
        assert_eq!(pr.state, TaskState::Closed);
        assert!(pr.closed_at.is_some());
        assert!(
            std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
                event,
                Event::WorkspaceUpserted(ws)
                    if ws.pr.as_ref().is_some_and(|t| t.state == TaskState::Closed)
            )),
            "the client has to be told, or the row only flips on the next restart",
        );
    }

    #[tokio::test]
    async fn closing_an_issue_retires_its_idle_row_the_way_the_poll_would_have() {
        let config = ServerConfig::in_memory();
        let task = issue("acme/widget#12");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        crate::polling::upsert(&config, task).await;
        assert!(crate::polling::load_workspace(&config, &key).is_some());

        completed(
            &config,
            None,
            None,
            Some(GhRecordChange {
                reference: "acme/widget#12".into(),
                repo: None,
                kind: GhChangeKind::Closed,
            }),
        )
        .await;

        // The signal routes into the same terminal-state cleanup the daemon's
        // own close does, and #552's safe-auto-remove reaps a clean,
        // session-less closed issue outright. That is the point: the row
        // leaves the inbox now rather than whenever the poller next has the
        // budget to rediscover the close. A workspace with a live terminal
        // gets the keep/remove modal instead, exactly as it does today.
        assert!(
            crate::polling::load_workspace(&config, &key).is_none(),
            "a closed, idle issue must not linger in the inbox",
        );
    }

    #[tokio::test]
    async fn a_second_signal_for_an_already_closed_record_changes_nothing() {
        let config = ServerConfig::in_memory();
        let mut task = issue("acme/widget#12");
        task.kind = Some(TaskKind::Pr);
        task.state = TaskState::Closed;
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        crate::polling::upsert(&config, task).await;
        let before = crate::polling::load_workspace(&config, &key)
            .expect("row")
            .pr
            .expect("pr")
            .updated_at;

        completed(
            &config,
            None,
            None,
            Some(GhRecordChange {
                reference: "acme/widget#12".into(),
                repo: None,
                kind: GhChangeKind::Closed,
            }),
        )
        .await;

        // A retried `gh` invocation must not re-stamp the row or re-fire the
        // one-shot cleanup: the transition already happened.
        assert_eq!(
            crate::polling::load_workspace(&config, &key)
                .expect("row")
                .pr
                .expect("pr")
                .updated_at,
            before,
        );
    }

    #[tokio::test]
    async fn a_touched_record_is_left_for_the_poller() {
        let config = ServerConfig::in_memory();
        let task = issue("acme/widget#12");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        crate::polling::upsert(&config, task).await;
        completed(
            &config,
            None,
            None,
            Some(GhRecordChange {
                reference: "acme/widget#12".into(),
                repo: None,
                kind: GhChangeKind::Touched,
            }),
        )
        .await;
        // A comment changes text only GitHub holds. Guessing at a state here
        // would be inventing provider state the daemon cannot vouch for.
        assert_eq!(
            crate::polling::load_workspace(&config, &key)
                .expect("row")
                .gh_issues[0]
                .state,
            TaskState::Open
        );
    }

    #[tokio::test]
    async fn an_unparseable_reference_changes_nothing() {
        let config = ServerConfig::in_memory();
        let task = issue("acme/widget#12");
        let key = WorkspaceKey::new(lazybox_core::workspace_key_for(&task));
        crate::polling::upsert(&config, task).await;
        completed(
            &config,
            None,
            None,
            Some(GhRecordChange {
                reference: "   ".into(),
                repo: None,
                kind: GhChangeKind::Closed,
            }),
        )
        .await;
        assert_eq!(
            crate::polling::load_workspace(&config, &key)
                .expect("row")
                .gh_issues[0]
                .state,
            TaskState::Open
        );
    }

    #[test]
    fn the_real_gh_is_resolved_past_the_shim_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let shim_dir = root.path().join("shims");
        let real_dir = root.path().join("usr-bin");
        std::fs::create_dir_all(&shim_dir).expect("mkdir");
        std::fs::create_dir_all(&real_dir).expect("mkdir");
        write_script(&shim_dir.join("gh"), "#!/bin/sh\nexit 0\n").expect("shim");
        write_script(&real_dir.join("gh"), "#!/bin/sh\nexit 0\n").expect("real");

        let path = format!("{}:{}", shim_dir.display(), real_dir.display());
        // The shim's own PATH contains the shim, so resolving by name would
        // find itself and fork-bomb the session on the first `gh` call.
        assert_eq!(
            real_gh(&shim_dir, Some(&path)),
            Some(real_dir.join("gh")),
            "the shim must never resolve to itself",
        );
        // With nothing but the shim on PATH there is no real gh to run.
        assert_eq!(
            real_gh(&shim_dir, Some(&shim_dir.display().to_string())),
            None
        );
    }

    #[test]
    fn a_shim_is_skipped_by_content_even_from_the_wrong_directory() {
        // The regression: `LAZYBOX_GH_SHIM_DIR` stripped and `LAZYBOX_HOME`
        // naming another profile made the caller pass a shim_dir that is not
        // the shim actually on PATH. With directory equality as the only
        // guard, `real_gh` returned the shim itself and the session fork
        // bombed — an unbounded fork of a 200 MB binary.
        let root = tempfile::tempdir().expect("tempdir");
        let on_path = root.path().join("real-profile-shims");
        let believed = root.path().join("other-profile-shims");
        let real_dir = root.path().join("usr-bin");
        for dir in [&on_path, &believed, &real_dir] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        write_script(&real_dir.join("gh"), "#!/bin/sh\nexit 0\n").expect("real");
        install(
            &on_path,
            &root.path().join("lazybox"),
            Some(&real_dir.display().to_string()),
        )
        .expect("shim installed");

        let path = format!("{}:{}", on_path.display(), real_dir.display());
        assert_eq!(
            real_gh(&believed, Some(&path)),
            Some(real_dir.join("gh")),
            "a shim must be recognised by its marker wherever it sits, not only \
             when the caller already knows its directory",
        );
        // And with nothing but an unrecognised-by-path shim available, the
        // answer is "no real gh", never the shim.
        assert_eq!(
            real_gh(&believed, Some(&on_path.display().to_string())),
            None
        );
    }

    #[test]
    fn install_writes_an_executable_shim_and_an_escape_hatch() {
        let root = tempfile::tempdir().expect("tempdir");
        let shim_dir = root.path().join("shims");
        let real_dir = root.path().join("usr bin");
        std::fs::create_dir_all(&real_dir).expect("mkdir");
        write_script(&real_dir.join("gh"), "#!/bin/sh\nexit 0\n").expect("real");
        let launcher = root.path().join("bin dir").join("lazybox");
        std::fs::create_dir_all(launcher.parent().expect("parent")).expect("mkdir");

        let installed = install(&shim_dir, &launcher, Some(&real_dir.display().to_string()))
            .expect("installed");
        assert_eq!(installed, shim_dir);
        assert!(is_executable(&shim_dir.join("gh")));
        assert!(is_executable(&shim_dir.join("gh.real")));

        let shim = std::fs::read_to_string(shim_dir.join("gh")).expect("read");
        assert!(shim.contains("lazybox"), "{shim}");
        // Paths with spaces are routine under $HOME; an unquoted exec would
        // split one into two arguments and run the wrong thing.
        assert!(
            shim.contains(&format!("'{}'", launcher.display())),
            "{shim}"
        );
        let escape = std::fs::read_to_string(shim_dir.join("gh.real")).expect("read");
        assert!(
            escape.contains(&format!("'{}'", real_dir.join("gh").display())),
            "{escape}"
        );
    }

    #[test]
    fn install_declines_when_there_is_no_real_gh_to_wrap() {
        let root = tempfile::tempdir().expect("tempdir");
        let shim_dir = root.path().join("shims");
        // Shimming a command the box doesn't have turns "gh: command not
        // found" into a confusing lazybox error for no benefit.
        assert_eq!(install(&shim_dir, Path::new("/bin/true"), Some("")), None);
        assert!(!shim_dir.join("gh").exists());
    }

    #[test]
    fn shell_quoting_survives_a_quote_in_the_path() {
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
    }
}
