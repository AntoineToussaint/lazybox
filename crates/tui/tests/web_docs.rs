//! Contracts for the hand-maintained parts of lazybox.ai.
//!
//! Keybindings have their own runtime-backed generator. These checks cover
//! the remaining seams that previously let a release add a CLI command or
//! config section while the website stayed silently stale.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: impl AsRef<Path>) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read website source {}: {error}", path.display()))
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

    let agent_guide = read("web/src/content/docs/docs/how-to/run-an-agent-per-workspace.md");
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
        page.matches("w w").count() >= 5,
        "homepage barely teaches `w w`"
    );
    assert!(page.contains("Automation you can see"));
    assert!(page.contains("Terminals fail visibly"));
}

#[test]
fn launch_surfaces_use_current_support_and_provider_contracts() {
    for relative in [
        "README.md",
        "CONTRIBUTING.md",
        "SUPPORT.md",
        ".github/ISSUE_TEMPLATE/config.yml",
        ".github/ISSUE_TEMPLATE/feature_request.yml",
    ] {
        let page = read(relative);
        assert!(
            !page.contains("/discussions"),
            "{relative} links to disabled GitHub Discussions"
        );
    }
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
