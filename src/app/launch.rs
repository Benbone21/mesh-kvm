//! Starting mesh-kvm's own pieces: the tray and the window run as separate processes of this exe.

use std::process::{Command, Stdio};

/// Starts this exe again with `arg` (such as `--app` or `--tray`) and does not wait for it.
pub fn spawn_self(arg: &str) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}
