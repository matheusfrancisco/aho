//! Test helper for the readiness handshake of exec-in-place reload.
//!
//! * `signal`: takes the readiness fd, prints its pid, writes the ready byte.
//! * `exit`: exits with status 3 without signalling.
//! * `exec-into <path>`: takes the readiness fd, prints its pid, then exec's `<path> signal` in
//!   place (same pid) handing the fd on.

#![forbid(unsafe_code)]

use std::io::Write;
use std::os::fd::AsFd;
use std::process::{Command, ExitCode};

use daemon::ready::{READY_FD_ENV, exec_with_ready, take_ready_fd};

fn print_pid() {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{}", std::process::id());
    let _ = out.flush();
}

#[allow(clippy::disallowed_methods, reason = "authority: daemon-ready-probe")]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("signal") => {
            let Some(fd) = take_ready_fd() else {
                eprintln!("ready_probe: no readiness fd in {READY_FD_ENV}; start it through spawn_with_ready");
                return ExitCode::from(2);
            };
            print_pid();
            match fd.signal() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("ready_probe: could not write the ready byte: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("exit") => ExitCode::from(3),
        Some("exec-into") => {
            let Some(path) = args.get(1) else {
                eprintln!("ready_probe: usage: ready_probe exec-into <path>");
                return ExitCode::from(64);
            };
            let Some(fd) = take_ready_fd() else {
                eprintln!("ready_probe: no readiness fd in {READY_FD_ENV}; start it through spawn_with_ready");
                return ExitCode::from(2);
            };
            print_pid();
            let err = exec_with_ready(Command::new(path).arg("signal"), fd.as_fd());
            eprintln!("ready_probe: could not exec {path}: {err}");
            ExitCode::from(1)
        }
        _ => {
            eprintln!("ready_probe: usage: ready_probe signal | exit | exec-into <path>");
            ExitCode::from(64)
        }
    }
}
