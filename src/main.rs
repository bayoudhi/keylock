use keylock::cli::{self, Command, RunOptions};
use keylock::control::{self, Listener, Request, SendError};
use keylock::{pty, relay};
use std::io;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match cli::parse(&args, std::env::var("KEYLOCK_PHRASE").ok()) {
        Ok(Command::Help) => {
            println!("{}", cli::USAGE);
            0
        }
        Ok(Command::Run(options)) => run(options),
        Ok(Command::On(name)) => request(&name, Request::Lock),
        Ok(Command::Off(name)) => request(&name, Request::Unlock),
        Ok(Command::Status(name)) => request(&name, Request::Status),
        Ok(Command::Ls) => ls(),
        Err(msg) => {
            eprintln!("keylock: {msg}\n\n{}", cli::USAGE);
            2
        }
    };
    std::process::exit(code);
}

fn run(options: RunOptions) -> i32 {
    if !pty::is_tty(0) {
        eprintln!("keylock: stdin is not a terminal");
        return 2;
    }
    let base = match options.name {
        Some(name) if control::valid_name(&name) => name,
        Some(name) => {
            eprintln!(
                "keylock: invalid session name `{name}` \
                 (letters, digits, `.`, `_` and `-`, at most 64, not starting with `.`)"
            );
            return 2;
        }
        None => control::default_name(&options.command[0]),
    };
    let dir = control::socket_dir();
    if let Err(msg) = control::ensure_dir(&dir) {
        eprintln!("keylock: {msg}");
        return 1;
    }
    let listener = match Listener::bind(&dir, &base) {
        Ok(listener) => listener,
        Err(msg) => {
            eprintln!("keylock: {msg}");
            return 1;
        }
    };
    let program = options.command[0].clone();
    let config = relay::Config {
        command: options.command,
        locked: options.locked,
        hotkey: options.hotkey,
        phrase: options.phrase,
    };
    match relay::run(config, &listener) {
        Ok(code) => code,
        Err(relay::Failure::Spawn(e)) => {
            let (code, message) = spawn_failure(&program, &e);
            eprintln!("{message}");
            code
        }
        Err(relay::Failure::Io(e)) => {
            eprintln!("keylock: {e}");
            1
        }
    }
}

/// Exit code and message for a command that could not be started.
fn spawn_failure(program: &str, e: &io::Error) -> (i32, String) {
    match e.kind() {
        io::ErrorKind::NotFound => (127, format!("keylock: {program}: {e}")),
        io::ErrorKind::PermissionDenied => (126, format!("keylock: {program}: {e}")),
        // openpty, setsid and the like failed: not a problem with the command.
        _ => (1, format!("keylock: {e}")),
    }
}

fn request(name: &str, request: Request) -> i32 {
    let dir = control::socket_dir();
    match control::send(&dir, name, request) {
        Ok(line) => {
            println!("{line}");
            0
        }
        Err(SendError::NoSession) => {
            eprintln!("keylock: no session named {name}");
            let sessions = control::list(&dir);
            if sessions.is_empty() {
                eprintln!("no live sessions");
            } else {
                eprintln!("live sessions:");
                for (live, _) in sessions {
                    eprintln!("  {live}");
                }
            }
            1
        }
        Err(SendError::Io(e)) => {
            eprintln!("keylock: {name}: {e}");
            1
        }
    }
}

fn ls() -> i32 {
    for (name, state) in control::list(&control::socket_dir()) {
        // "locked pid=1 cmd=x y" -> "locked  pid=1  cmd=x y"
        let columns: Vec<&str> = state.splitn(3, ' ').collect();
        println!("{name}  {}", columns.join("  "));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_failures() {
        let not_found = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(spawn_failure("m.sh", &not_found).0, 127);
        assert!(spawn_failure("m.sh", &not_found)
            .1
            .starts_with("keylock: m.sh: "));

        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(spawn_failure("m.sh", &denied).0, 126);
        assert!(spawn_failure("m.sh", &denied)
            .1
            .starts_with("keylock: m.sh: "));

        // openpty, setsid and the like: not the command's fault.
        let other = io::Error::from_raw_os_error(libc::EMFILE);
        assert_eq!(
            spawn_failure("m.sh", &other),
            (1, format!("keylock: {other}"))
        );
    }
}
