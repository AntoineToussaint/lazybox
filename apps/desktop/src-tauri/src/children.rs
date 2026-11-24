//! Programs the host starts and does not wait for in the command that started
//! them (a notification sound, a terminal that keeps running). Every one is
//! still waited for, on a thread of its own, so none is left a zombie.
use std::{
    io,
    process::{Child, ExitStatus},
    thread::JoinHandle,
};

/// Waits for `child` on a detached thread; the handle says how it ended.
pub fn reap(mut child: Child) -> io::Result<JoinHandle<io::Result<ExitStatus>>> {
    std::thread::Builder::new()
        .name("lazybox-child".into())
        .spawn(move || child.wait())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// Whether `pid` is still in the process table (a zombie still is).
    fn listed(pid: u32) -> bool {
        Command::new("/bin/ps")
            .args(["-o", "pid=", "-p", &pid.to_string()])
            .stdout(Stdio::piped())
            .output()
            .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
            .unwrap_or(true)
    }

    #[test]
    fn a_child_nobody_waits_for_is_reaped() {
        let child = Command::new("/usr/bin/true").spawn().expect("spawn");
        let pid = child.id();
        let status = reap(child).expect("thread").join().expect("join");
        assert!(status.expect("wait").success());
        assert!(!listed(pid), "{pid} is no zombie once reaped");
    }
}
