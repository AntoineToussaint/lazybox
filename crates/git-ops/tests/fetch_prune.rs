//! Regression for the `fetch.prune = true` ref loss (issue #404).
//!
//! With that (common) global git config, the single-branch refresh
//! used to run `git fetch origin "+<branch>:refs/remotes/origin/<branch>"`
//! — and on a RE-fetch git's prune pass failed to reverse-map the
//! existing remote-tracking ref onto the short-named command-line
//! refspec, deleted `refs/remotes/origin/<branch>`, and exited 0. When
//! no local `refs/heads/<branch>` existed either (first checkout of a
//! PR branch fetched by an earlier, failed provision), `checkout_at`
//! then errored "branch not found locally or on origin" and the spawn
//! fell back to an empty non-git dir — the entry point of the
//! "issue→PR transfer loses sessions on real paths" failure. CI has no
//! global git config, which is why this only broke on real machines.
//!
//! This file holds a single test: it points `GIT_CONFIG_GLOBAL` at a
//! synthetic global config for the whole process.

use lazybox_git_ops::WorktreeManager;
use std::path::Path;

fn git(cwd: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn checkout_survives_refetch_under_global_fetch_prune() {
    let scratch = tempfile::TempDir::new().unwrap();
    let global = scratch.path().join("gitconfig");
    std::fs::write(
        &global,
        "[fetch]\n\tprune = true\n[user]\n\temail = t@example.com\n\tname = Tester\n[commit]\n\tgpgsign = false\n",
    )
    .unwrap();
    // SAFETY: this is the only test in this binary, so no concurrent
    // test observes the mutation; child git processes must inherit it.
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &global) };

    let upstream = scratch.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-b", "main", "-q"]);
    git(&upstream, &["commit", "--allow-empty", "-m", "first", "-q"]);
    git(&upstream, &["branch", "feat"]);
    let base = scratch.path().join("base");
    let bare = base.join("repos/o/r.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        &upstream,
        &[
            "clone",
            "--bare",
            "-q",
            &upstream.to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );
    // A prior provision attempt left the remote-tracking ref behind
    // WITHOUT a matching local branch — the shape the prune bug needs.
    git(
        &bare,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/heads/feat:refs/remotes/origin/feat",
        ],
    );

    let mgr = WorktreeManager::new(base.clone());
    // The internal branch refresh re-fetches `feat`; under
    // fetch.prune=true the old refspec deleted the ref here and the
    // start-point resolution failed.
    let wt = mgr
        .checkout_at(&base.join("wt/pr"), "o", "r", "feat", None)
        .await
        .expect("checkout of a pre-fetched branch must survive fetch.prune=true");
    assert!(wt.path.join(".git").exists());

    // The base-branch refresh in the new-branch path runs the same
    // fetch; a second provision on the repo must not lose refs either.
    mgr.checkout_new_branch_at(&base.join("wt/issue"), "o", "r", "issue-1", "main")
        .await
        .expect("new-branch checkout must survive fetch.prune=true");
    let refs = std::process::Command::new("git")
        .current_dir(&bare)
        .args(["for-each-ref", "--format=%(refname)", "refs/remotes/origin"])
        .output()
        .unwrap();
    let refs = String::from_utf8_lossy(&refs.stdout);
    assert!(
        refs.contains("refs/remotes/origin/feat") && refs.contains("refs/remotes/origin/main"),
        "remote-tracking refs must survive re-fetches, got:\n{refs}",
    );
}
