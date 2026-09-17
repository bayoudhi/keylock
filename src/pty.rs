//! Pseudo-terminal plumbing: spawning the command on a new pty, window sizes,
//! and switching the user's terminal into raw mode and back.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, Once};

pub struct Spawned {
    pub master: File,
    pub child: Child,
}

pub fn spawn(argv: &[String], env: &[(&str, &str)], size: &libc::winsize) -> io::Result<Spawned> {
    let (master, slave) = open_pty(size)?;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .envs(env.iter().copied())
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: the closure only makes async-signal-safe system calls.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // stdin is already the pty slave; make it the controlling terminal.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    // `command` owns our copies of the slave. Close them so reads on the
    // master see end-of-file once the child side is gone.
    drop(command);
    Ok(Spawned {
        master: File::from(master),
        child,
    })
}

pub fn open_pty(size: &libc::winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = *size;
    // SAFETY: the out-pointers are valid; name and termios may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty returned two new descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_fd_flag(
        master.as_raw_fd(),
        libc::F_GETFD,
        libc::F_SETFD,
        libc::FD_CLOEXEC,
    )?;
    set_fd_flag(
        slave.as_raw_fd(),
        libc::F_GETFD,
        libc::F_SETFD,
        libc::FD_CLOEXEC,
    )?;
    Ok((master, slave))
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    set_fd_flag(fd, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)
}

fn set_fd_flag(fd: RawFd, get: libc::c_int, set: libc::c_int, flag: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl get/set on a descriptor owned by the caller.
    let flags = unsafe { libc::fcntl(fd, get) };
    if flags == -1 || unsafe { libc::fcntl(fd, set, flags | flag) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

pub fn window_size(fd: RawFd) -> io::Result<libc::winsize> {
    let mut size = winsize(0, 0);
    // SAFETY: TIOCGWINSZ writes a winsize into a valid pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(size)
}

pub fn set_window_size(fd: RawFd, size: &libc::winsize) -> io::Result<()> {
    // SAFETY: TIOCSWINSZ reads a winsize from a valid pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, size) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

fn get_termios(fd: RawFd) -> io::Result<libc::termios> {
    // SAFETY: termios is plain data; tcgetattr fills it in.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(termios)
}

fn set_termios(fd: RawFd, termios: &libc::termios) -> io::Result<()> {
    // SAFETY: tcsetattr reads a valid termios.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

static SAVED: Mutex<Option<(RawFd, libc::termios)>> = Mutex::new(None);
static PANIC_HOOK: Once = Once::new();

pub struct RawMode {
    _private: (),
}

impl RawMode {
    pub fn enable(fd: RawFd) -> io::Result<RawMode> {
        let saved = get_termios(fd)?;
        let mut raw = saved;
        // SAFETY: cfmakeraw edits a valid termios in place.
        unsafe { libc::cfmakeraw(&mut raw) };
        set_termios(fd, &raw)?;
        *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some((fd, saved));
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_terminal();
                previous(info);
            }));
        });
        Ok(RawMode { _private: () })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore_terminal();
    }
}

pub fn restore_terminal() {
    let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some((fd, termios)) = saved {
        let _ = set_termios(fd, &termios);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// Reads until the child side closes. Linux reports that as EIO.
    fn read_all(mut master: &File) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn child_gets_a_controlling_terminal_size_and_env() {
        let mut spawned = spawn(
            &argv(&["sh", "-c", "stty size; tty; printf \"$KEYLOCK_T\""]),
            &[("KEYLOCK_T", "hello")],
            &winsize(30, 100),
        )
        .unwrap();
        let out = read_all(&spawned.master);
        assert!(spawned.child.wait().unwrap().success());
        assert!(out.contains("30 100"), "{out:?}");
        assert!(out.contains("/dev/"), "{out:?}");
        assert!(!out.contains("not a tty"), "{out:?}");
        assert!(out.ends_with("hello"), "{out:?}");
    }

    #[test]
    fn exec_errors_keep_their_kind() {
        let missing = spawn(&argv(&["keylock-no-such-command"]), &[], &winsize(24, 80));
        assert_eq!(missing.err().unwrap().kind(), io::ErrorKind::NotFound);

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("script.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        let denied = spawn(&argv(&[script.to_str().unwrap()]), &[], &winsize(24, 80));
        assert_eq!(
            denied.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn window_size_round_trip() {
        let (master, _slave) = open_pty(&winsize(24, 80)).unwrap();
        assert_eq!(window_size(master.as_raw_fd()).unwrap().ws_col, 80);
        set_window_size(master.as_raw_fd(), &winsize(40, 120)).unwrap();
        let size = window_size(master.as_raw_fd()).unwrap();
        assert_eq!((size.ws_row, size.ws_col), (40, 120));
    }

    #[test]
    fn raw_mode_is_restored() {
        let (_master, slave) = open_pty(&winsize(24, 80)).unwrap();
        let fd = slave.as_raw_fd();
        assert!(is_tty(fd));
        let canonical = |fd| get_termios(fd).unwrap().c_lflag & libc::ICANON != 0;
        assert!(canonical(fd));
        let raw = RawMode::enable(fd).unwrap();
        assert!(!canonical(fd));
        drop(raw);
        assert!(canonical(fd));
        restore_terminal(); // second restore is a no-op
        assert!(canonical(fd));
    }

    #[test]
    fn regular_files_are_not_terminals() {
        let file = tempfile::tempfile().unwrap();
        assert!(!is_tty(file.as_raw_fd()));
    }
}
