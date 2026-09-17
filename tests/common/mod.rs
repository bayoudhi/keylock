#![allow(dead_code)]

use keylock::pty;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_keylock");
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Shell snippet that reads `n` raw bytes and prints them once all have arrived,
/// so keylock's own title updates can't land in the middle of the match.
pub fn recorder(n: usize) -> String {
    format!("stty raw -echo; echo READY; x=$(head -c {n}); printf 'GOT[%s]' \"$x\"")
}

/// Runs a `keylock` client command against the sessions in `dir`.
pub fn keylock(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("XDG_RUNTIME_DIR", dir)
        .output()
        .unwrap()
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Polls `keylock status` until the session reports `state` (`locked` or `unlocked`).
pub fn wait_for_state(dir: &Path, name: &str, state: &str) {
    let start = Instant::now();
    loop {
        let out = stdout(&keylock(dir, &["status", name]));
        if out.starts_with(&format!("{state} ")) {
            return;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "{name} never became {state}: {out:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// keylock running inside a pty, the way it runs inside a real terminal.
pub struct Session {
    master: File,
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Session {
    pub fn start(dir: &Path, args: &[&str]) -> Session {
        let mut argv = vec![BIN.to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let runtime_dir = dir.to_str().unwrap();
        let pty::Spawned { master, child } = pty::spawn(
            &argv,
            &[("XDG_RUNTIME_DIR", runtime_dir)],
            &pty::winsize(24, 80),
        )
        .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let mut reader = master.try_clone().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Session {
            master,
            child,
            output,
        }
    }

    pub fn type_bytes(&self, bytes: &[u8]) {
        (&self.master).write_all(bytes).unwrap();
    }

    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    pub fn wait_for(&self, needle: &str) {
        let start = Instant::now();
        while !self.output().contains(needle) {
            assert!(
                start.elapsed() < TIMEOUT,
                "timed out waiting for {needle:?}; output: {:?}",
                self.output()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        pty::set_window_size(self.master.as_raw_fd(), &pty::winsize(rows, cols)).unwrap();
    }

    pub fn signal(&self, signal: libc::c_int) {
        // SAFETY: sending a signal to our own child process.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, signal) },
            0
        );
    }

    pub fn wait_exit(mut self) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "keylock did not exit; output: {:?}",
                self.output()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
