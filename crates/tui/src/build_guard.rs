//! Result types for the startup update check.
//!
//! These are UI-facing: the Model stashes an [`AvailableUpdate`] and the
//! update modal renders its `modal_body`, while the sidebar consults
//! [`is_release_build`]. The *fetching* half — git, Homebrew, the
//! GitHub releases API, and the state-DB cache — lives boot-side in
//! `lazybox-tui-boot`'s `build_guard`, so the UI library carries neither
//! `octocrab` nor a client credential path (#548).

use lazybox_ipc::IS_RELEASE_BUILD;

const SHELL_INSTALLER_COMMAND: &str = "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AntoineToussaint/lazybox/releases/latest/download/lazybox-tui-installer.sh | sh";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseInstall {
    Homebrew,
    Shell,
}

impl ReleaseInstall {
    fn update_command(self) -> &'static str {
        match self {
            Self::Homebrew => "brew upgrade lazybox",
            Self::Shell => SHELL_INSTALLER_COMMAND,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvailableUpdate {
    Source {
        current: String,
        available: String,
        commits_behind: u32,
        source_dir: String,
        pull_before_build: bool,
    },
    Release {
        current: String,
        available: String,
        install: ReleaseInstall,
    },
}

impl AvailableUpdate {
    pub(crate) fn target(&self) -> String {
        match self {
            Self::Source { available, .. } => format!("source:{available}"),
            Self::Release { available, .. } => format!("release:{available}"),
        }
    }

    pub(crate) fn modal_body(&self) -> String {
        match self {
            Self::Source {
                current,
                available,
                commits_behind,
                source_dir,
                pull_before_build,
            } => {
                let commits = if *commits_behind == 1 {
                    "commit"
                } else {
                    "commits"
                };
                let source = shell_quote(source_dir);
                let command = if *pull_before_build {
                    format!("cd -- {source} && git pull --ff-only && cargo build --release")
                } else {
                    format!("cd -- {source} && cargo build --release")
                };
                format!(
                    "Your lazybox source is {commits_behind} {commits} ahead of this binary — \
                     {current} → {available}.\n\nUpdate with:\n  {command}\n\n\
                     Lazybox will not run this command or update itself."
                )
            }
            Self::Release {
                current,
                available,
                install,
            } => format!(
                "A newer release is available — {current} → {available}.\n\n\
                 Update with:\n  {}\n\n\
                 Lazybox will not run this command or update itself.",
                install.update_command()
            ),
        }
    }
}

pub fn is_release_build() -> bool {
    IS_RELEASE_BUILD
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_copy_scopes_commands_and_names_the_install_channel() {
        let source = AvailableUpdate::Source {
            current: "abc123".into(),
            available: "def456".into(),
            commits_behind: 2,
            source_dir: "/tmp/lazybox source/it's-here".into(),
            pull_before_build: true,
        }
        .modal_body();
        assert!(source.contains("2 commits ahead of this binary — abc123 → def456"));
        assert!(source.contains(
            "cd -- '/tmp/lazybox source/it'\"'\"'s-here' && git pull --ff-only && cargo build --release"
        ));
        assert!(source.contains("will not run this command"));

        let brew = AvailableUpdate::Release {
            current: "v0.1.7".into(),
            available: "v0.2.0".into(),
            install: ReleaseInstall::Homebrew,
        }
        .modal_body();
        assert!(brew.contains("brew upgrade lazybox"));

        let shell = AvailableUpdate::Release {
            current: "v0.1.7".into(),
            available: "v0.2.0".into(),
            install: ReleaseInstall::Shell,
        }
        .modal_body();
        assert!(shell.contains("lazybox-tui-installer.sh | sh"));
    }
}
