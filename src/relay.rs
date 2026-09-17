//! The event loop: moves bytes between the user's terminal and the command,
//! applies the gate, serves the control socket and handles signals.

use crate::control::{self, Listener, Request};
use crate::gate::{Event, Gate, Output};
use crate::pty::{self, RawMode};
use crate::title::{self, TitleFilter, LOCK_PREFIX};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

const STDIN: RawFd = 0;
const STDOUT: RawFd = 1;
const BEL: u8 = 0x07;
const ESCAPE_TIMEOUT: Duration = Duration::from_millis(25);
const FLASH: Duration = Duration::from_secs(2);
/// How long to keep copying output after the command exits.
const DRAIN: Duration = Duration::from_millis(100);
const MAX_PENDING_INPUT: usize = 64 * 1024;
const FORWARDED_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGQUIT];

pub struct Config {
    pub command: Vec<String>,
    pub locked: bool,
    pub hotkey: bool,
    pub phrase: Option<String>,
}

#[derive(Debug)]
pub enum Failure {
    Spawn(io::Error),
    Io(io::Error),
}

enum End {
    ChildDone(ExitStatus),
    HangUp,
}

pub fn run(config: Config, listener: &Listener) -> Result<i32, Failure> {
    let size = pty::window_size(STDIN).map_err(Failure::Io)?;
    let signals = install_signal_handlers().map_err(Failure::Io)?;
    let pty::Spawned { master, child } =
        pty::spawn(&config.command, &[("KEYLOCK_NAME", listener.name())], &size)
            .map_err(Failure::Spawn)?;
    pty::set_nonblocking(master.as_raw_fd()).map_err(Failure::Io)?;
    let raw = RawMode::enable(STDIN).map_err(Failure::Io)?;

    let mut relay = Relay {
        listener,
        master,
        pty_open: true,
        signals,
        gate: Gate::new(config.locked, config.hotkey, config.phrase),
        title: TitleFilter::default(),
        child_pid: child.id() as libc::pid_t,
        child,
        command_line: config.command.join(" "),
        to_child: Vec::new(),
        to_user: Vec::new(),
        last_input: Instant::now(),
        flash_until: None,
        next_flash: Instant::now(),
    };
    let end = relay.run_loop().and_then(|end| {
        if matches!(end, End::ChildDone(_)) {
            relay.drain_child_output()?;
            relay.reset_title();
        }
        Ok(end)
    });
    // Closing the master hangs up the command's terminal, as closing a
    // terminal window would. After the command exits, that reaches only what
    // it left running in the background.
    drop(relay);
    drop(raw);

    match end.map_err(Failure::Io)? {
        End::ChildDone(status) => Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))),
        End::HangUp => Ok(128 + libc::SIGHUP),
    }
}

struct Relay<'a> {
    listener: &'a Listener,
    master: File,
    /// False once no process has the command's side of the pty open. The
    /// master stays open regardless: closing it would hang up the command.
    pty_open: bool,
    signals: UnixStream,
    gate: Gate,
    title: TitleFilter,
    child_pid: libc::pid_t,
    child: Child,
    command_line: String,
    /// Input waiting for the command to read it.
    to_child: Vec<u8>,
    /// keylock's own output (titles, bell) waiting for a safe point.
    to_user: Vec<u8>,
    last_input: Instant,
    flash_until: Option<Instant>,
    next_flash: Instant,
}

impl Relay<'_> {
    fn run_loop(&mut self) -> io::Result<End> {
        if self.gate.is_locked() {
            self.queue_title();
            self.emit_queued()?;
        }
        loop {
            // A negative descriptor is ignored by poll. Linux keeps reporting
            // POLLHUP on a master whose other side is closed.
            let master_fd = if self.pty_open {
                self.master.as_raw_fd()
            } else {
                -1
            };
            let master_events = if self.to_child.is_empty() {
                libc::POLLIN
            } else {
                libc::POLLIN | libc::POLLOUT
            };
            let mut fds = [
                poll_fd(STDIN, libc::POLLIN),
                poll_fd(master_fd, master_events),
                poll_fd(self.listener.as_raw_fd(), libc::POLLIN),
                poll_fd(self.signals.as_raw_fd(), libc::POLLIN),
            ];
            let timeout = self.poll_timeout();
            // SAFETY: `fds` is a valid array of pollfd for its whole length.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if rc == -1 {
                let err = io::Error::last_os_error();
                // EAGAIN can replace EINTR when the signal pipe is full.
                if matches!(
                    err.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                return Err(err);
            }
            let ready = |i: usize, mask: libc::c_short| fds[i].revents & mask != 0;
            let hangup_or_in = libc::POLLIN | libc::POLLHUP | libc::POLLERR;

            if ready(3, libc::POLLIN) {
                if let Some(end) = self.handle_signals()? {
                    return Ok(end);
                }
            }
            if ready(0, hangup_or_in) && self.read_stdin()? {
                return Ok(End::HangUp);
            }
            if ready(1, libc::POLLOUT) {
                self.write_to_child()?;
            }
            if ready(1, hangup_or_in) {
                self.read_child()?;
            }
            if ready(2, libc::POLLIN) {
                self.serve_control();
            }
            self.on_timers()?;
            self.emit_queued()?;
        }
    }

    fn poll_timeout(&self) -> libc::c_int {
        let now = Instant::now();
        let escape = self
            .gate
            .has_partial()
            .then(|| (self.last_input + ESCAPE_TIMEOUT).saturating_duration_since(now));
        let flash = self.flash_until.map(|t| t.saturating_duration_since(now));
        match escape.into_iter().chain(flash).min() {
            Some(wait) => wait.as_millis() as libc::c_int + 1,
            None => -1,
        }
    }

    /// Returns `true` when the user's terminal has gone away.
    fn read_stdin(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; 4096];
        let n = match read_fd(STDIN, &mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => n,
            Err(e) if retryable(&e) => return Ok(false),
            Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(true),
            Err(e) => return Err(e),
        };
        self.last_input = Instant::now();
        let out = self.gate.feed(&buf[..n]);
        self.apply(out);
        self.write_to_child()?;
        Ok(false)
    }

    fn apply(&mut self, out: Output) {
        if self.to_child.len() + out.forward.len() <= MAX_PENDING_INPUT {
            self.to_child.extend_from_slice(&out.forward);
        }
        for event in out.events {
            match event {
                Event::Lock | Event::Unlock => self.state_changed(),
                Event::DroppedInput => self.flash(),
            }
        }
    }

    fn write_to_child(&mut self) -> io::Result<()> {
        if !self.pty_open {
            // Nothing is left to read it.
            self.to_child.clear();
        }
        if self.to_child.is_empty() {
            return Ok(());
        }
        match self.master.write(&self.to_child) {
            Ok(n) => {
                self.to_child.drain(..n);
            }
            Err(e) if retryable(&e) => {}
            // The command's side is gone; the read path will notice.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => self.to_child.clear(),
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Copies the command's output to the user. Returns `true` if there was
    /// any; notes when the command's side of the pty has closed.
    fn read_child(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; 65536];
        match self.master.read(&mut buf) {
            Ok(0) => {
                self.pty_open = false;
                Ok(false)
            }
            Ok(n) => {
                let out = self.title.filter(&buf[..n], self.gate.is_locked());
                write_all_fd(STDOUT, &out)?;
                Ok(true)
            }
            Err(e) if retryable(&e) => Ok(false),
            // Linux reports a closed pty as EIO rather than end-of-file.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                self.pty_open = false;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// After the command exits: copies what it wrote last, without waiting
    /// on anything it left running that still holds the pty.
    fn drain_child_output(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + DRAIN;
        while self.pty_open && Instant::now() < deadline && self.read_child()? {}
        Ok(())
    }

    fn serve_control(&mut self) {
        let listener = self.listener;
        let gate = &mut self.gate;
        let pid = self.child_pid as u32;
        let command_line = &self.command_line;
        let mut changed = false;
        listener.serve(|request| {
            match request {
                Request::Lock => changed |= gate.set_locked(true),
                Request::Unlock => changed |= gate.set_locked(false),
                Request::Status => {}
            }
            control::state_line(gate.is_locked(), pid, command_line)
        });
        if changed {
            self.state_changed();
        }
    }

    fn handle_signals(&mut self) -> io::Result<Option<End>> {
        let mut buf = [0u8; 64];
        let n = match (&self.signals).read(&mut buf) {
            Ok(n) => n,
            Err(e) if retryable(&e) => 0,
            Err(e) => return Err(e),
        };
        for &signal in &buf[..n] {
            match libc::c_int::from(signal) {
                // Checked below, after every wake-up.
                libc::SIGCHLD => {}
                libc::SIGWINCH => {
                    if let Ok(size) = pty::window_size(STDIN) {
                        let _ = pty::set_window_size(self.master.as_raw_fd(), &size);
                    }
                }
                libc::SIGHUP => return Ok(Some(End::HangUp)),
                signal => {
                    // SAFETY: the command leads its own session, so its pid
                    // is also its process group id.
                    unsafe { libc::kill(-self.child_pid, signal) };
                }
            }
        }
        // The session ends when the command exits, not when it closes the
        // terminal: it may redirect its output and keep running.
        Ok(self.child.try_wait()?.map(End::ChildDone))
    }

    fn on_timers(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if self.gate.has_partial() && now >= self.last_input + ESCAPE_TIMEOUT {
            let out = self.gate.flush();
            self.apply(out);
            self.write_to_child()?;
        }
        if self.flash_until.is_some_and(|t| now >= t) {
            self.flash_until = None;
            if self.gate.is_locked() {
                self.queue_title();
            }
        }
        Ok(())
    }

    fn state_changed(&mut self) {
        self.flash_until = None;
        self.queue_title();
    }

    fn flash(&mut self) {
        let now = Instant::now();
        if now < self.next_flash {
            return;
        }
        self.next_flash = now + FLASH;
        self.flash_until = Some(now + FLASH);
        self.to_user.push(BEL);
        let text = format!("{LOCK_PREFIX}locked — keylock off {}", self.listener.name());
        self.to_user.extend(title::set_title(text.as_bytes()));
    }

    fn queue_title(&mut self) {
        let mut text = Vec::new();
        if self.gate.is_locked() {
            text.extend_from_slice(LOCK_PREFIX.as_bytes());
        }
        text.extend_from_slice(self.base_title());
        self.to_user.extend(title::set_title(&text));
    }

    fn base_title(&self) -> &[u8] {
        self.title
            .last_title()
            .unwrap_or(self.listener.name().as_bytes())
    }

    fn reset_title(&mut self) {
        if self.gate.is_locked() && !self.title.in_sequence() {
            let _ = write_all_fd(STDOUT, &title::set_title(self.base_title()));
        }
    }

    fn emit_queued(&mut self) -> io::Result<()> {
        if self.to_user.is_empty() || self.title.in_sequence() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.to_user);
        write_all_fd(STDOUT, &bytes)
    }
}

fn poll_fd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn retryable(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_all_fd(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: `data` is valid for reads of `data.len()` bytes.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n >= 0 {
            data = &data[n as usize..];
            continue;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => {
                let mut fds = [poll_fd(fd, libc::POLLOUT)];
                // SAFETY: one valid pollfd.
                unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
            }
            _ => return Err(err),
        }
    }
    Ok(())
}

static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let byte = signal as u8;
    // SAFETY: write is async-signal-safe. If the pipe is full the signal is
    // dropped, which is fine: one pending wake-up is enough.
    unsafe {
        libc::write(
            SIGNAL_PIPE.load(Ordering::Relaxed),
            (&byte as *const u8).cast(),
            1,
        )
    };
}

fn install_signal_handlers() -> io::Result<UnixStream> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    SIGNAL_PIPE.store(writer.as_raw_fd(), Ordering::Relaxed);
    // The handlers write to this descriptor for the rest of the process.
    std::mem::forget(writer);
    let signals = [libc::SIGWINCH, libc::SIGHUP, libc::SIGCHLD]
        .into_iter()
        .chain(FORWARDED_SIGNALS);
    for signal in signals {
        // SAFETY: a zeroed sigaction is valid; we then fill in the handler,
        // an empty mask and SA_RESTART.
        let rc = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut())
        };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(reader)
}
