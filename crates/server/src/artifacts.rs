//! The agent artifact channel's daemon half (#1822).
//!
//! An agent hands lazybox a markdown document by writing a file into
//! [`ARTIFACT_SPOOL_RELATIVE_PATH`] in its worktree. Nothing enters the PTY
//! stream, so nothing has to survive a repaint and no agent capability is
//! required beyond writing a file — the channel works for `GenericCli`
//! exactly as it does for Claude. See `lazybox_core::artifact` for the file's
//! shape and `docs/agent-artifact-channel.md` (#1818) for why the VT stays a
//! VT.
//!
//! ## Watched, not polled through the provider tiers
//!
//! `polling/scheduler.rs`'s tiers pace *network* work against a shared
//! GitHub budget; a spool scan is a `read_dir` of one local directory and
//! shares nothing with them. It gets its own low-frequency sweep instead, on
//! the `working_watchdog` pattern — a plain ticker. Polling rather than a
//! filesystem watch because the workspace has no fs-watch dependency today,
//! and adding one means a new crate through `cargo-deny` and `machete` plus a
//! cross-platform inotify / FSEvents surface, to notice a local file a few
//! seconds sooner.
//!
//! The sweep reads a **watch set**, not the store: decoding every workspace
//! row means parsing each one's whole activity feed (the cost
//! `task_cache::workspaces_matching` exists to avoid), and paying that every
//! few seconds forever to find a directory that is usually empty is the wrong
//! trade. The set is seeded once at startup and extended at each spawn, which
//! is also where the spool's git exclusion lands.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use lazybox_core::{
    ARTIFACT_EXTENSION, ARTIFACT_MAX_BYTES, ARTIFACT_MAX_PER_WORKSPACE, ARTIFACT_MAX_TOTAL_BYTES,
    ARTIFACT_SPOOL_RELATIVE_PATH, Artifact, Workspace, WorkspaceKey,
};
use lazybox_ipc::Event;

use crate::ServerConfig;

/// How often the sweep re-reads the watch set's spools.
///
/// An artifact is something the agent just told you about, so a sweep slower
/// than a few seconds reads as lazybox having missed it. Each tick is one
/// `read_dir` per watched worktree over a directory that is almost always
/// empty, so the frequency costs little — and unlike a provider poll it
/// spends no shared budget.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3);

/// `written_at` for a spool file whose filesystem reports no modification
/// time. A constant, because the value is compared for equality by the
/// broadcast's change gate — see [`scan_worktree`].
const UNDATED: DateTime<Utc> = DateTime::UNIX_EPOCH;

/// What one workspace's spool currently holds, as the daemon last read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceArtifacts {
    /// Newest first, capped at [`ARTIFACT_MAX_PER_WORKSPACE`].
    pub artifacts: Vec<Artifact>,
    /// How many older artifacts the cap left out. Carried rather than
    /// discarded so the reader can say a set is partial.
    pub hidden: usize,
}

/// The watch set and the attached artifacts derived from it.
///
/// Cloneable (shared `Arc`), lives on [`ServerConfig`], and is the daemon's
/// authority for "what artifacts does this workspace have" — the on-disk
/// spool is the durable copy, so a restart re-derives this rather than
/// persisting it.
#[derive(Clone, Default)]
pub struct ArtifactSpool {
    inner: Arc<parking_lot::Mutex<Inner>>,
}

/// One spool file as `read_dir` reports it, without opening it. Two scans
/// with equal fingerprints have equal contents, so the bytes need not be
/// re-read — see [`scan_worktree`].
type Fingerprint = (String, u64, Option<SystemTime>);

/// What the last scan of one worktree's spool found: the cheap metadata
/// summary and the artifacts parsed from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SpoolScan {
    /// Sorted by name so comparison is order-independent. Deliberately
    /// empty when the scan was unstable (a file changed under the read),
    /// because an empty fingerprint set can never equal a real one and so
    /// forces the next tick to re-read.
    fingerprints: Vec<Fingerprint>,
    artifacts: Vec<Artifact>,
}

#[derive(Default)]
struct Inner {
    /// Which workspace each watched worktree belongs to.
    ///
    /// Keyed by **path**, not by workspace, and the path is canonicalized:
    /// both halves are load-bearing. Not every worktree lazybox spawns into
    /// is owned by one workspace — the `on main` checkout
    /// (`spawn_handler::main_worktree_path_under`) is
    /// `root/<scope>/<shared-main>`, shared by every workspace in the repo,
    /// and a linked checkout is the user's own clone. Keying by workspace let
    /// two of them both hold that one path, so one task's artifact surfaced
    /// on an unrelated task's row and never cleared. Keying by path makes
    /// "a worktree belongs to at most one workspace" structural: a later
    /// spawn moves it rather than sharing it. Canonicalizing makes it
    /// *one* key — an agent spawn canonicalizes its cwd and a shell spawn
    /// does not, so the same worktree arrived under two spellings and every
    /// artifact in it was counted twice.
    watched: HashMap<PathBuf, WorkspaceKey>,
    /// Last scan per worktree, so an unchanged spool costs one `read_dir`
    /// and no file reads at all.
    scanned: HashMap<PathBuf, SpoolScan>,
    /// The last set broadcast for each workspace — the change gate, so a
    /// tick over an unchanged spool sends nothing.
    attached: HashMap<WorkspaceKey, WorkspaceArtifacts>,
}

/// The one spelling of a worktree path the watch set uses.
///
/// `canonicalize` fails only when the path does not resolve, which for a
/// worktree means it is gone; the raw path is then the honest key and the
/// first scan retires it.
fn watch_key(worktree: &Path) -> PathBuf {
    std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf())
}

impl ArtifactSpool {
    /// Start watching `worktree` for `key`, and put the spool's git
    /// exclusion in place.
    ///
    /// The exclusion has to land *before* the writer, not after: the agent
    /// chooses when to write, and an unexcluded spool dirties the worktree
    /// and trips the dirty-worktree delete refusal. That is why this is
    /// called for every session spawn rather than only for agent terminals —
    /// a user who runs an agent by hand inside a shell session writes to the
    /// same spool.
    pub(crate) fn watch(&self, key: &WorkspaceKey, worktree: &Path) {
        if let Err(error) = crate::task_cache::exclude_lazybox_paths(worktree) {
            tracing::warn!(
                worktree = %worktree.display(),
                %error,
                "artifacts: could not exclude {ARTIFACT_SPOOL_RELATIVE_PATH} from git — \
                 spooled artifacts will show as untracked files"
            );
        }
        // A later spawn takes the worktree over rather than sharing it. For
        // an isolated worktree the claimant never changes; for the shared
        // `on main` checkout it is the workspace that actually spawned
        // there, which is the only non-arbitrary answer available.
        self.inner
            .lock()
            .watched
            .insert(watch_key(worktree), key.clone());
    }

    /// Start watching `worktree` without touching git.
    ///
    /// The startup seed goes through here rather than [`Self::watch`]: it
    /// walks worktrees the user may since have removed or made read-only,
    /// and a seed that writes `info/exclude` would turn reading persisted
    /// state into mutating checkouts. The spawn path is the one that has to
    /// guarantee the exclusion, and it is the one an artifact can follow.
    fn watch_without_excluding(&self, key: &WorkspaceKey, worktree: &Path) {
        self.inner
            .lock()
            .watched
            .insert(watch_key(worktree), key.clone());
    }

    /// Every workspace currently carrying artifacts, for the `Subscribe`
    /// replay. A client that connects between two changes would otherwise
    /// see none until the next one.
    pub(crate) fn snapshot(&self) -> BTreeMap<WorkspaceKey, WorkspaceArtifacts> {
        self.inner
            .lock()
            .attached
            .iter()
            .filter(|(_, found)| !found.artifacts.is_empty())
            .map(|(key, found)| (key.clone(), found.clone()))
            .collect()
    }

    /// Record a freshly-scanned set, returning it when it changed and
    /// `None` when it did not — the change gate the broadcast rides.
    ///
    /// An empty set for a workspace that had none is not a change and is
    /// forgotten entirely, so a workspace that never spools anything costs
    /// one map lookup per tick and no broadcast ever.
    fn record(&self, key: &WorkspaceKey, found: WorkspaceArtifacts) -> Option<WorkspaceArtifacts> {
        let mut inner = self.inner.lock();
        match inner.attached.get(key) {
            Some(previous) if *previous == found => None,
            None if found.artifacts.is_empty() => None,
            _ => {
                if found.artifacts.is_empty() {
                    inner.attached.remove(key);
                } else {
                    inner.attached.insert(key.clone(), found.clone());
                }
                Some(found)
            }
        }
    }

    /// The watch set, grouped into the workspaces to sweep.
    ///
    /// A workspace with an `attached` entry but no watched worktree is
    /// included with an empty path list: a worktree that moved to another
    /// workspace, or was removed, leaves the former owner still badged, and
    /// only a sweep that reaches it can clear that.
    fn targets(&self) -> Vec<(WorkspaceKey, Vec<PathBuf>)> {
        let inner = self.inner.lock();
        let mut grouped: HashMap<WorkspaceKey, Vec<PathBuf>> = inner
            .attached
            .keys()
            .map(|key| (key.clone(), Vec::new()))
            .collect();
        for (path, key) in &inner.watched {
            grouped.entry(key.clone()).or_default().push(path.clone());
        }
        grouped.into_iter().collect()
    }

    /// The last scan of each of `worktrees`, for the fingerprint skip.
    fn cached_scans(&self, worktrees: &[PathBuf]) -> HashMap<PathBuf, SpoolScan> {
        let inner = self.inner.lock();
        worktrees
            .iter()
            .filter_map(|path| {
                inner
                    .scanned
                    .get(path)
                    .map(|scan| (path.clone(), scan.clone()))
            })
            .collect()
    }

    /// Store this tick's scan of `worktree`.
    fn remember_scan(&self, worktree: &Path, scan: SpoolScan) {
        self.inner
            .lock()
            .scanned
            .insert(worktree.to_path_buf(), scan);
    }

    /// Drop a worktree that no longer exists, with its cached scan. A
    /// removed workspace or a pruned session leaves the watch set
    /// self-healing rather than re-scanning a vanished path forever.
    fn forget_worktree(&self, worktree: &Path) {
        let mut inner = self.inner.lock();
        inner.watched.remove(worktree);
        inner.scanned.remove(worktree);
    }
}

/// Spawn the sweep.
pub fn spawn(config: &ServerConfig) -> tokio::task::JoinHandle<()> {
    let config = config.clone();
    tokio::spawn(async move {
        seed_watch_set(&config).await;
        run(config, SWEEP_INTERVAL).await
    })
}

/// Seed the watch set from the persisted sessions, once.
///
/// Without this an artifact spooled by the previous daemon run stays
/// invisible until the workspace is spawned into again. One full store decode
/// at startup is the same cost the first `Subscribe` already pays.
async fn seed_watch_set(config: &ServerConfig) {
    let loaded = crate::store_blocking(&config.store, |store| {
        store.list_workspaces().map(|records| {
            records
                .into_iter()
                .filter_map(|record| record.workspace_json)
                .filter_map(|json| serde_json::from_str::<Workspace>(&json).ok())
                .map(|ws| {
                    let paths: Vec<PathBuf> = ws
                        .sessions
                        .iter()
                        .map(|session| session.worktree_path.clone())
                        .collect();
                    (ws.key, paths)
                })
                .collect::<Vec<_>>()
        })
    })
    .await;
    let workspaces = match loaded {
        Ok(workspaces) => workspaces,
        Err(error) => {
            tracing::warn!(
                %error,
                "artifacts: could not seed the watch set — spools are picked up at the next spawn"
            );
            return;
        }
    };
    for (key, paths) in workspaces {
        for path in paths.iter().filter(|path| path.is_dir()) {
            config.artifacts.watch_without_excluding(&key, path);
        }
    }
}

/// Re-scan every watched spool once per `interval`, broadcasting each
/// workspace whose set moved. Never returns in production.
async fn run(config: ServerConfig, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        sweep_once(&config).await;
    }
}

/// One pass over the watch set.
async fn sweep_once(config: &ServerConfig) {
    for (key, worktrees) in config.artifacts.targets() {
        let cached = config.artifacts.cached_scans(&worktrees);
        let scanned = match tokio::task::spawn_blocking(move || {
            worktrees
                .into_iter()
                .map(|worktree| {
                    let found = scan_worktree(&worktree, cached.get(&worktree));
                    (worktree, found)
                })
                .collect::<Vec<_>>()
        })
        .await
        {
            Ok(scanned) => scanned,
            Err(error) => {
                tracing::warn!(%error, workspace = %key, "artifacts: spool scan task failed");
                continue;
            }
        };
        let mut all: Vec<Artifact> = Vec::new();
        for (worktree, outcome) in scanned {
            match outcome {
                Some(scan) => {
                    all.extend(scan.artifacts.iter().cloned());
                    config.artifacts.remember_scan(&worktree, scan);
                }
                None => config.artifacts.forget_worktree(&worktree),
            }
        }
        if let Some(found) = config.artifacts.record(&key, collate(all)) {
            let _ = config.bus.send(Event::WorkspaceArtifacts {
                workspace_key: key,
                artifacts: found.artifacts,
                hidden: found.hidden,
            });
        }
    }
}

/// Order newest-first and apply [`ARTIFACT_MAX_PER_WORKSPACE`].
fn collate(mut artifacts: Vec<Artifact>) -> WorkspaceArtifacts {
    // Name breaks a mtime tie so the order is stable across ticks — two
    // files written in the same second must not swap places and look like a
    // change on every sweep.
    artifacts.sort_by(|a, b| {
        b.written_at
            .cmp(&a.written_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    let total = artifacts.len();
    artifacts.truncate(ARTIFACT_MAX_PER_WORKSPACE);
    // The count cap alone leaves the event's size to the *product* of the two
    // per-file bounds — 6 MiB, which nobody chose. Admit newest-first until
    // the decided total is reached; the rest are hidden exactly as the count
    // cap hides them, so the reader still says how many it is not showing.
    let mut carried = 0usize;
    let keep = artifacts
        .iter()
        .take_while(|artifact| {
            carried += artifact.body.len();
            carried <= ARTIFACT_MAX_TOTAL_BYTES
        })
        .count()
        // A single artifact larger than the whole budget is still carried:
        // dropping it would leave a workspace whose only artifact is
        // invisible, which is the failure the bounds exist to avoid.
        .max(1)
        .min(artifacts.len());
    artifacts.truncate(keep);
    let hidden = total.saturating_sub(artifacts.len());
    WorkspaceArtifacts { artifacts, hidden }
}

/// Read one worktree's spool, or `None` when the worktree itself is gone.
///
/// A missing *spool* is the normal case and reads as an empty set; a missing
/// *worktree* means the session was removed, which is what retires the path
/// from the watch set.
///
/// `cached` is the previous scan of this same worktree. Its fingerprints are
/// compared first, and the files are opened only when they differ: in the
/// steady state this is one `read_dir` and no file reads at all. Without it
/// every artifact's full bytes were re-read, re-validated as UTF-8 and
/// re-allocated every tick forever — a cost proportional to everything an
/// agent had ever written, paid whether or not anyone was looking.
fn scan_worktree(worktree: &Path, cached: Option<&SpoolScan>) -> Option<SpoolScan> {
    if !worktree.is_dir() {
        return None;
    }
    let spool = worktree.join(ARTIFACT_SPOOL_RELATIVE_PATH);
    let Ok(entries) = std::fs::read_dir(&spool) else {
        return Some(SpoolScan::default());
    };
    let mut files: Vec<(PathBuf, Fingerprint)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ARTIFACT_EXTENSION) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        files.push((path, (name, meta.len(), meta.modified().ok())));
    }
    files.sort_by(|a, b| a.1.0.cmp(&b.1.0));
    let fingerprints: Vec<Fingerprint> = files.iter().map(|(_, fp)| fp.clone()).collect();
    if let Some(cached) = cached
        && cached.fingerprints == fingerprints
        && !fingerprints.is_empty()
    {
        return Some(cached.clone());
    }

    let mut artifacts = Vec::new();
    for (path, (name, len, modified)) in files {
        // `modified()` is unavailable on some filesystems. The fallback must
        // be a CONSTANT, not `Utc::now()`: `written_at` is part of
        // `Artifact`'s `Eq` and so feeds the broadcast's change gate, and a
        // clock reading there makes every tick look like a change and
        // re-broadcasts the workspace forever. Ordering then falls back to
        // the name tie-break `collate` already applies.
        let written_at = modified.map(DateTime::from).unwrap_or(UNDATED);
        if len > ARTIFACT_MAX_BYTES {
            tracing::warn!(
                path = %path.display(),
                bytes = len,
                "artifacts: spool file past the size limit — attaching a notice in its place"
            );
            artifacts.push(Artifact::oversized(name, len, written_at));
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                // Stat-read-stat. The agent writes the spool with an ordinary
                // editor tool, not atomically, so a sweep landing inside that
                // window reads a truncated file — and a truncated markdown
                // document renders as a truncated document. If the file moved
                // under the read, keep the last known-good artifacts and
                // return no fingerprints, which forces a full re-read next
                // tick: the artifact appears one tick later, whole, instead of
                // flashing half-written.
                if std::fs::metadata(&path)
                    .map(|after| after.len() != len || after.modified().ok() != modified)
                    .unwrap_or(true)
                {
                    tracing::debug!(
                        path = %path.display(),
                        "artifacts: spool file changed under the read — retrying next tick"
                    );
                    return Some(SpoolScan {
                        fingerprints: Vec::new(),
                        artifacts: cached.map(|c| c.artifacts.clone()).unwrap_or_default(),
                    });
                }
                artifacts.push(Artifact::from_markdown(name, &contents, written_at));
            }
            Err(error) => {
                // Announced, not dropped — the same rule `Artifact::oversized`
                // follows. A file that is present but unreadable (not UTF-8,
                // permissions) used to vanish with only a log line, which
                // reads to the user as lazybox never having noticed it.
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "artifacts: could not read a spool file — attaching a notice in its place"
                );
                artifacts.push(Artifact::unreadable(name, &error.to_string(), written_at));
            }
        }
    }
    Some(SpoolScan {
        fingerprints,
        artifacts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_store::MemoryStore;

    fn spool_dir(worktree: &Path) -> PathBuf {
        let dir = worktree.join(ARTIFACT_SPOOL_RELATIVE_PATH);
        std::fs::create_dir_all(&dir).expect("mkdir spool");
        dir
    }

    fn write_artifact(worktree: &Path, name: &str, body: &str) {
        std::fs::write(spool_dir(worktree).join(name), body).expect("write artifact");
    }

    fn scan(worktree: &Path) -> Option<Vec<Artifact>> {
        scan_worktree(worktree, None).map(|s| s.artifacts)
    }

    #[test]
    fn a_worktree_with_no_spool_yields_an_empty_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(scan(dir.path()), Some(Vec::new()));
    }

    #[test]
    fn a_vanished_worktree_is_distinguishable_from_an_empty_spool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone");
        assert_eq!(scan(&path), None);
    }

    #[test]
    fn only_markdown_is_picked_up() {
        // Slice 1 is markdown only. A mermaid or image file is a later
        // slice's business — guessing at it now would attach something the
        // reader cannot render.
        let dir = tempfile::tempdir().expect("tempdir");
        write_artifact(dir.path(), "plan.md", "# Plan\n\nbody\n");
        write_artifact(dir.path(), "diagram.mmd", "graph TD;");
        write_artifact(dir.path(), "notes.txt", "hello");
        let found = scan(dir.path()).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Plan");
    }

    #[test]
    fn a_directory_in_the_spool_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(spool_dir(dir.path()).join("nested.md")).expect("mkdir");
        assert_eq!(scan(dir.path()), Some(Vec::new()));
    }

    #[test]
    fn an_oversized_artifact_is_announced_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "x".repeat(ARTIFACT_MAX_BYTES as usize + 1);
        write_artifact(dir.path(), "huge.md", &body);
        let found = scan(dir.path()).expect("scan");
        assert_eq!(found.len(), 1, "the artifact must still be attached");
        assert!(
            found[0].body.contains("past lazybox's"),
            "the body must say why it is not shown: {}",
            found[0].body
        );
    }

    #[test]
    fn collation_is_newest_first_and_names_what_the_cap_hid() {
        let at = |secs: i64| DateTime::from_timestamp(secs, 0).expect("timestamp");
        let artifacts: Vec<Artifact> = (0..ARTIFACT_MAX_PER_WORKSPACE + 3)
            .map(|i| Artifact::from_markdown(format!("a{i}.md"), "body", at(i as i64)))
            .collect();
        let collated = collate(artifacts);
        assert_eq!(collated.artifacts.len(), ARTIFACT_MAX_PER_WORKSPACE);
        assert_eq!(collated.hidden, 3);
        assert_eq!(
            collated.artifacts[0].name,
            format!("a{}.md", ARTIFACT_MAX_PER_WORKSPACE + 2),
            "the newest artifact must lead"
        );
    }

    #[test]
    fn collation_of_equal_timestamps_is_stable() {
        // Two files written in the same second must not swap places between
        // ticks — an unstable order reads as a change and re-broadcasts the
        // workspace forever.
        let at = DateTime::from_timestamp(10, 0).expect("timestamp");
        let one = collate(vec![
            Artifact::from_markdown("b.md", "x", at),
            Artifact::from_markdown("a.md", "x", at),
        ]);
        let two = collate(vec![
            Artifact::from_markdown("a.md", "x", at),
            Artifact::from_markdown("b.md", "x", at),
        ]);
        assert_eq!(one, two);
    }

    #[test]
    fn an_unchanged_spool_is_not_re_broadcast() {
        let spool = ArtifactSpool::default();
        let key = WorkspaceKey::new("github:o/r#1");
        let at = DateTime::from_timestamp(10, 0).expect("timestamp");
        let found = collate(vec![Artifact::from_markdown("a.md", "# A\n\nx", at)]);
        assert!(
            spool.record(&key, found.clone()).is_some(),
            "the first set is news"
        );
        assert!(
            spool.record(&key, found).is_none(),
            "an identical set is not"
        );
    }

    #[test]
    fn a_workspace_that_never_spools_anything_never_broadcasts() {
        let spool = ArtifactSpool::default();
        let key = WorkspaceKey::new("github:o/r#1");
        assert!(spool.record(&key, WorkspaceArtifacts::default()).is_none());
        assert!(spool.snapshot().is_empty());
    }

    #[test]
    fn a_cleared_spool_broadcasts_once_then_is_forgotten() {
        let spool = ArtifactSpool::default();
        let key = WorkspaceKey::new("github:o/r#1");
        let at = DateTime::from_timestamp(10, 0).expect("timestamp");
        spool.record(
            &key,
            collate(vec![Artifact::from_markdown("a.md", "x", at)]),
        );
        assert!(
            spool.record(&key, WorkspaceArtifacts::default()).is_some(),
            "the badge has to be cleared"
        );
        assert!(spool.snapshot().is_empty());
        assert!(
            spool.record(&key, WorkspaceArtifacts::default()).is_none(),
            "and cleared only once"
        );
    }

    /// The `on main` checkout is `root/<scope>/<shared-main>` — shared by
    /// every workspace in the repo. Keying the watch set by workspace let two
    /// of them both hold it, so one task's artifact appeared on an unrelated
    /// task's row and never cleared.
    #[tokio::test]
    async fn a_shared_checkout_belongs_to_one_workspace_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared_main = dir.path().join("repos/o/r/main");
        std::fs::create_dir_all(&shared_main).expect("mkdir");
        write_artifact(
            &shared_main,
            "findings.md",
            "# Findings for #10\n\ndetail\n",
        );

        let config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
        let ten = WorkspaceKey::new("github:o/r#10");
        let twenty = WorkspaceKey::new("github:o/r#20");
        config.artifacts.watch(&twenty, &shared_main);
        config.artifacts.watch(&ten, &shared_main);

        let mut events = config.bus.subscribe();
        sweep_once(&config).await;
        let mut badged: Vec<WorkspaceKey> = Vec::new();
        while let Ok(Event::WorkspaceArtifacts {
            workspace_key,
            artifacts,
            ..
        }) = events.try_recv()
        {
            if !artifacts.is_empty() {
                badged.push(workspace_key);
            }
        }
        assert_eq!(
            badged,
            vec![ten.clone()],
            "only the workspace that last spawned there may carry its artifacts"
        );

        // And when the other workspace takes it over, the first one's badge
        // is cleared rather than left pinned to a worktree it no longer owns.
        config.artifacts.watch(&twenty, &shared_main);
        let mut events = config.bus.subscribe();
        sweep_once(&config).await;
        let mut moved: Vec<(WorkspaceKey, usize)> = Vec::new();
        while let Ok(Event::WorkspaceArtifacts {
            workspace_key,
            artifacts,
            ..
        }) = events.try_recv()
        {
            moved.push((workspace_key, artifacts.len()));
        }
        moved.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        assert_eq!(moved, vec![(ten, 0), (twenty, 1)]);
    }

    /// An agent spawn canonicalizes its cwd and a shell spawn does not, so
    /// the same worktree reached the watch set under two spellings and every
    /// artifact in it was counted twice.
    #[tokio::test]
    #[cfg(unix)]
    async fn two_spellings_of_one_worktree_are_one_watch_entry() {
        // The symlink is built rather than borrowed from the platform: on
        // macOS `/tmp` happens to be one and on Linux it is not, so a test
        // that leaned on `temp_dir()` would quietly stop testing anything on
        // the runner CI actually uses.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        let raw = dir.path().join("link").join("wt");
        std::fs::create_dir_all(real.join("wt")).expect("mkdir");
        std::os::unix::fs::symlink(&real, dir.path().join("link")).expect("symlink");
        let canonical = std::fs::canonicalize(&raw).expect("canonicalize");
        assert_ne!(raw, canonical, "the two spellings must actually differ");
        write_artifact(&raw, "plan.md", "# The plan\n\nStep one.\n");

        let config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
        let key = WorkspaceKey::new("github:o/r#1");
        config.artifacts.watch(&key, &canonical);
        config.artifacts.watch(&key, &raw);
        assert_eq!(
            config.artifacts.targets().len(),
            1,
            "one workspace, however many spellings"
        );

        let mut events = config.bus.subscribe();
        sweep_once(&config).await;
        match events.try_recv().expect("an artifact event") {
            Event::WorkspaceArtifacts { artifacts, .. } => assert_eq!(
                artifacts.len(),
                1,
                "one file on disk is one artifact: {artifacts:?}"
            ),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// An unchanged spool must cost one `read_dir` and no file reads. The
    /// proof is that making the file unreadable *without* changing its
    /// fingerprint leaves the cached artifact standing.
    #[test]
    fn an_unchanged_spool_is_not_re_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_artifact(dir.path(), "plan.md", "# The plan\n\nStep one.\n");
        let first = scan_worktree(dir.path(), None).expect("scan");
        assert_eq!(first.artifacts.len(), 1);

        // Same name, same length, same mtime — a re-read would see the new
        // bytes, the fingerprint skip does not.
        let path = spool_dir(dir.path()).join("plan.md");
        let meta = std::fs::metadata(&path).expect("stat");
        let rewritten = "# Rewritten\n\nStep 2!!\n";
        assert_eq!(
            rewritten.len() as u64,
            meta.len(),
            "the fixture only tests the skip while the length is unchanged"
        );
        std::fs::write(&path, rewritten).expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen")
            .set_modified(meta.modified().expect("mtime"))
            .expect("restore mtime");

        let second = scan_worktree(dir.path(), Some(&first)).expect("scan");
        assert_eq!(
            second.artifacts[0].title, "The plan",
            "an equal fingerprint must not re-read the file"
        );
    }

    /// A changed fingerprint does re-read — the skip must not pin a stale
    /// artifact once the file actually moves.
    #[test]
    fn a_changed_spool_is_re_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_artifact(dir.path(), "plan.md", "# The plan\n\nStep one.\n");
        let first = scan_worktree(dir.path(), None).expect("scan");
        write_artifact(dir.path(), "plan.md", "# Rewritten\n\nStep two, longer.\n");
        let second = scan_worktree(dir.path(), Some(&first)).expect("scan");
        assert_eq!(second.artifacts[0].title, "Rewritten");
    }

    /// The unstable-read sentinel: a scan that caught a file mid-write
    /// returns no fingerprints, and an empty fingerprint set must never
    /// satisfy the skip — otherwise the last known-good artifacts would be
    /// pinned and the finished file never read.
    #[test]
    fn an_empty_fingerprint_set_forces_a_re_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_artifact(dir.path(), "plan.md", "# The plan\n\nStep one.\n");
        let unstable = SpoolScan {
            fingerprints: Vec::new(),
            artifacts: vec![Artifact::from_markdown("plan.md", "# Stale\n", UNDATED)],
        };
        let next = scan_worktree(dir.path(), Some(&unstable)).expect("scan");
        assert_eq!(next.artifacts[0].title, "The plan");
        assert!(!next.fingerprints.is_empty());
    }

    /// A file that is present but unreadable is announced, not dropped —
    /// the same rule the size cap follows. Dropping it silently reads to the
    /// user as lazybox never having noticed the artifact.
    #[test]
    fn an_unreadable_artifact_is_announced_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            spool_dir(dir.path()).join("junk.md"),
            [b'#', b' ', b'H', b'i', b'\n', 0xff, 0xfe],
        )
        .expect("write non-utf8");
        let found = scan(dir.path()).expect("scan");
        assert_eq!(found.len(), 1, "the artifact must still be attached");
        assert!(
            found[0].body.contains("could not read"),
            "the body must say why: {}",
            found[0].body
        );
    }

    /// `written_at` feeds `Artifact`'s `Eq`, which feeds the broadcast's
    /// change gate. A clock reading as the no-mtime fallback would make every
    /// tick look like a change and re-broadcast forever.
    #[test]
    fn a_missing_mtime_yields_a_stable_timestamp() {
        // The guard that actually prevents the regression is that the
        // fallback is a `const`: a clock read cannot appear in a const
        // initializer, so re-introducing `Utc::now()` here stops compiling
        // rather than silently re-broadcasting the workspace every tick.
        const STABLE: DateTime<Utc> = UNDATED;
        assert_eq!(
            Artifact::from_markdown("a.md", "x", STABLE),
            Artifact::from_markdown("a.md", "x", UNDATED)
        );
        assert_ne!(UNDATED, Utc::now(), "and it is not a clock reading");
    }

    /// The count cap left the event's size to the product of the two
    /// per-file bounds. The decided bound is the total.
    #[test]
    fn collation_bounds_the_total_bytes_not_only_the_count() {
        let at = |secs: i64| DateTime::from_timestamp(secs, 0).expect("timestamp");
        let big = "x".repeat(ARTIFACT_MAX_TOTAL_BYTES / 4);
        let artifacts: Vec<Artifact> = (0..ARTIFACT_MAX_PER_WORKSPACE)
            .map(|i| Artifact::from_markdown(format!("a{i}.md"), &big, at(i as i64)))
            .collect();
        let collated = collate(artifacts);
        let carried: usize = collated.artifacts.iter().map(|a| a.body.len()).sum();
        assert!(
            carried <= ARTIFACT_MAX_TOTAL_BYTES,
            "carried {carried} bytes past the total bound"
        );
        assert!(collated.hidden > 0, "and it must say how many it hid");
    }

    /// One artifact larger than the whole budget is still carried: dropping
    /// it would leave a workspace whose only artifact is invisible.
    #[test]
    fn a_single_oversized_body_is_still_carried() {
        let at = DateTime::from_timestamp(0, 0).expect("timestamp");
        let body = "x".repeat(ARTIFACT_MAX_TOTAL_BYTES * 2);
        let collated = collate(vec![Artifact::from_markdown("a.md", &body, at)]);
        assert_eq!(collated.artifacts.len(), 1);
        assert_eq!(collated.hidden, 0);
    }

    #[tokio::test]
    async fn the_sweep_broadcasts_a_new_artifact_and_retires_a_vanished_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(&worktree).expect("mkdir");
        let config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
        let mut events = config.bus.subscribe();
        let key = WorkspaceKey::new("github:o/r#1");
        config.artifacts.watch(&key, &worktree);

        write_artifact(&worktree, "plan.md", "# The plan\n\nStep one.\n");
        sweep_once(&config).await;
        match events.try_recv().expect("an artifact event") {
            Event::WorkspaceArtifacts {
                workspace_key,
                artifacts,
                hidden,
            } => {
                assert_eq!(workspace_key, key);
                assert_eq!(hidden, 0);
                assert_eq!(artifacts.len(), 1);
                assert_eq!(artifacts[0].title, "The plan");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        sweep_once(&config).await;
        assert!(
            events.try_recv().is_err(),
            "an unchanged spool must not re-broadcast"
        );

        std::fs::remove_dir_all(&worktree).expect("remove the worktree");
        sweep_once(&config).await;
        assert!(
            config.artifacts.targets().is_empty(),
            "a vanished worktree must leave the watch set"
        );
    }
}
