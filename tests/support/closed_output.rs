//! Connected descriptors whose peers are already closed deterministically fail writes.
//! Unlike removing stdout/stderr, valid descriptors cannot be reopened as /dev/null.
use std::{os::{fd::OwnedFd, unix::net::UnixStream}, process::Command};

pub fn disconnect_output_peers(command: &mut Command) {
    let (stdout, stdout_peer) = UnixStream::pair().unwrap();
    let (stderr, stderr_peer) = UnixStream::pair().unwrap();
    drop(stdout_peer); drop(stderr_peer);
    let stdout: OwnedFd = stdout.into();
    let stderr: OwnedFd = stderr.into();
    command.stdout(stdout).stderr(stderr);
}
