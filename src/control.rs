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

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    name: String,
}

impl Listener {
    pub fn bind(dir: &Path, base: &str) -> Result<Listener, String> {
        let mut attempt = 1;
        loop {
            let name = if attempt == 1 {
                base.to_string()
            } else {
                format!("{base}-{attempt}")
            };
            attempt += 1;
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
        .filter_map(|name| match send(dir, &name, Request::Status) {
            Ok(state) => Some((name, state)),
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
        drop(UnixListener::bind(tmp.path().join("old.sock")).unwrap());
        assert!(tmp.path().join("old.sock").exists());
        assert_eq!(Listener::bind(tmp.path(), "old").unwrap().name(), "old");
        drop((first, second));
    }

    #[test]
    fn list_reports_live_sessions_and_removes_stale_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = Listener::bind(tmp.path(), "live").unwrap();
        drop(UnixListener::bind(tmp.path().join("dead.sock")).unwrap());
        let dir = tmp.path().to_path_buf();
        let sessions = with_server(&listener, false, move || list(&dir));
        assert_eq!(
            sessions,
            vec![("live".to_string(), "unlocked pid=7 cmd=sh".to_string())]
        );
        assert!(!tmp.path().join("dead.sock").exists());
    }
}
