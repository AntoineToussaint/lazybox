//! Programs the host starts and does not wait for in the command that started
//! them (a notification sound, a terminal that keeps running). Every one is
//! still waited for, on a thread of its own, so none is left a zombie.
use std::{
    io::{self, Read},
    process::{Child, ExitStatus},
    thread::JoinHandle,
};

/// Waits for `child` on a detached thread; the handle says how it ended.
pub fn reap(mut child: Child) -> io::Result<JoinHandle<io::Result<ExitStatus>>> {
    std::thread::Builder::new()
        .name("lazybox-child".into())
        .spawn(move || child.wait())
}

/// How much of a child's output `drain` keeps: enough for its first lines.
pub const KEPT: usize = 4096;

/// Reads `stream` (a child's stderr pipe) to its end on a detached thread,
/// keeping its first `KEPT` bytes and discarding the rest, so the child can
/// write for as long as it runs without filling the pipe, meeting it closed
/// (SIGPIPE) or filling a disk. The handle gives what was kept.
pub fn drain(mut stream: impl Read + Send + 'static) -> io::Result<JoinHandle<Vec<u8>>> {
    std::thread::Builder::new()
        .name("lazybox-child-output".into())
        .spawn(move || {
            let mut kept = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = KEPT.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            kept
        })
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

    /// Output is read to its end, whatever its size, and only its start kept.
    #[test]
    fn drained_output_is_read_to_the_end_and_only_its_start_kept() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "head -c 1000000 /dev/zero | tr '\\0' 'x' >&2"])
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let kept = drain(child.stderr.take().expect("stderr"))
            .expect("thread")
            .join()
            .expect("join");
        assert_eq!(kept.len(), KEPT);
        assert!(kept.iter().all(|b| *b == b'x'));
        // A megabyte through a pipe: the writer was never blocked or cut off.
        assert!(child.wait().expect("wait").success());
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
