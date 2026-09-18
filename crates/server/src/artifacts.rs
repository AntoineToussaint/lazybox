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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use lazybox_core::{
    ARTIFACT_EXTENSION, ARTIFACT_MAX_BYTES, ARTIFACT_MAX_PER_WORKSPACE,
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

#[derive(Default)]
struct Inner {
    /// Worktrees to scan, per workspace. A workspace's sessions can sit in
    /// more than one worktree, and every one of them may host an agent.
    watched: HashMap<WorkspaceKey, BTreeSet<PathBuf>>,
    /// The last set broadcast for each workspace — the change gate, so a
    /// tick over an unchanged spool sends nothing.
    attached: HashMap<WorkspaceKey, WorkspaceArtifacts>,
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
        self.inner
            .lock()
            .watched
            .entry(key.clone())
            .or_default()
            .insert(worktree.to_path_buf());
    }

    /// Start watching `worktree` without touching git.
    ///
    /// The startup seed goes through here rather than [`Self::watch`]: it
    /// walks worktrees the user may since have removed or made read-only,
    /// and a seed that writes `info/exclude` would turn reading persisted
    /// state into mutating checkouts. The spawn path is the one that has to
    /// guarantee the exclusion, and it is the one an artifact can follow.
    fn watch_without_excluding(&self, key: &WorkspaceKey, worktree: PathBuf) {
        self.inner
            .lock()
            .watched
            .entry(key.clone())
            .or_default()
            .insert(worktree);
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

    /// The watch set, as pairs to scan.
    fn targets(&self) -> Vec<(WorkspaceKey, Vec<PathBuf>)> {
        self.inner
            .lock()
            .watched
            .iter()
            .map(|(key, paths)| (key.clone(), paths.iter().cloned().collect()))
            .collect()
    }

    /// Drop a worktree that no longer exists. A removed workspace or a
    /// pruned session leaves the watch set self-healing rather than
    /// re-scanning a vanished path forever.
    fn forget_worktree(&self, key: &WorkspaceKey, worktree: &Path) {
        let mut inner = self.inner.lock();
        if let Some(paths) = inner.watched.get_mut(key) {
            paths.remove(worktree);
            if paths.is_empty() {
                inner.watched.remove(key);
            }
        }
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
        for path in paths.into_iter().filter(|path| path.is_dir()) {
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
        let scanned = match tokio::task::spawn_blocking(move || {
            worktrees
                .into_iter()
                .map(|worktree| {
                    let found = scan_worktree(&worktree);
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
                Some(artifacts) => all.extend(artifacts),
                None => config.artifacts.forget_worktree(&key, &worktree),
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
    let hidden = artifacts.len().saturating_sub(ARTIFACT_MAX_PER_WORKSPACE);
    artifacts.truncate(ARTIFACT_MAX_PER_WORKSPACE);
    WorkspaceArtifacts { artifacts, hidden }
}

/// Read one worktree's spool, or `None` when the worktree itself is gone.
///
/// A missing *spool* is the normal case and reads as an empty set; a missing
/// *worktree* means the session was removed, which is what retires the path
/// from the watch set.
fn scan_worktree(worktree: &Path) -> Option<Vec<Artifact>> {
    if !worktree.is_dir() {
        return None;
    }
    let spool = worktree.join(ARTIFACT_SPOOL_RELATIVE_PATH);
    let Ok(entries) = std::fs::read_dir(&spool) else {
        return Some(Vec::new());
    };
    let mut artifacts = Vec::new();
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
        let written_at: DateTime<Utc> = meta
            .modified()
            .map(DateTime::from)
            .unwrap_or_else(|_| Utc::now());
        if meta.len() > ARTIFACT_MAX_BYTES {
            tracing::warn!(
                path = %path.display(),
                bytes = meta.len(),
                "artifacts: spool file past the size limit — attaching a notice in its place"
            );
            artifacts.push(Artifact::oversized(name, meta.len(), written_at));
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => artifacts.push(Artifact::from_markdown(name, &contents, written_at)),
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "artifacts: could not read a spool file"
            ),
        }
    }
    Some(artifacts)
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

    #[test]
    fn a_worktree_with_no_spool_yields_an_empty_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(scan_worktree(dir.path()), Some(Vec::new()));
    }

    #[test]
    fn a_vanished_worktree_is_distinguishable_from_an_empty_spool() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone");
        assert_eq!(scan_worktree(&path), None);
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
        let found = scan_worktree(dir.path()).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Plan");
    }

    #[test]
    fn a_directory_in_the_spool_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(spool_dir(dir.path()).join("nested.md")).expect("mkdir");
        assert_eq!(scan_worktree(dir.path()), Some(Vec::new()));
    }

    #[test]
    fn an_oversized_artifact_is_announced_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "x".repeat(ARTIFACT_MAX_BYTES as usize + 1);
        write_artifact(dir.path(), "huge.md", &body);
        let found = scan_worktree(dir.path()).expect("scan");
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
