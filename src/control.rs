//! Per-session control sockets: where they live, how sessions are named, the
//! one-line protocol, and the client side used by `on`, `off`, `status`, `ls`.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SERVER_TIMEOUT: Duration = Duration::from_millis(500);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_NAME: usize = 64;
const MAX_LINE: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Lock,
    Unlock,
    Status,
}

impl Request {
    pub fn parse(line: &str) -> Option<Request> {
        match line.trim() {
            "lock" => Some(Request::Lock),
            "unlock" => Some(Request::Unlock),
            "status" => Some(Request::Status),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Request::Lock => "lock",
            Request::Unlock => "unlock",
            Request::Status => "status",
        }
    }
}

pub fn state_line(locked: bool, pid: u32, command: &str) -> String {
    let state = if locked { "locked" } else { "unlocked" };
    format!("{state} pid={pid} cmd={command}")
}

pub fn resolve_dir(
    xdg_runtime_dir: Option<OsString>,
    tmpdir: Option<OsString>,
    uid: u32,
) -> PathBuf {
    let usable = |value: Option<OsString>| value.filter(|v| !v.is_empty()).map(PathBuf::from);
    match usable(xdg_runtime_dir).or_else(|| usable(tmpdir)) {
        Some(base) => base.join("keylock"),
        None => PathBuf::from(format!("/tmp/keylock-{uid}")),
    }
}

pub fn socket_dir() -> PathBuf {
    resolve_dir(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("TMPDIR"),
        current_uid(),
    )
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    let refuse = |why: &dyn std::fmt::Display| format!("refusing to use {}: {why}", dir.display());
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(refuse(&e)),
    }
    let meta = fs::symlink_metadata(dir).map_err(|e| refuse(&e))?;
    if !meta.is_dir() {
        return Err(refuse(&"not a directory"));
    }
    if meta.uid() != current_uid() {
        return Err(refuse(&"owned by another user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refuse(&"accessible by other users"));
    }
    Ok(())
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn default_name(command: &str) -> String {
    let base = Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let mut name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_start_matches('.')
        .to_string();
    // Leave room for a collision suffix such as `-2`.
    name.truncate(MAX_NAME - 4);
    if name.is_empty() {
        "session".to_string()
    } else {
        name
    }
}

/// Bytes available for a session name in `dir`: the socket path must fit
/// `sun_path` with its terminating NUL, and the name must stay valid.
fn name_room(dir: &Path) -> usize {
    // SAFETY: sockaddr_un is plain data; a zeroed one is valid.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let max_path = addr.sun_path.len() - 1;
    // Everything but the name: the directory, a separator and `.sock`.
    let used = dir.join("x.sock").as_os_str().len() - 1;
    max_path.saturating_sub(used).min(MAX_NAME)
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    name: String,
}

impl Listener {
    /// Binds `dir/NAME.sock`, where NAME is `base` shortened to fit the
    /// socket path limit and, if taken by a live session, suffixed `-2`, `-3`, ….
    pub fn bind(dir: &Path, base: &str) -> Result<Listener, String> {
        let room = name_room(dir);
        let mut attempt = 1;
        loop {
            let suffix = if attempt == 1 {
                String::new()
            } else {
                format!("-{attempt}")
            };
            attempt += 1;
            let Some(keep) = room.checked_sub(suffix.len()).filter(|&keep| keep > 0) else {
                return Err(format!(
                    "cannot create a session socket in {}: the path is too long \
                     (set a shorter $TMPDIR or $XDG_RUNTIME_DIR)",
                    dir.display()
                ));
            };
            let name = format!("{}{suffix}", truncate(base, keep));
            let path = dir.join(format!("{name}.sock"));
            if fs::symlink_metadata(&path).is_ok() {
                if UnixStream::connect(&path).is_ok() {
                    continue;
                }
                let _ = fs::remove_file(&path);
            }
            let fail = |e: io::Error| format!("cannot listen on {}: {e}", path.display());
            let listener = UnixListener::bind(&path).map_err(fail)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(fail)?;
            listener.set_nonblocking(true).map_err(fail)?;
            return Ok(Listener {
                listener,
                path,
                name,
            });
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Answers every connection that is already waiting, then returns.
    pub fn serve(&self, mut respond: impl FnMut(Request) -> String) {
        while let Ok((stream, _)) = self.listener.accept() {
            let _ = serve_one(&stream, &mut respond);
        }
    }
}

fn serve_one(stream: &UnixStream, respond: &mut impl FnMut(Request) -> String) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(SERVER_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_LINE)).read_line(&mut line)?;
    if line.is_empty() {
        // A liveness probe from `Listener::bind`: connect, then close.
        return Ok(());
    }
    let reply = match Request::parse(&line) {
        Some(request) => respond(request),
        None => "error unknown request".to_string(),
    };
    let mut stream = stream;
    stream.write_all(format!("{reply}\n").as_bytes())
}

impl AsRawFd for Listener {
    fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub enum SendError {
    NoSession,
    Io(io::Error),
}

pub fn send(dir: &Path, name: &str, request: Request) -> Result<String, SendError> {
    if !valid_name(name) {
        return Err(SendError::NoSession);
    }
    let mut stream =
        UnixStream::connect(dir.join(format!("{name}.sock"))).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => SendError::NoSession,
            _ => SendError::Io(e),
        })?;
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(SendError::Io)?;
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .map_err(SendError::Io)?;
    writeln!(stream, "{}", request.as_str()).map_err(SendError::Io)?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_LINE))
        .read_line(&mut line)
        .map_err(SendError::Io)?;
    if line.is_empty() {
        return Err(SendError::Io(io::ErrorKind::UnexpectedEof.into()));
    }
    Ok(line.trim_end().to_string())
}

pub fn list(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name();
            file_name
                .to_str()?
                .strip_suffix(".sock")
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        // keylock never creates such names; leave whatever they are alone.
        .filter(|name| valid_name(name))
        .filter_map(|name| match send(dir, &name, Request::Status) {
            Ok(state) => Some((name, state)),
            // For a valid name this means the connection was refused or the
            // file is gone: nobody is listening.
            Err(SendError::NoSession) => {
                let _ = fs::remove_file(dir.join(format!("{name}.sock")));
                None
            }
            Err(SendError::Io(_)) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    /// Binds a `UnixListener` at `path` and drops it to simulate a session
    /// whose owner died without cleaning up, then waits until a connect to
    /// `path` actually fails. A forked pty-test child can hold a CLOEXEC dup
    /// of the just-dropped listener fd until it execs, during which a
    /// connect can still succeed — so dropping alone doesn't guarantee the
    /// socket looks stale yet.
    fn stale_socket(path: &Path) {
        drop(UnixListener::bind(path).unwrap());
        let start = Instant::now();
        while UnixStream::connect(path).is_ok() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "socket at {} never went stale",
                path.display()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Answers requests on `listener` until `client` finishes, then returns its result.
    fn with_server<T: Send + 'static>(
        listener: &Listener,
        locked: bool,
        client: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let handle = thread::spawn(client);
        while !handle.is_finished() {
            listener.serve(|_request| state_line(locked, 7, "sh"));
            thread::sleep(Duration::from_millis(5));
        }
        handle.join().unwrap()
    }

    #[test]
    fn requests_and_state_lines() {
        assert_eq!(Request::parse("lock\n"), Some(Request::Lock));
        assert_eq!(Request::parse("unlock"), Some(Request::Unlock));
        assert_eq!(Request::parse("status\r\n"), Some(Request::Status));
        assert_eq!(Request::parse("reboot"), None);
        assert_eq!(Request::Unlock.as_str(), "unlock");
        assert_eq!(
            state_line(true, 42, "./m.sh a"),
            "locked pid=42 cmd=./m.sh a"
        );
        assert_eq!(state_line(false, 1, "x"), "unlocked pid=1 cmd=x");
    }

    #[test]
    fn directory_resolution() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            resolve_dir(os("/run/user/501"), os("/tmp/x"), 501),
            PathBuf::from("/run/user/501/keylock")
        );
        assert_eq!(
            resolve_dir(None, os("/var/folders/T/"), 501),
            PathBuf::from("/var/folders/T/keylock")
        );
        assert_eq!(
            resolve_dir(os(""), None, 501),
            PathBuf::from("/tmp/keylock-501")
        );
    }

    #[test]
    fn directory_safety() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("keylock");
        ensure_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        ensure_dir(&dir).unwrap();

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let err = ensure_dir(&dir).unwrap_err();
        assert!(err.starts_with("refusing to use"), "{err}");
        assert!(err.contains("accessible by other users"), "{err}");

        let file = tmp.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(ensure_dir(&file).unwrap_err().contains("not a directory"));
    }

    #[test]
    fn names() {
        assert!(valid_name("migrate.sh"));
        assert!(valid_name("a-2_b"));
        assert!(!valid_name(""));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("../etc"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(&"x".repeat(65)));
        assert_eq!(default_name("./scripts/migrate.sh"), "migrate.sh");
        assert_eq!(default_name("my tool"), "my_tool");
        assert_eq!(default_name(".env"), "env");
        assert_eq!(default_name("/"), "session");
        assert_eq!(default_name(&"y".repeat(100)).len(), 60);
    }

    #[test]
    fn send_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = Listener::bind(tmp.path(), "mig").unwrap();
        assert_eq!(listener.name(), "mig");
        let dir = tmp.path().to_path_buf();
        let reply = with_server(&listener, true, move || {
            send(&dir, "mig", Request::Status).unwrap()
        });
        assert_eq!(reply, "locked pid=7 cmd=sh");
    }

    #[test]
    fn socket_is_private_and_removed_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mig.sock");
        let listener = Listener::bind(tmp.path(), "mig").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        drop(listener);
        assert!(!path.exists());
    }

    #[test]
    fn unknown_session() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            send(tmp.path(), "nope", Request::Lock),
            Err(SendError::NoSession)
        ));
        assert!(matches!(
            send(tmp.path(), "../x", Request::Lock),
            Err(SendError::NoSession)
        ));
    }

    #[test]
    fn live_names_get_a_suffix_and_stale_sockets_are_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Listener::bind(tmp.path(), "mig").unwrap();
        let second = Listener::bind(tmp.path(), "mig").unwrap();
        assert_eq!(second.name(), "mig-2");

        // A socket file whose owner died without cleaning up.
        stale_socket(&tmp.path().join("old.sock"));
        assert!(tmp.path().join("old.sock").exists());
        assert_eq!(Listener::bind(tmp.path(), "old").unwrap().name(), "old");
        drop((first, second));
    }

    /// A directory under `tmp` whose socket paths leave exactly `room` bytes
    /// for the session name.
    fn dir_with_room(tmp: &Path, room: usize) -> PathBuf {
        // SAFETY: sockaddr_un is plain data.
        let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let max_path = addr.sun_path.len() - 1;
        // tmp + "/" + dir + "/" + name + ".sock"
        let used = tmp.as_os_str().len() + 1 + 1 + room + ".sock".len();
        assert!(used < max_path, "temp dir {} is too long", tmp.display());
        let dir = tmp.join("d".repeat(max_path - used));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn long_names_are_shortened_to_fit_the_socket_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = dir_with_room(tmp.path(), 20);
        let base = "n".repeat(64);
        let first = Listener::bind(&dir, &base).unwrap();
        assert_eq!(first.name(), "n".repeat(20));
        assert!(dir.join(format!("{}.sock", first.name())).exists());
        let second = Listener::bind(&dir, &base).unwrap();
        assert_eq!(second.name(), format!("{}-2", "n".repeat(18)));
        let name = second.name().to_string();
        let reply = with_server(&second, false, move || {
            send(&dir, &name, Request::Status).unwrap()
        });
        assert_eq!(reply, "unlocked pid=7 cmd=sh");
        drop(first);
    }

    #[test]
    fn suffixed_names_stay_valid() {
        // A short directory, so the name limit applies before the path limit.
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let base = "a".repeat(MAX_NAME);
        let _first = Listener::bind(tmp.path(), &base).unwrap();
        let second = Listener::bind(tmp.path(), &base).unwrap();
        assert!(valid_name(second.name()), "{}", second.name());
        assert!(second.name().ends_with("-2"), "{}", second.name());
    }

    #[test]
    fn directory_too_long_for_any_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = dir_with_room(tmp.path(), 0);
        let err = Listener::bind(&dir, "mig").err().unwrap();
        assert!(err.contains(&dir.display().to_string()), "{err}");
        assert!(err.contains("$TMPDIR"), "{err}");
        assert!(err.contains("$XDG_RUNTIME_DIR"), "{err}");
    }

    #[test]
    fn list_never_removes_a_live_socket() {
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        // Names `send` refuses must not be mistaken for dead sessions.
        let path = tmp
            .path()
            .join(format!("{}.sock", "z".repeat(MAX_NAME + 2)));
        let _live = UnixListener::bind(&path).unwrap();
        assert!(list(tmp.path()).is_empty());
        assert!(path.exists());
    }

    #[test]
    fn list_reports_live_sessions_and_removes_stale_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = Listener::bind(tmp.path(), "live").unwrap();
        stale_socket(&tmp.path().join("dead.sock"));
        let dir = tmp.path().to_path_buf();
        let sessions = with_server(&listener, false, move || list(&dir));
        assert_eq!(
            sessions,
            vec![("live".to_string(), "unlocked pid=7 cmd=sh".to_string())]
        );
        assert!(!tmp.path().join("dead.sock").exists());
    }
}
