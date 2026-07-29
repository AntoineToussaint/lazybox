//! Process resource-limit hardening for the daemon.
//!
//! A tmux-backed terminal deliberately keeps several descriptors open:
//! a resize handle, reader/writer PTY handles, and a relay wakeup pair.
//! macOS commonly launches terminal applications with `RLIMIT_NOFILE=256`,
//! which leaves no operating headroom once lazybox recovers a few dozen
//! sessions. Raise the soft limit before backend detection so both lazybox
//! and any tmux server it starts inherit a server-appropriate budget.

use std::io;

/// Enough headroom for thousands of terminal conduits plus SQLite, provider
/// subprocesses, IPC clients, and logs. Never lowers a larger inherited limit.
#[cfg(unix)]
const OPEN_FILE_TARGET: libc::rlim_t = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OpenFileLimit {
    pub previous_soft: u64,
    pub soft: u64,
    pub hard: Option<u64>,
}

#[cfg(unix)]
fn desired_soft_limit(
    current: libc::rlim_t,
    hard: libc::rlim_t,
    target: libc::rlim_t,
) -> libc::rlim_t {
    if current >= target {
        return current;
    }
    if hard == libc::RLIM_INFINITY {
        target
    } else {
        target.min(hard).max(current)
    }
}

/// Raise this process's soft open-file limit, bounded by its hard limit.
///
/// Call before constructing the session backend: a newly-created tmux server
/// inherits this limit. Existing tmux servers keep their original limit, but
/// the much larger lazybox-side budget still removes the immediate 5-FD-per-
/// attachment ceiling.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // rlim_t width varies across Unix targets.
pub(crate) fn raise_open_file_limit() -> io::Result<OpenFileLimit> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let previous_soft = limit.rlim_cur;
    let desired = desired_soft_limit(previous_soft, limit.rlim_max, OPEN_FILE_TARGET);
    if desired != previous_soft {
        let raised = libc::rlimit {
            rlim_cur: desired,
            rlim_max: limit.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
            return Err(io::Error::last_os_error());
        }
        limit.rlim_cur = desired;
    }

    Ok(OpenFileLimit {
        previous_soft: previous_soft as u64,
        soft: limit.rlim_cur as u64,
        hard: (limit.rlim_max != libc::RLIM_INFINITY).then_some(limit.rlim_max as u64),
    })
}

#[cfg(not(unix))]
pub(crate) fn raise_open_file_limit() -> io::Result<OpenFileLimit> {
    Ok(OpenFileLimit {
        previous_soft: 0,
        soft: 0,
        hard: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn desired_limit_raises_to_target_without_exceeding_hard_limit() {
        assert_eq!(desired_soft_limit(256, 1_024, 65_536), 1_024);
        assert_eq!(desired_soft_limit(256, libc::RLIM_INFINITY, 65_536), 65_536);
        assert_eq!(desired_soft_limit(100_000, 100_000, 65_536), 100_000);
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::unnecessary_cast)] // rlim_t width varies across Unix targets.
    fn real_process_limit_is_never_lowered() {
        let raised = raise_open_file_limit().expect("raise open-file soft limit");
        assert!(raised.soft >= raised.previous_soft);
        if raised
            .hard
            .is_none_or(|hard| hard >= OPEN_FILE_TARGET as u64)
        {
            assert!(raised.soft >= OPEN_FILE_TARGET as u64);
        }
    }
}
