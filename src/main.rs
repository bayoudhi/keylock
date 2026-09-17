use keylock::cli::{self, Command};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args, std::env::var("KEYLOCK_PHRASE").ok()) {
        Ok(Command::Help) => println!("{}", cli::USAGE),
        Ok(_) => {
            eprintln!("keylock: not implemented yet");
            std::process::exit(1);
        }
        Err(msg) => {
            eprintln!("keylock: {msg}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    }
}
