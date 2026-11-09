//! Running another program for a limited time, for the update helper and the link log's looks at
//! the network. Its output is read on threads of their own, so a full pipe can't hold it up.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub enum Ran {
    Finished(Output),
    TimedOut,
    CouldNotStart(String),
}

/// What happens to a program still running when time is up.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OnExpiry {
    /// It's left to finish: a root helper halfway through `apt` can't be stopped from here and
    /// shouldn't be. Only the wait ends. Only the Linux updater has one.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Leave,
    Kill,
}

const POLL: Duration = Duration::from_millis(10);

/// Runs `command` for at most `limit`, with nothing on its input.
pub fn run_limited(mut command: Command, limit: Duration, on_expiry: OnExpiry) -> Ran {
    let deadline = Instant::now() + limit;
    let spawned = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Ran::CouldNotStart(e.to_string()),
    };
    let stdout = reading(child.stdout.take());
    let stderr = reading(child.stderr.take());
    let status = match wait_until(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            if on_expiry == OnExpiry::Kill {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Ran::TimedOut;
        }
        Err(e) => return Ran::CouldNotStart(e.to_string()),
    };
    // Something it started may still hold its output open, and that counts against the limit too.
    let left = |now: Instant| deadline.saturating_duration_since(now);
    let Ok(stdout) = stdout.recv_timeout(left(Instant::now())) else { return Ran::TimedOut };
    let Ok(stderr) = stderr.recv_timeout(left(Instant::now())) else { return Ran::TimedOut };
    Ran::Finished(Output { status, stdout, stderr })
}

fn reading(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (done, read) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = done.send(bytes);
    });
    read
}

fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(POLL.min(deadline - now));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn a_program_within_the_limit_gives_its_output_and_status() {
        let Ran::Finished(output) = run_limited(shell("echo done; echo said >&2; exit 3"), Duration::from_secs(5), OnExpiry::Kill) else {
            panic!("it finished")
        };
        assert_eq!((output.stdout.as_slice(), output.stderr.as_slice()), (&b"done\n"[..], &b"said\n"[..]));
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn a_program_that_outlasts_the_limit_is_killed_when_asked() {
        let marker = std::env::temp_dir().join(format!("mesh-kvm-limited-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let started = Instant::now();
        let script = format!("sleep 1; touch '{}'", marker.display());
        assert!(matches!(run_limited(shell(&script), Duration::from_millis(100), OnExpiry::Kill), Ran::TimedOut));
        assert!(started.elapsed() < Duration::from_millis(900));
        std::thread::sleep(Duration::from_millis(1300));
        assert!(!marker.exists(), "it never got to finish");
    }

    #[test]
    fn a_program_left_to_finish_still_ends_the_wait() {
        let started = Instant::now();
        assert!(matches!(run_limited(shell("sleep 30"), Duration::from_millis(100), OnExpiry::Leave), Ran::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_program_that_cannot_start_says_why() {
        let ran = run_limited(Command::new("/nonexistent/mesh-kvm-nothing"), Duration::from_secs(1), OnExpiry::Kill);
        assert!(matches!(ran, Ran::CouldNotStart(_)));
    }
}
