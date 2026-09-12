//! Migration off #1376's per-workspace Codex credential homes.
//!
//! #1376 gave every workspace its own `CODEX_HOME` under
//! `agent-homes/codex/<session>` so a re-auth in one pane could not re-log
//! the machine-wide `~/.codex` the rest of the fleet shares. That forked
//! refresh-token state and missed keyring credentials, so Codex is back on
//! the shared login — which leaves each legacy home holding conversation
//! rollouts `codex resume` can no longer find.
//!
//! This module links those rollouts into the shared home. It sweeps **every
//! legacy home at each daemon start**, importing any rollout it has not
//! already recorded, rather than running per workspace-spawn: a
//! migration keyed to "the next time an agent spawns in this workspace"
//! never reaches a workspace the user does not reopen, and the resulting
//! rollouts are the only copy in the tree — nothing else references
//! `agent-homes/`, so a user reclaiming that space would lose them for good.

use std::path::{Path, PathBuf};

/// Name of the per-legacy-home manifest recording where its rollouts were
/// imported and which ones already made it. Lives in the legacy home
/// (lazybox-owned) rather than the shared one, so the user's `~/.codex`
/// gains nothing but the rollouts themselves.
const MANIFEST: &str = ".lazybox-shared-home";

/// Import every legacy per-workspace Codex home's conversation rollouts
/// into the shared machine home. Best-effort and idempotent: a rollout
/// already recorded in a home's manifest is never re-imported, so a
/// conversation the user later deletes from the shared home stays deleted.
///
/// Blocking IO — call from [`tokio::task::spawn_blocking`], never straight
/// off a runtime worker.
pub(crate) fn migrate_legacy_codex_homes() {
    let Some(shared) = shared_codex_home() else {
        tracing::warn!(
            "codex-home migration: cannot resolve the shared Codex home \
             (CODEX_HOME is relative, or unset with no HOME) — skipping. \
             Legacy per-workspace conversations stay where they are."
        );
        return;
    };
    let root = lazybox_core::paths::agent_homes_root().join("codex");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        // No legacy homes at all is the steady state, not a problem.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(dir = %root.display(), %error, "codex-home migration: cannot list legacy homes");
            return;
        }
    };
    let mut imported = 0usize;
    let mut homes = 0usize;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let count = migrate_one_home(&entry.path(), &shared);
        if count > 0 {
            homes += 1;
            imported += count;
        }
    }
    if imported > 0 {
        tracing::info!(
            rollouts = imported,
            homes,
            shared = %shared.display(),
            "codex-home migration: linked legacy per-workspace conversations into the shared home"
        );
    }
}

/// The shared Codex home: the daemon's own `CODEX_HOME` when set, else
/// `$HOME/.codex`. `None` when the result would not be an absolute path —
/// an unset/empty `HOME`, or a relative `CODEX_HOME`. Both cases would
/// otherwise resolve against the daemon's working directory and write a
/// stray `.codex/` tree somewhere the user will never find it, while Codex
/// itself resolves the same relative path against the worktree it runs in.
pub(crate) fn shared_codex_home() -> Option<PathBuf> {
    let dir = match std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        Some(codex_home) => PathBuf::from(codex_home),
        None => {
            let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
            PathBuf::from(home).join(".codex")
        }
    };
    dir.is_absolute().then_some(dir)
}

/// Import one legacy home's rollouts. Returns how many were newly linked.
///
/// Idempotency is **per rollout**, recorded in the home's manifest as each
/// one lands. Two consequences the coarser "did this home run yet?" marker
/// this replaces got wrong: a transient failure on one rollout no longer
/// discards the record of the ones that succeeded (which re-ran the whole
/// import on the next start, resurrecting whatever the user deleted in
/// between), and a rollout written after the migration first ran — by an
/// old-build Codex process still live in that home — is picked up on the
/// next start instead of being stranded forever.
fn migrate_one_home(home: &Path, shared: &PathBuf) -> usize {
    if home == shared {
        return 0;
    }
    let manifest_path = home.join(MANIFEST);
    let mut manifest = Manifest::load(&manifest_path, shared);
    let mut newly_imported = 0usize;
    for directory in ["sessions", "archived_sessions"] {
        import_rollouts(
            &home.join(directory),
            &shared.join(directory),
            Path::new(directory),
            &mut manifest,
            &mut newly_imported,
        );
    }
    if newly_imported > 0
        && let Err(error) = manifest.save(&manifest_path)
    {
        // The rollouts are linked; only the record of it failed. Log loudly:
        // until this write succeeds the next start re-links them, which is
        // harmless in itself but would resurrect any the user deletes.
        tracing::warn!(
            file = %manifest_path.display(),
            %error,
            "codex-home migration: imported rollouts but could not record them"
        );
    }
    newly_imported
}

/// Walk `source` for rollout JSONL and link each one not already recorded.
/// Per-file failures are logged and skipped, never aborting the walk: one
/// unreadable rollout must not strand every later one in the same home.
fn import_rollouts(
    source: &Path,
    dest: &Path,
    relative: &Path,
    manifest: &mut Manifest,
    newly_imported: &mut usize,
) {
    if source == dest {
        return;
    }
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(dir = %source.display(), %error, "codex-home migration: cannot list");
            return;
        }
    };
    for entry in entries.flatten() {
        // `file_type` from `read_dir` does not follow symlinks and neither
        // branch below matches one, so a symlinked directory cannot send
        // this walk into a loop or out of the legacy home.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let child_relative = relative.join(&name);
        if kind.is_dir() {
            import_rollouts(
                &entry.path(),
                &dest.join(&name),
                &child_relative,
                manifest,
                newly_imported,
            );
        } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            if manifest.contains(&child_relative) {
                continue;
            }
            match link_rollout(&entry.path(), &dest.join(&name)) {
                Ok(()) => {
                    manifest.record(child_relative);
                    *newly_imported += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        file = %entry.path().display(),
                        %error,
                        "codex-home migration: could not import rollout"
                    );
                }
            }
        }
    }
}

/// Link one rollout into the shared home, never replacing what is there.
///
/// A hard link is preferred: it costs nothing, stays live so an old Codex
/// process still appending to the legacy name keeps the shared name in
/// sync, and survives the legacy home being deleted. Only a link that
/// cannot exist (a different filesystem, or one without hard links) falls
/// back to a copy — and that copy is written to a temp name and renamed
/// into place, so a crash or a partial write can never leave a torn
/// rollout under the real name. The copy is also truncated to the last
/// complete line: the source may have a live writer mid-append, and a
/// JSONL file ending in half a record is one Codex may refuse to parse.
fn link_rollout(source: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::hard_link(source, dest) {
        Ok(()) => return Ok(()),
        // Already imported under a manifest we lost, or created natively.
        // Either way the shared home's copy wins.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(_) => {}
    }
    copy_complete_records(source, dest)
}

/// The copy fallback, split out so it can be exercised directly: on one
/// filesystem a hard link always wins, so this path would otherwise only
/// ever run on a machine nobody tests on.
///
/// Writes through a temp name and renames into place, so a crash or a
/// partial write can never leave a torn rollout under the real name. The
/// content is truncated to the last complete line as well: the source may
/// have a live writer mid-append, and a JSONL file ending in half a record
/// is one Codex may refuse to parse. A rollout with no complete record yet
/// has nothing worth importing.
fn copy_complete_records(source: &Path, dest: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(source)?;
    let complete = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(last_newline) => &bytes[..=last_newline],
        None => return Ok(()),
    };
    let temp = dest.with_extension("jsonl.lazybox-import");
    // A previous interrupted run may have left the temp behind.
    let _ = std::fs::remove_file(&temp);
    if let Err(error) = write_private(&temp, complete) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if dest.exists() {
        let _ = std::fs::remove_file(&temp);
        return Ok(());
    }
    if let Err(error) = std::fs::rename(&temp, dest) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Write `bytes` to `path`, owner-only on unix — a rollout carries the
/// conversation, so it must not land more readable than Codex writes it.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A legacy home's import record: the shared home it was imported into,
/// plus the rollouts that already landed there.
struct Manifest {
    destination: String,
    imported: std::collections::BTreeSet<String>,
}

impl Manifest {
    /// Read the manifest, or start an empty one. A manifest naming a
    /// *different* shared home is discarded rather than trusted: pointing
    /// `CODEX_HOME` somewhere new is a legitimate reason to import the same
    /// history again, and reusing the old record would silently skip it.
    fn load(path: &Path, shared: &Path) -> Self {
        let destination = shared.to_string_lossy().into_owned();
        let mut imported = std::collections::BTreeSet::new();
        if let Ok(contents) = std::fs::read_to_string(path) {
            let mut lines = contents.lines();
            if lines.next() == Some(destination.as_str()) {
                imported.extend(lines.filter(|line| !line.is_empty()).map(str::to_string));
            }
        }
        Self {
            destination,
            imported,
        }
    }

    fn contains(&self, relative: &Path) -> bool {
        self.imported.contains(relative.to_string_lossy().as_ref())
    }

    fn record(&mut self, relative: PathBuf) {
        self.imported
            .insert(relative.to_string_lossy().into_owned());
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut contents = self.destination.clone();
        for entry in &self.imported {
            contents.push('\n');
            contents.push_str(entry);
        }
        contents.push('\n');
        std::fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HOME`/`CODEX_HOME` are process-global; serialize the tests that
    /// swap them so they cannot observe each other's values.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    const ROLLOUT: &str = "sessions/2026/09/10/rollout-conversation.jsonl";

    #[test]
    fn imports_rollouts_but_never_credentials_config_or_databases() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("legacy");
        let shared = base.path().join("shared");
        write(&home.join(ROLLOUT), b"{\"conversation\":1}\n");
        write(&home.join("auth.json"), b"old login");
        write(&home.join("config.toml"), b"model = 'gpt'");
        write(&home.join("sessions/state.sqlite"), b"old database");
        write(&home.join("history.jsonl"), b"cross-workspace history");

        assert_eq!(migrate_one_home(&home, &shared), 1);

        assert_eq!(
            std::fs::read(shared.join(ROLLOUT)).unwrap(),
            b"{\"conversation\":1}\n"
        );
        // The whole point of dropping isolation is that the shared login is
        // authoritative — importing a forked credential would undo it. The
        // migration walks only `sessions`/`archived_sessions`, so a
        // root-level file has no path into the shared home at all.
        assert!(!shared.join("auth.json").exists());
        assert!(!shared.join("config.toml").exists());
        assert!(!shared.join("history.jsonl").exists());
        // ...and inside `sessions`, only `.jsonl` rollouts qualify.
        assert!(!shared.join("sessions/state.sqlite").exists());
    }

    #[test]
    fn a_live_writers_appends_stay_visible_through_the_hard_link() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("legacy");
        let shared = base.path().join("shared");
        write(&home.join(ROLLOUT), b"{\"turn\":1}\n");
        migrate_one_home(&home, &shared);

        // An old-build Codex still running in the legacy home appends.
        std::fs::write(home.join(ROLLOUT), b"{\"turn\":1}\n{\"turn\":2}\n").unwrap();
        assert_eq!(
            std::fs::read(shared.join(ROLLOUT)).unwrap(),
            b"{\"turn\":1}\n{\"turn\":2}\n"
        );
    }

    #[test]
    fn never_overwrites_a_rollout_the_shared_home_already_has() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("legacy");
        let shared = base.path().join("shared");
        write(&home.join(ROLLOUT), b"{\"legacy\":1}\n");
        write(&shared.join(ROLLOUT), b"{\"native\":1}\n");

        migrate_one_home(&home, &shared);

        assert_eq!(
            std::fs::read(shared.join(ROLLOUT)).unwrap(),
            b"{\"native\":1}\n"
        );
        assert_eq!(
            std::fs::read(home.join(ROLLOUT)).unwrap(),
            b"{\"legacy\":1}\n"
        );
    }

    #[test]
    fn a_conversation_deleted_from_the_shared_home_is_not_resurrected() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("legacy");
        let shared = base.path().join("shared");
        write(&home.join(ROLLOUT), b"{\"turn\":1}\n");
        write(
            &home.join("archived_sessions/rollout-archived.jsonl"),
            b"{\"archived\":1}\n",
        );

        assert_eq!(migrate_one_home(&home, &shared), 2);
        assert!(
            shared
                .join("archived_sessions/rollout-archived.jsonl")
                .exists()
        );

        // The user deletes one from the shared home; a later start must
        // leave it deleted.
        std::fs::remove_file(shared.join(ROLLOUT)).unwrap();
        assert_eq!(migrate_one_home(&home, &shared), 0);
        assert!(!shared.join(ROLLOUT).exists());
    }

    /// The regression the coarse "did this home run yet?" marker had: it was
    /// written only after every rollout in the home succeeded, so one
    /// failure discarded the record for all of them and the next start
    /// re-imported the lot — resurrecting anything deleted in between.
    #[test]
    fn one_unimportable_rollout_does_not_strand_or_re_import_its_siblings() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("legacy");
        let shared = base.path().join("shared");
        write(&home.join("sessions/a/rollout-a.jsonl"), b"{\"a\":1}\n");
        write(&home.join("sessions/b/rollout-b.jsonl"), b"{\"b\":1}\n");
        // Block one destination by parking a regular FILE where its parent
        // directory needs to be: neither the link nor the copy can create it.
        std::fs::create_dir_all(shared.join("sessions")).unwrap();
        std::fs::write(shared.join("sessions/a"), b"in the way").unwrap();

        assert_eq!(migrate_one_home(&home, &shared), 1);
        assert_eq!(
            std::fs::read(shared.join("sessions/b/rollout-b.jsonl")).unwrap(),
            b"{\"b\":1}\n"
        );

        // The sibling that landed is recorded, so deleting it stays deleted
        // even though the failing one is retried on every later start.
        std::fs::remove_file(shared.join("sessions/b/rollout-b.jsonl")).unwrap();
        assert_eq!(migrate_one_home(&home, &shared), 0);
        assert!(!shared.join("sessions/b/rollout-b.jsonl").exists());
    }

    /// A rollout written into the legacy home *after* the migration first
    /// ran — the old-build process the release notes tell users to restart —
    /// must still be picked up.
    #[test]
    fn a_rollout_created_after_the_first_pass_is_imported_later() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("legacy");
        let shared = base.path().join("shared");
        write(
            &home.join("sessions/rollout-first.jsonl"),
            b"{\"first\":1}\n",
        );
        assert_eq!(migrate_one_home(&home, &shared), 1);

        write(
            &home.join("sessions/rollout-later.jsonl"),
            b"{\"later\":1}\n",
        );
        assert_eq!(migrate_one_home(&home, &shared), 1);
        assert_eq!(
            std::fs::read(shared.join("sessions/rollout-later.jsonl")).unwrap(),
            b"{\"later\":1}\n"
        );
    }

    #[test]
    fn pointing_codex_home_somewhere_new_imports_the_history_again() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("legacy");
        let first = base.path().join("first");
        let second = base.path().join("second");
        write(&home.join(ROLLOUT), b"{\"turn\":1}\n");

        assert_eq!(migrate_one_home(&home, &first), 1);
        assert_eq!(migrate_one_home(&home, &second), 1);
        assert!(second.join(ROLLOUT).exists());
    }

    /// The copy fallback runs when a hard link cannot exist (a different
    /// filesystem). Its source may have a live writer mid-append, so what
    /// lands must never end in half a JSON record.
    #[test]
    fn the_copy_fallback_truncates_a_torn_source_to_whole_records() {
        let base = tempfile::tempdir().unwrap();
        let source = base.path().join("rollout.jsonl");
        let dest = base.path().join("out/rollout.jsonl");
        std::fs::write(&source, b"{\"turn\":1}\n{\"turn\":2}\n{\"tur").unwrap();
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();

        copy_complete_records(&source, &dest).unwrap();

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"{\"turn\":1}\n{\"turn\":2}\n",
            "the half-written trailing record must not be imported"
        );
        // No temp file is left behind under the rollout directory.
        assert_eq!(
            std::fs::read_dir(dest.parent().unwrap())
                .unwrap()
                .flatten()
                .count(),
            1
        );
    }

    #[test]
    fn the_copy_fallback_writes_nothing_when_no_record_is_complete_yet() {
        let base = tempfile::tempdir().unwrap();
        let source = base.path().join("partial.jsonl");
        let dest = base.path().join("out/partial.jsonl");
        std::fs::write(&source, b"{\"half").unwrap();
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();

        copy_complete_records(&source, &dest).unwrap();

        assert!(
            !dest.exists(),
            "importing half a record would hand Codex an unparseable rollout"
        );
    }

    #[test]
    fn the_copy_fallback_never_replaces_an_existing_rollout() {
        let base = tempfile::tempdir().unwrap();
        let source = base.path().join("rollout.jsonl");
        let dest = base.path().join("out/rollout.jsonl");
        std::fs::write(&source, b"{\"legacy\":1}\n").unwrap();
        write(&dest, b"{\"native\":1}\n");

        copy_complete_records(&source, &dest).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"{\"native\":1}\n");
        assert_eq!(
            std::fs::read_dir(dest.parent().unwrap())
                .unwrap()
                .flatten()
                .count(),
            1,
            "the temp copy is cleaned up when the destination already exists"
        );
    }

    #[test]
    fn a_home_that_is_the_shared_home_is_left_untouched() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("same");
        write(&home.join(ROLLOUT), b"{\"turn\":1}\n");
        assert_eq!(migrate_one_home(&home, &home), 0);
        assert!(!home.join(MANIFEST).exists());
    }

    #[test]
    fn a_missing_legacy_home_is_not_an_error() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(
            migrate_one_home(&base.path().join("absent"), &base.path().join("shared")),
            0
        );
    }

    #[test]
    fn the_shared_home_comes_from_codex_home_then_home() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _codex = EnvGuard::set("CODEX_HOME", "/tmp/explicit-codex");
        assert_eq!(
            shared_codex_home(),
            Some(PathBuf::from("/tmp/explicit-codex"))
        );
        drop(_codex);

        let _unset = EnvGuard::unset("CODEX_HOME");
        let _home = EnvGuard::set("HOME", "/tmp/some-home");
        assert_eq!(
            shared_codex_home(),
            Some(PathBuf::from("/tmp/some-home/.codex"))
        );
    }

    /// Neither an unset `HOME` nor a relative `CODEX_HOME` may resolve
    /// against the daemon's working directory: that would hard-link the
    /// user's conversations into a stray `.codex/` wherever the daemon
    /// happens to have been started, while Codex itself resolves the same
    /// relative path against the worktree it runs in.
    #[test]
    fn a_home_that_would_not_be_absolute_is_refused() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _codex = EnvGuard::unset("CODEX_HOME");

        let _empty = EnvGuard::set("HOME", "");
        assert_eq!(shared_codex_home(), None);
        drop(_empty);

        let _no_home = EnvGuard::unset("HOME");
        assert_eq!(shared_codex_home(), None);
        drop(_no_home);

        let _relative = EnvGuard::set("CODEX_HOME", "relative/.codex");
        assert_eq!(shared_codex_home(), None);
    }

    /// The migration must sweep every legacy home under `agent-homes/codex`,
    /// not only the workspace that happens to spawn next — a workspace the
    /// user never reopens holds the only copy of its conversations.
    #[test]
    fn every_legacy_home_is_swept_not_just_one() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = tempfile::tempdir().unwrap();
        let shared = base.path().join("shared-home");
        let _lazybox = EnvGuard::set("LAZYBOX_HOME", base.path().to_str().unwrap());
        let _codex = EnvGuard::set("CODEX_HOME", shared.to_str().unwrap());

        let root = lazybox_core::paths::agent_homes_root().join("codex");
        let workspaces = ["github-acme-widget-1", "github-acme-widget-2", "scratch"];
        for workspace in workspaces {
            write(
                &root
                    .join(workspace)
                    .join("sessions")
                    .join(format!("rollout-{workspace}.jsonl")),
                format!("{{\"from\":\"{workspace}\"}}\n").as_bytes(),
            );
        }
        // A stray file beside the homes must not derail the sweep.
        std::fs::write(root.join("not-a-home"), b"x").unwrap();

        migrate_legacy_codex_homes();

        // EVERY workspace's conversation reached the shared home — the whole
        // point of sweeping at startup instead of on the next spawn, since a
        // workspace the user never reopens holds the only copy of its own.
        for workspace in workspaces {
            assert_eq!(
                std::fs::read(
                    shared
                        .join("sessions")
                        .join(format!("rollout-{workspace}.jsonl"))
                )
                .unwrap(),
                format!("{{\"from\":\"{workspace}\"}}\n").as_bytes(),
                "{workspace}'s conversation never reached the shared home"
            );
            assert!(
                root.join(workspace).join(MANIFEST).exists(),
                "{workspace} was never swept"
            );
        }
        assert_eq!(
            std::fs::read_dir(shared.join("sessions"))
                .unwrap()
                .flatten()
                .count(),
            workspaces.len()
        );
    }

    #[test]
    fn no_legacy_homes_at_all_is_a_silent_no_op() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = tempfile::tempdir().unwrap();
        let _lazybox = EnvGuard::set("LAZYBOX_HOME", base.path().to_str().unwrap());
        let _codex = EnvGuard::set("CODEX_HOME", base.path().join("shared").to_str().unwrap());
        migrate_legacy_codex_homes();
        assert!(!base.path().join("shared").exists());
    }
}
