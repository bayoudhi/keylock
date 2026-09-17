//! Command-line parsing. Hand-rolled: the surface is five subcommands.

pub const USAGE: &str = "\
usage:
  keylock run [--locked] [--name NAME] [--phrase PHRASE | --no-phrase] [--no-hotkey] -- COMMAND [ARGS...]
  keylock on NAME        lock a session
  keylock off NAME       unlock a session
  keylock status NAME    print a session's state
  keylock ls             list live sessions

While a session is locked, all input to the command is dropped.
Lock from inside the terminal with Ctrl+] then l.
Unlock with `keylock off NAME`, or type the phrase (default: unlock) and Enter.";

pub const DEFAULT_PHRASE: &str = "unlock";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(RunOptions),
    On(String),
    Off(String),
    Status(String),
    Ls,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RunOptions {
    pub locked: bool,
    pub name: Option<String>,
    /// `None` when unlocking by phrase is disabled.
    pub phrase: Option<String>,
    pub hotkey: bool,
    pub command: Vec<String>,
}

pub fn parse(args: &[String], env_phrase: Option<String>) -> Result<Command, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Err("missing subcommand".into());
    };
    match sub.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "run" => parse_run(rest, env_phrase).map(Command::Run),
        "on" => one_name(rest).map(Command::On),
        "off" => one_name(rest).map(Command::Off),
        "status" => one_name(rest).map(Command::Status),
        "ls" if rest.is_empty() => Ok(Command::Ls),
        "ls" => Err("ls takes no arguments".into()),
        other => Err(format!("unknown subcommand `{other}`")),
    }
}

fn one_name(rest: &[String]) -> Result<String, String> {
    match rest {
        [name] => Ok(name.clone()),
        [] => Err("missing session name".into()),
        _ => Err("expected exactly one session name".into()),
    }
}

fn parse_run(rest: &[String], env_phrase: Option<String>) -> Result<RunOptions, String> {
    let mut locked = false;
    let mut name = None;
    let mut phrase_flag = None;
    let mut no_phrase = false;
    let mut hotkey = true;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => break,
            "--locked" => locked = true,
            "--no-hotkey" => hotkey = false,
            "--no-phrase" => no_phrase = true,
            "--name" => name = Some(value(&mut iter, "--name")?),
            "--phrase" => phrase_flag = Some(value(&mut iter, "--phrase")?),
            other => {
                return Err(format!(
                    "unknown option `{other}` (put the command after `--`)"
                ))
            }
        }
    }
    let command: Vec<String> = iter.cloned().collect();
    if command.is_empty() {
        return Err("missing command after `--`".into());
    }
    if no_phrase && phrase_flag.is_some() {
        return Err("--phrase and --no-phrase conflict".into());
    }
    let phrase = if no_phrase {
        None
    } else {
        let phrase = phrase_flag
            .or(env_phrase)
            .unwrap_or_else(|| DEFAULT_PHRASE.to_string());
        if phrase.is_empty() {
            return Err("phrase must not be empty".into());
        }
        // The gate only collects printable ASCII while locked.
        if !phrase.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
            return Err("phrase must be printable ASCII".into());
        }
        Some(phrase)
    };
    Ok(RunOptions {
        locked,
        name,
        phrase,
        hotkey,
        command,
    })
}

fn value(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    iter.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn run(s: &[&str]) -> Result<RunOptions, String> {
        match parse(&args(s), None)? {
            Command::Run(opts) => Ok(opts),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_defaults() {
        assert_eq!(
            run(&["run", "--", "./migrate.sh", "--fast"]).unwrap(),
            RunOptions {
                locked: false,
                name: None,
                phrase: Some("unlock".into()),
                hotkey: true,
                command: args(&["./migrate.sh", "--fast"]),
            }
        );
    }

    #[test]
    fn run_all_flags() {
        let opts = run(&[
            "run",
            "--locked",
            "--name",
            "mig",
            "--phrase",
            "open sesame",
            "--no-hotkey",
            "--",
            "sh",
            "-c",
            "true",
        ])
        .unwrap();
        assert!(opts.locked);
        assert_eq!(opts.name.as_deref(), Some("mig"));
        assert_eq!(opts.phrase.as_deref(), Some("open sesame"));
        assert!(!opts.hotkey);
        assert_eq!(opts.command, args(&["sh", "-c", "true"]));
    }

    #[test]
    fn flags_after_double_dash_belong_to_the_command() {
        let opts = run(&["run", "--", "cmd", "--locked"]).unwrap();
        assert!(!opts.locked);
        assert_eq!(opts.command, args(&["cmd", "--locked"]));
    }

    #[test]
    fn phrase_precedence() {
        let with_env = |s: &[&str]| match parse(&args(s), Some("env phrase".into())).unwrap() {
            Command::Run(o) => o.phrase,
            _ => unreachable!(),
        };
        assert_eq!(with_env(&["run", "--", "x"]).as_deref(), Some("env phrase"));
        assert_eq!(
            with_env(&["run", "--phrase", "flag", "--", "x"]).as_deref(),
            Some("flag")
        );
        assert_eq!(with_env(&["run", "--no-phrase", "--", "x"]), None);
    }

    #[test]
    fn run_errors() {
        assert!(run(&["run", "./migrate.sh"])
            .unwrap_err()
            .contains("unknown option"));
        assert!(run(&["run", "--"]).unwrap_err().contains("missing command"));
        assert!(run(&["run", "--name"])
            .unwrap_err()
            .contains("--name needs a value"));
        assert!(run(&["run", "--phrase", "a", "--no-phrase", "--", "x"])
            .unwrap_err()
            .contains("conflict"));
        assert!(run(&["run", "--phrase", "", "--", "x"])
            .unwrap_err()
            .contains("empty"));
        assert!(run(&["run", "--phrase", "é", "--", "x"])
            .unwrap_err()
            .contains("printable ASCII"));
    }

    #[test]
    fn control_subcommands() {
        assert_eq!(
            parse(&args(&["on", "mig"]), None),
            Ok(Command::On("mig".into()))
        );
        assert_eq!(
            parse(&args(&["off", "mig"]), None),
            Ok(Command::Off("mig".into()))
        );
        assert_eq!(
            parse(&args(&["status", "mig"]), None),
            Ok(Command::Status("mig".into()))
        );
        assert_eq!(parse(&args(&["ls"]), None), Ok(Command::Ls));
        assert!(parse(&args(&["on"]), None)
            .unwrap_err()
            .contains("missing session name"));
        assert!(parse(&args(&["on", "a", "b"]), None).is_err());
        assert!(parse(&args(&["ls", "x"]), None).is_err());
    }

    #[test]
    fn help_and_unknown() {
        assert_eq!(parse(&args(&["--help"]), None), Ok(Command::Help));
        assert_eq!(parse(&args(&["-h"]), None), Ok(Command::Help));
        assert!(parse(&args(&[]), None)
            .unwrap_err()
            .contains("missing subcommand"));
        assert!(parse(&args(&["frobnicate"]), None)
            .unwrap_err()
            .contains("unknown subcommand"));
    }
}
