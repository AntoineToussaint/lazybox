# Release-candidate checklist

Run this checklist from the exact commit that will receive the release tag.

## Automated gates

- [ ] `make setup` completes once online and verifies the pinned Zig archive.
- [ ] Disconnect networking and run `make release` successfully.
- [ ] `cargo nextest run --workspace --profile ci` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace` passes.
- [ ] `cargo deny check advisories bans licenses sources` passes.
- [ ] `npm --prefix web ci && npm --prefix web run check && npm --prefix web run build && npm --prefix web run lighthouse` passes.
- [ ] The latest main-branch Criterion run has no performance-regression alert.
- [ ] The release workflow extracts every target archive and runs `--version`,
      `--help`, and a bounded `--test` startup/shutdown smoke test.

## Real-provider checks

Use dedicated test resources and revoke temporary credentials afterward.

- [ ] GitHub: poll PRs/issues, refresh, reply, request review, label, and verify
      one intentionally rejected mutation surfaces a useful error.
- [ ] Linear: poll assigned issues and verify pagination plus rate-limit errors.
- [ ] Slack: run `slack doctor`, mirror one workspace, send in both directions,
      reconnect Socket Mode, and prune the test channel.
- [ ] Start an agent in a newly provisioned worktree and confirm a stale or
      unknown workspace request cannot spawn anywhere else.
- [ ] Restart the daemon with live tmux sessions and confirm replay/recovery.

## Packaging and publication

- [ ] Bump `[workspace.package].version` and move changelog entries out of
      `Unreleased`.
- [ ] Confirm README/docs platform claims match the cargo-dist target matrix.
- [ ] Inspect archive contents, checksums, installer, and Homebrew formula.
- [ ] Install and uninstall through both the shell installer and Homebrew.
- [ ] Confirm `lazybox --version` matches the tag and embedded Git commit.
- [ ] Publish the tag only after the release-plan pull request is green.
- [ ] Keep the previous release artifacts available for rollback.
