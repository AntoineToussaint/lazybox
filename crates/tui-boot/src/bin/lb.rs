//! `lb` — small executable alias for the sibling `lazybox` binary.
//!
//! Keeping this as an exec shim avoids shipping a second copy of the full TUI
//! while preserving the convenient command name in Cargo and release archives.

use std::path::{Path, PathBuf};
use std::process::Command;

fn lazybox_sibling(current_exe: &Path) -> PathBuf {
    current_exe.with_file_name("lazybox")
}

#[cfg(unix)]
fn main() {
    use std::os::unix::process::CommandExt;

    let current = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("lb: could not locate this executable: {error}");
        std::process::exit(126);
    });
    let lazybox = lazybox_sibling(&current);
    let error = Command::new(&lazybox)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("lb: could not execute {}: {error}", lazybox.display());
    std::process::exit(if error.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    });
}

#[cfg(not(unix))]
fn main() {
    let current = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("lb: could not locate this executable: {error}");
        std::process::exit(126);
    });
    let lazybox = lazybox_sibling(&current);
    match Command::new(&lazybox)
        .args(std::env::args_os().skip(1))
        .status()
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("lb: could not run {}: {error}", lazybox.display());
            std::process::exit(126);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_alias_next_to_main_binary() {
        assert_eq!(
            lazybox_sibling(Path::new("/tmp/release/lb")),
            Path::new("/tmp/release/lazybox")
        );
    }
}
