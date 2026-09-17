mod common;

use common::{keylock, recorder, stderr, stdout, wait_for_state, Session, BIN};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_millis(300);

#[test]
fn forwards_input_and_passes_the_exit_code_through() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &[
            "run",
            "--",
            "sh",
            "-c",
            "stty raw -echo; echo \"READY name=$KEYLOCK_NAME\"; head -c 3; exit 7",
        ],
    );
    s.wait_for("READY name=sh");
    s.type_bytes(b"hi!");
    s.wait_for("hi!");
    assert_eq!(s.wait_exit().code(), Some(7));
    assert!(!dir.path().join("keylock/sh.sock").exists());
}

#[test]
fn locked_session_drops_everything_until_unlocked_from_outside() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &["run", "--locked", "--", "sh", "-c", &recorder(5)],
    );
    s.wait_for("READY");
    s.type_bytes(b"abc \x03\x1a\x1c\x04\x1b[A\x1b[<0;3;4M\x1b[200~paste\x1b[201~\r");
    thread::sleep(SETTLE);
    assert!(stdout(&keylock(dir.path(), &["status", "sh"])).starts_with("locked pid="));

    let off = keylock(dir.path(), &["off", "sh"]);
    assert!(off.status.success());
    assert!(stdout(&off).starts_with("unlocked pid="));
    s.type_bytes(b"XYZ12");
    s.wait_for("GOT[XYZ12]");
    assert_eq!(s.wait_exit().code(), Some(0));
}

#[test]
fn hotkey_locks_and_phrase_unlocks() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &["run", "--phrase", "unlock", "--", "sh", "-c", &recorder(5)],
    );
    s.wait_for("READY");
    s.type_bytes(b"\x1dl");
    wait_for_state(dir.path(), "sh", "locked");
    s.type_bytes(b"abc\x03");
    s.type_bytes(b"unlock\r");
    wait_for_state(dir.path(), "sh", "unlocked");
    s.type_bytes(b"XYZ12");
    s.wait_for("GOT[XYZ12]");
    assert_eq!(s.wait_exit().code(), Some(0));
}

#[test]
fn on_from_another_shell_locks() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &["run", "--name", "mig", "--", "sh", "-c", &recorder(5)],
    );
    s.wait_for("READY");
    assert!(stdout(&keylock(dir.path(), &["on", "mig"])).starts_with("locked pid="));
    s.type_bytes(b"abc");
    thread::sleep(SETTLE);
    keylock(dir.path(), &["off", "mig"]);
    s.type_bytes(b"XYZ12");
    s.wait_for("GOT[XYZ12]");
}

#[test]
fn lock_shows_in_the_title_and_typing_rings_the_bell() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &["run", "--locked", "--", "sh", "-c", "echo READY; sleep 30"],
    );
    s.wait_for("\x1b]2;🔒 sh\x07");
    s.wait_for("READY");
    s.type_bytes(b"a");
    s.wait_for("\x07\x1b]2;🔒 locked — keylock off sh\x07");
    keylock(dir.path(), &["off", "sh"]);
    s.wait_for("\x1b]2;sh\x07");
    // Unlocked again: Ctrl+C reaches the command as a real interrupt.
    s.type_bytes(b"\x03");
    assert_eq!(s.wait_exit().code(), Some(130));
}

#[test]
fn ls_suffixes_duplicate_names_and_unknown_names_fail() {
    let dir = tempfile::tempdir().unwrap();
    let a = Session::start(
        dir.path(),
        &[
            "run",
            "--name",
            "mig",
            "--",
            "sh",
            "-c",
            "echo READY; sleep 30",
        ],
    );
    a.wait_for("READY");
    let b = Session::start(
        dir.path(),
        &[
            "run",
            "--name",
            "mig",
            "--",
            "sh",
            "-c",
            "echo READY; sleep 30",
        ],
    );
    b.wait_for("READY");

    let ls = stdout(&keylock(dir.path(), &["ls"]));
    assert!(ls.contains("mig  unlocked  pid="), "{ls}");
    assert!(ls.contains("mig-2  unlocked  pid="), "{ls}");

    let unknown = keylock(dir.path(), &["off", "nope"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("no session named nope"),
        "{}",
        stderr(&unknown)
    );
    assert!(stderr(&unknown).contains("  mig-2"), "{}", stderr(&unknown));
}

#[test]
fn window_size_changes_reach_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &["run", "--", "sh", "-c", "stty raw -echo; echo READY; head -c 1 >/dev/null; echo \"size=$(stty size)\"; head -c 1 >/dev/null"],
    );
    s.wait_for("READY");
    s.resize(40, 120);
    thread::sleep(SETTLE);
    s.type_bytes(b"x");
    s.wait_for("size=40 120");
    s.type_bytes(b"x");
    assert_eq!(s.wait_exit().code(), Some(0));
}

#[test]
fn signals() {
    let dir = tempfile::tempdir().unwrap();
    let killed = Session::start(dir.path(), &["run", "--", "sh", "-c", "kill -TERM $$"]);
    assert_eq!(killed.wait_exit().code(), Some(143));

    let trapped = Session::start(
        dir.path(),
        &[
            "run",
            "--",
            "sh",
            "-c",
            "trap 'echo TRAPPED; exit 3' TERM; echo READY; while :; do sleep 0.1; done",
        ],
    );
    trapped.wait_for("READY");
    trapped.signal(libc::SIGTERM);
    trapped.wait_for("TRAPPED");
    assert_eq!(trapped.wait_exit().code(), Some(3));
}

#[test]
fn runs_until_the_command_exits_even_after_it_lets_go_of_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let s = Session::start(
        dir.path(),
        &[
            "run",
            "--",
            "sh",
            "-c",
            "echo READY; exec sleep 1 </dev/null >/dev/null 2>&1",
        ],
    );
    s.wait_for("READY");
    assert_eq!(s.wait_exit().code(), Some(0));
    assert!(start.elapsed() >= Duration::from_secs(1));
}

#[test]
fn output_written_just_before_exit_is_not_lost() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::start(
        dir.path(),
        &[
            "run",
            "--",
            "sh",
            "-c",
            "i=0; while [ $i -lt 3000 ]; do echo line$i; i=$((i+1)); done; echo END; exit 4",
        ],
    );
    assert_eq!(s.exit_status().code(), Some(4));
    s.wait_for("line2999\r\nEND\r\n");
}

#[test]
fn signals_still_reach_a_command_that_let_go_of_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &[
            "run",
            "--",
            "sh",
            "-c",
            "echo READY; exec sleep 30 </dev/null >/dev/null 2>&1",
        ],
    );
    s.wait_for("READY");
    // Give keylock time to see the command's side of the pty close.
    thread::sleep(SETTLE);
    s.signal(libc::SIGTERM);
    assert_eq!(s.wait_exit().code(), Some(143));
}

#[test]
fn hangup_ends_the_session_and_removes_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let s = Session::start(
        dir.path(),
        &["run", "--", "sh", "-c", "echo READY; sleep 30"],
    );
    s.wait_for("READY");
    let socket = dir.path().join("keylock/sh.sock");
    assert!(socket.exists());
    s.signal(libc::SIGHUP);
    assert_eq!(s.wait_exit().code(), Some(129));
    assert!(!socket.exists());
}

#[test]
fn startup_errors() {
    let dir = tempfile::tempdir().unwrap();

    let not_tty = Command::new(BIN)
        .args(["run", "--", "true"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(not_tty.status.code(), Some(2));
    assert!(stderr(&not_tty).contains("stdin is not a terminal"));

    assert_eq!(keylock(dir.path(), &["run", "--"]).status.code(), Some(2));

    let missing = Session::start(dir.path(), &["run", "--", "keylock-no-such-command"]);
    missing.wait_for("keylock: keylock-no-such-command:");
    assert_eq!(missing.wait_exit().code(), Some(127));

    let bad_name = Session::start(dir.path(), &["run", "--name", "../x", "--", "true"]);
    bad_name.wait_for("invalid session name");
    assert_eq!(bad_name.wait_exit().code(), Some(2));
}
