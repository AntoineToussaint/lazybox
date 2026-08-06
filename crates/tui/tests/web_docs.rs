//! Contracts for the hand-maintained parts of lazybox.ai.
//!
//! Keybindings have their own runtime-backed generator. These checks cover
//! the remaining seams that previously let a release add a CLI command or
//! config section while the website stayed silently stale.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: impl AsRef<Path>) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read website source {}: {error}", path.display()))
}

fn ci_filters() -> std::collections::BTreeMap<String, Vec<String>> {
    let workflow = read(".github/workflows/ci.yml");
    let block = workflow
        .split_once("          filters: |\n")
        .map(|(_, block)| block)
        .expect("CI path filters");
    let yaml = block
        .lines()
        .take_while(|line| line.is_empty() || line.starts_with("            "))
        .map(|line| line.strip_prefix("            ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    serde_yaml::from_str(&yaml).expect("parse CI path filters")
}

fn ci_filter_matches(filter: &str, path: &str) -> bool {
    ci_filters()[filter].iter().any(|pattern| {
        pattern.strip_suffix("/**").map_or_else(
            || pattern == path,
            |prefix| path == prefix || path.starts_with(&format!("{prefix}/")),
        )
    })
}

#[test]
fn ci_routes_every_compiled_non_crate_input_through_the_rust_lane() {
    for path in [
        ".zig-checksums",
        "README.md",
        "docs/features/inbox-and-sync.md",
        "docs/slack-setup.md",
        "docs/snippets.md",
        "docs/themes.md",
        "prompts/agent-work.md",
    ] {
        assert!(
            ci_filter_matches("rust", path),
            "{path} is consumed by a Rust build and must select the Rust CI lane"
        );
    }
}

#[test]
fn ci_web_lane_runs_the_rust_backed_documentation_contracts() {
    let workflow = read(".github/workflows/ci.yml");
    for path in [
        ".github/ISSUE_TEMPLATE/config.yml",
        ".github/workflows/release.yml",
        "CONTRIBUTING.md",
        "SUPPORT.md",
        "web/src/content/docs/docs/reference/keybindings.md",
        "web/src/pages/index.astro",
    ] {
        assert!(
            ci_filter_matches("rust", path) || ci_filter_matches("contracts", path),
            "{path} is read by a Rust-backed documentation contract"
        );
    }
    assert!(workflow.contains(
        "cargo nextest run -p lazybox-tui --profile ci --test web_docs --test keymap_docs"
    ));
    assert!(
        workflow
            .matches(
                "if: needs.changes.outputs.contracts == 'true' && \
                 needs.changes.outputs.rust != 'true'",
            )
            .count()
            >= 3,
        "contract setup and execution must be skipped only when the full Rust lane runs"
    );
}

#[test]
fn ci_linux_lane_links_benchmark_targets() {
    let workflow = read(".github/workflows/ci.yml");
    assert!(
        workflow.contains("cargo build --workspace --benches"),
        "the PR lane must link benchmark targets omitted by default nextest selection"
    );
}

#[test]
fn desktop_dogfood_artifact_preserves_the_app_executable() {
    let workflow = read(".github/workflows/ci.yml");
    let manifest = read("apps/desktop/src-tauri/Cargo.toml");
    let binary_name = manifest
        .lines()
        .find_map(|line| {
            line.strip_prefix("name = \"")
                .and_then(|name| name.strip_suffix('"'))
        })
        .expect("desktop package name");
    assert!(workflow.contains(
        "- name: Package executable macOS app\n        working-directory: apps/desktop/src-tauri"
    ));
    assert!(workflow.contains(&format!(
        "test -x target/debug/bundle/macos/lazybox.app/Contents/MacOS/{binary_name}"
    )));
    assert!(workflow.contains(&format!(
        "test -x target/debug/bundle/macos/archive-check/lazybox.app/Contents/MacOS/{binary_name}"
    )));
    assert!(workflow.contains("tar -czf target/debug/bundle/macos/lazybox-macos-dogfood.tar.gz"));
    assert!(workflow.contains(
        "path: apps/desktop/src-tauri/target/debug/bundle/macos/lazybox-macos-dogfood.tar.gz"
    ));
    assert!(!workflow.contains("path: target/debug/bundle/macos/lazybox.app"));
}

#[test]
fn configuration_reference_lists_every_top_level_section() {
    let page = read("web/src/content/docs/docs/reference/configuration.md");
    let value =
        serde_yaml::to_value(lazybox_config::Config::default()).expect("default config serializes");
    let mapping = value.as_mapping().expect("config serializes as a mapping");

    for key in mapping.keys().filter_map(serde_yaml::Value::as_str) {
        let linked_row = format!("| [`{key}`]");
        assert!(
            page.contains(&linked_row),
            "configuration reference top-level table is missing `{key}`"
        );
    }
}

#[test]
fn website_covers_v017_public_contracts() {
    let cli = read("web/src/content/docs/docs/reference/cli.md");
    for expected in [
        "lazybox scan [ROOTS...]",
        "--depth N",
        "--hidden",
        "read-only",
        "x i",
    ] {
        assert!(cli.contains(expected), "CLI reference missing {expected:?}");
    }

    let config = read("web/src/content/docs/docs/reference/configuration.md");
    for expected in [
        "scan.roots",
        "max_depth",
        "agent_dead_on_arrival_ms",
        "terminal_new_layout",
        "manage_policies",
    ] {
        assert!(
            config.contains(expected),
            "configuration reference missing {expected:?}"
        );
    }
    for expected in [
        "spawn_agent.aider: \"a z\"",
        "command: aider",
        "resume_args: [--resume]",
        "custom agents\nhave no implicit chord",
    ] {
        assert!(
            config.contains(expected),
            "custom-agent configuration is incomplete: missing {expected:?}"
        );
    }

    let agent_guide = read("web/src/content/docs/docs/how-to/run-an-agent-per-workspace.md");
    for expected in [
        "Add another agent CLI",
        "aider --model sonnet",
        "setup.default_agent",
        "without a shell",
    ] {
        assert!(
            agent_guide.contains(expected),
            "custom-agent workflow missing {expected:?}"
        );
    }

    for expected in ["]]r", "]]t", "exit code", "failed-to-start"] {
        assert!(
            agent_guide.contains(expected),
            "terminal workflow guide missing {expected:?}"
        );
    }

    let policy_guide = read("web/src/content/docs/docs/how-to/manage-automation-policies.md");
    for expected in [
        "g p",
        "merge on green",
        "GitHub auto-merge",
        "auto-fix CI",
        "auto-fix conflict",
    ] {
        assert!(
            policy_guide.contains(expected),
            "automation guide missing {expected:?}"
        );
    }

    let sidebar = read("web/astro.config.mjs");
    assert!(
        sidebar.contains("docs/how-to/manage-automation-policies"),
        "automation guide is not linked from the docs sidebar"
    );

    let architecture = read("web/src/content/docs/docs/explanation/architecture.md");
    for expected in ["wire fingerprint", "bounded", "exit code"] {
        assert!(
            architecture.contains(expected),
            "architecture page missing {expected:?}"
        );
    }
}

#[test]
fn homepage_never_advertises_the_removed_single_w_action() {
    let page = read("web/src/pages/index.astro");
    assert!(
        !page.contains("<kbd>w</kbd>"),
        "homepage still advertises timed single-w work; use deterministic `w w`"
    );
    assert!(
        page.matches("w w").count() >= 3,
        "homepage barely teaches `w w`"
    );
}

#[test]
fn homepage_install_prioritizes_prebuilt_releases() {
    let page = read("web/src/pages/index.astro");
    let brew = page
        .find("brew install AntoineToussaint/lazybox/lazybox")
        .expect("homepage is missing the Homebrew install");
    let alternatives = page
        .find("<details class=\"install-more\">")
        .expect("homepage is missing alternate install methods");
    let installer = page
        .find("Installer script <span>Prebuilt</span>")
        .expect("homepage is missing the prebuilt installer");
    let source = page
        .find("Advanced / from source")
        .expect("homepage is missing the advanced source install");

    assert!(
        brew < alternatives,
        "Homebrew must be the primary install method"
    );
    assert!(
        alternatives < installer && installer < source,
        "the prebuilt installer must come before the advanced source build"
    );
    assert!(
        page.contains(
            "cargo install --git https://github.com/AntoineToussaint/lazybox --locked lazybox-tui-boot"
        ) && page.contains("Compiles the current main branch (HEAD) locally.")
            && page.contains("Zig 0.16.0"),
        "the advanced source build must identify HEAD and its toolchain requirements"
    );
}

#[test]
fn flagship_workflows_are_prominent_across_public_discovery_surfaces() {
    let homepage = read("web/src/pages/index.astro");
    // The homepage leads with real terminal recordings instead of CSS
    // mockups; it stays a discovery surface by showing the demo reel and
    // linking into the core-workflows guide, where the flagship stories live.
    assert!(
        homepage.contains(r#"<section id="demos""#),
        "homepage must present the real demo reel"
    );
    assert!(
        homepage.contains("/docs/tutorials/core-workflows/"),
        "homepage must link into the core workflows guide"
    );
    for label in [
        "<b>Snippets</b>",
        "<b>GitHub controls</b>",
        "<b>Transparent worktrees</b>",
        "<b>GitHub-driven work</b>",
    ] {
        assert!(
            homepage.contains(label),
            "homepage demo reel missing {label:?}"
        );
    }

    let core = read("web/src/content/docs/docs/tutorials/core-workflows.md");
    for expected in [
        "Always open in the right folder",
        "stable and accurate enough",
        "Recent order persists",
        "workspace's `]N`",
        "Start on an issue and continue on its PR",
        "same live tmux session",
        "Complete GitHub work inside lazybox",
        "10 repositories and 15 live",
        "allowlisted",
        "Claude Code, Codex, and Cursor",
        "Learn shortcuts as you use them",
        "Claude Code or Codex",
        "keybinding search remains available",
    ] {
        assert!(
            core.contains(expected),
            "core-workflows guide missing {expected:?}"
        );
    }

    let docs_home = read("web/src/content/docs/docs/index.md");
    assert!(
        docs_home.contains("[Core workflows](/docs/tutorials/core-workflows/)"),
        "docs landing page does not lead into the core workflows"
    );
    let sidebar = read("web/astro.config.mjs");
    assert!(
        sidebar.contains("docs/tutorials/core-workflows"),
        "core workflows are not linked from the docs sidebar"
    );

    let snippets = read("web/src/content/docs/docs/how-to/use-snippets.md");
    for expected in [
        "Recent order persists",
        "`]N` badge",
        "confirm-with-preview",
        "hot-reloads",
        "lazybox—not the agent—owns the filesystem write",
        "Claude Code or Codex",
        "still searches your effective keybindings",
    ] {
        assert!(
            snippets.contains(expected),
            "snippet reuse loop missing {expected:?}"
        );
    }
}

#[test]
fn quickstart_finishes_contextual_work_before_sending_review_workflow() {
    let page = read("web/src/content/docs/docs/tutorials/quickstart.md");
    let daily = page
        .split_once("## 4. Your daily fast path")
        .map(|(_, section)| section)
        .expect("quickstart daily-workflow section");
    let wait = daily
        .find("wait for its contextual task to finish")
        .expect("quickstart must wait for the `w w` task");
    let send = daily
        .find("]]srev")
        .expect("quickstart must send the review workflow");
    assert!(
        wait < send,
        "quickstart sends `rev` before the contextual `w w` task finishes"
    );
}

#[test]
fn snippet_guide_describes_the_client_wide_launch_directory_layer() {
    let page = read("web/src/content/docs/docs/how-to/use-snippets.md");
    let normalized = page.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "resolved once when the client starts",
        "shared by all of its workspaces",
        "does not load another workspace",
    ] {
        assert!(
            normalized.contains(expected),
            "snippet scope contract missing {expected:?}"
        );
    }
    assert!(
        !page.contains("without changing the workflow elsewhere"),
        "guide still implies the directory override follows workspace scope"
    );
}

#[test]
fn snippet_surfaces_describe_the_workspace_history_as_bounded() {
    for relative in [
        "README.md",
        "docs/snippets.md",
        "web/src/content/docs/docs/how-to/use-snippets.md",
    ] {
        let page = read(relative);
        assert!(
            page.contains("12") && page.contains("recent"),
            "{relative} must describe the ]N badge as a 12-entry recent history"
        );
    }
}

#[test]
fn issue_to_pr_handoff_is_documented_across_discovery_surfaces() {
    let guide = read("web/src/content/docs/docs/how-to/keep-session-from-issue-to-pr.md");
    for expected in [
        "Closes #42.",
        "no live terminal",
        "x j",
        "same live terminal",
        "worktree and local edits",
        "scrollback",
        "prompt history",
        "activity and read/unread state",
        "without showing the automatic confirmation again",
    ] {
        assert!(
            guide.contains(expected),
            "issue-to-PR workflow guide missing {expected:?}"
        );
    }

    for landing in [
        "web/src/content/docs/docs/index.md",
        "web/src/content/docs/docs/how-to/index.md",
        "web/src/content/docs/docs/tutorials/quickstart.md",
        "web/astro.config.mjs",
    ] {
        assert!(
            read(landing).contains("keep-session-from-issue-to-pr"),
            "{landing} does not surface the issue-to-PR guide"
        );
    }
}

#[test]
fn mention_guides_describe_the_full_sweep_cadence() {
    for relative in [
        "web/src/content/docs/docs/how-to/lazybox-mentions.md",
        "web/src/content/docs/docs/how-to/run-an-agent-per-workspace.md",
    ] {
        let page = read(relative);
        let prose = page.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            prose.contains("full GitHub sweep"),
            "{relative} must name the polling path that scans mentions",
        );
        assert!(
            prose.contains("ten minutes"),
            "{relative} must set the default trigger cadence",
        );
        assert!(
            !prose.contains("next poll"),
            "{relative} must not imply incremental polls scan mentions",
        );
    }
}

#[test]
fn comparison_distinguishes_task_sources_from_the_slack_mirror() {
    let page = read("web/src/content/docs/docs/explanation/comparison.md");
    let prose = page.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        page.contains("✓ GitHub · Linear") && prose.contains("Slack mirrors workspace activity"),
        "comparison must distinguish inbox task sources from the optional Slack mirror"
    );
    assert!(
        !page.contains("GitHub · Linear · Slack")
            && !prose.contains("Slack threads flow into one read/unread event feed"),
        "comparison must not advertise Slack as a read/unread task source"
    );
}

#[test]
fn label_trigger_docs_explain_their_distinct_authorization_boundary() {
    let page = read("web/src/content/docs/docs/how-to/lazybox-mentions.md");
    let prose = page.split_whitespace().collect::<Vec<_>>().join(" ");

    for expected in [
        "`mention.allowed_logins` applies only to `@lazybox` mentions",
        "GitHub does not include who applied a label",
        "anyone with permission to label an eligible issue can trigger the agent",
    ] {
        assert!(
            prose.contains(expected),
            "label authorization contract missing {expected:?}"
        );
    }
}

#[test]
fn label_trigger_discovery_surfaces_state_issue_eligibility() {
    for relative in [
        "web/src/pages/index.astro",
        "web/src/content/docs/docs/explanation/comparison.md",
        "web/src/content/docs/docs/how-to/index.md",
    ] {
        let page = read(relative);
        let prose = page.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            prose.contains("authored or are assigned to"),
            "{relative} must state which issues are eligible for label-triggered agents"
        );
    }
}

#[test]
fn comparison_tracks_documented_remote_and_license_contracts() {
    let page = read("web/src/content/docs/docs/explanation/comparison.md");

    assert!(
        page.contains("✓ Cloud / API (beta)")
            && page.contains("https://www.conductor.build/docs/api"),
        "Conductor's documented cloud execution must be represented and sourced"
    );
    assert!(
        page.contains("AGPL-3.0 client · proprietary service"),
        "Warp's license cell must distinguish its open client from its hosted service"
    );
}

#[test]
fn bulk_label_example_preserves_metadata_and_paginates_the_backlog() {
    let page = read("web/src/content/docs/docs/how-to/lazybox-mentions.md");

    assert!(
        page.contains("gh api --paginate"),
        "bulk example must traverse every page of matching issues"
    );
    assert!(
        page.contains("--color") && !page.contains("gh label create 'lazybox:claude/M'"),
        "bulk example must create a missing label explicitly without force-updating it"
    );
    assert!(
        !page.contains("--force") && !page.contains("--limit 1000"),
        "bulk example must neither mutate existing label metadata nor truncate the backlog"
    );
}

#[test]
fn launch_surfaces_use_current_support_and_provider_contracts() {
    assert!(read(".github/ISSUE_TEMPLATE/question.yml").contains("Question / setup help"));

    let readme = read("README.md");
    assert!(readme.contains("lazybox-tui-installer.sh | sh"));
    let release_config = read("crates/tui-boot/Cargo.toml");
    assert!(release_config.contains("lazybox-tui-installer.sh"));
    let installer_compat = read("crates/tui-boot/lazybox-tui-installer.sh");
    assert!(installer_compat.contains("lazybox-tui-boot-installer.sh"));

    let cli = read("web/src/content/docs/docs/reference/cli.md");
    for expected in ["SLACK_BOT_TOKEN", "SLACK_APP_TOKEN", "Archive"] {
        assert!(cli.contains(expected), "CLI reference missing {expected:?}");
    }

    let config = read("web/src/content/docs/docs/reference/configuration.md");
    for expected in [
        "`name`",
        "`command`",
        "`args`",
        "`resume_args`",
        "`asking_patterns`",
        "`shell`",
    ] {
        assert!(
            config.contains(expected),
            "configuration reference missing {expected:?}"
        );
    }

    let mentions = read("web/src/content/docs/docs/how-to/lazybox-mentions.md");
    for expected in ["@lazybox claude S", "lazybox:claude/S", "Unknown agent ids"] {
        assert!(
            mentions.contains(expected),
            "mentions guide missing {expected:?}"
        );
    }
    assert!(!mentions.contains("hardcoded"));
    assert!(config.contains("when an agent needs input or finishes"));
}

#[test]
fn release_surfaces_use_public_product_metadata_and_curated_notes() {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["metadata", "--no-deps", "--format-version=1"])
        .current_dir(repo_root())
        .output()
        .expect("read Cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse Cargo metadata");
    let package = metadata["packages"]
        .as_array()
        .and_then(|packages| {
            packages
                .iter()
                .find(|package| package["name"] == "lazybox-tui-boot")
        })
        .expect("find release package");
    assert_eq!(
        package["description"],
        "A reactive PR inbox and agent workspace manager for the terminal."
    );
    assert_eq!(package["metadata"]["dist"]["display-name"], "lazybox");

    let workflow = read(".github/workflows/release.yml");
    assert!(workflow.contains(
        "ANNOUNCEMENT_BODY: \"${{ fromJson(steps.host.outputs.manifest).announcement_changelog }}\""
    ));
    assert!(!workflow.contains(
        "ANNOUNCEMENT_BODY: \"${{ fromJson(steps.host.outputs.manifest).announcement_github_body }}\""
    ));
}

#[cfg(unix)]
#[test]
fn compatibility_installer_delegates_to_the_same_release() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("create installer test directory");
    let bin_dir = temp.path().join("bin");
    std::fs::create_dir(&bin_dir).expect("create fake binary directory");
    let captured_url = temp.path().join("url");
    let fake_curl = bin_dir.join("curl");
    std::fs::write(
        &fake_curl,
        r#"#!/bin/sh
set -eu
url=
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        https://*) url="$1" ;;
        --output) shift; output="$1" ;;
    esac
    shift
done
printf '%s\n' "$url" > "$CAPTURE_URL"
printf '%s\n' 'exit 0' > "$output"
"#,
    )
    .expect("write fake curl");
    let mut permissions = std::fs::metadata(&fake_curl)
        .expect("read fake curl metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_curl, permissions).expect("make fake curl executable");

    let path = std::env::join_paths(std::iter::once(bin_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .expect("compose test PATH");
    let output = Command::new("sh")
        .arg(repo_root().join("crates/tui-boot/lazybox-tui-installer.sh"))
        .env("PATH", path)
        .env("CAPTURE_URL", &captured_url)
        .output()
        .expect("run compatibility installer");
    assert!(
        output.status.success(),
        "compatibility installer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requested_url = std::fs::read_to_string(captured_url).expect("read captured URL");
    assert_eq!(
        requested_url.trim(),
        format!(
            "https://github.com/AntoineToussaint/lazybox/releases/download/v{}/lazybox-tui-boot-installer.sh",
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn changelog_top_release_matches_package_version_and_carries_a_date() {
    // cargo-dist extracts the newest `## [x.y.z] - DATE` section verbatim as the
    // GitHub release notes, so that heading must name the version actually
    // shipping and carry a real date. A version bump that leaves the changelog
    // topping out at the previous release would slip through every other check.
    let changelog = read("CHANGELOG.md");
    let heading = changelog
        .lines()
        .find(|line| line.starts_with("## [") && !line.contains("[Unreleased]"))
        .expect("CHANGELOG.md has a versioned release heading");

    let version = heading
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(version, _)| version)
        .expect("release heading names a version in brackets");
    assert_eq!(
        version,
        env!("CARGO_PKG_VERSION"),
        "newest CHANGELOG heading {heading:?} must match the package version {}; \
         bump the version and add its changelog section together",
        env!("CARGO_PKG_VERSION"),
    );

    let date = heading
        .split_once(" - ")
        .map(|(_, date)| date.trim())
        .expect("release heading carries an ` - <date>` suffix");
    let parts: Vec<&str> = date.split('-').collect();
    assert!(
        parts.len() == 3
            && parts[0].len() == 4
            && parts[1].len() == 2
            && parts[2].len() == 2
            && parts
                .iter()
                .all(|part| part.chars().all(|c| c.is_ascii_digit())),
        "release heading must carry an ISO YYYY-MM-DD date, got {date:?}",
    );
}
