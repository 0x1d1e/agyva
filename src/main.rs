use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use agyva::cli::{Cli, Io, run};
use agyva::core::error::ErrorKind;
use agyva::paths::Env;

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGYVA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}

fn is_broken_pipe(e: &agyva::core::error::Error) -> bool {
    use std::error::Error as _;
    e.kind() == ErrorKind::Io
        && e.source()
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let stdout_tty = std::io::stdout().is_terminal();
    let stdin_tty = std::io::stdin().is_terminal();
    // The interactive view owns the screen: log lines on stderr would corrupt it.
    if !(cli.command.is_none() && stdout_tty && stdin_tty) {
        init_logging();
    }
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let mut io = Io {
        stdout_tty,
        stdin_tty,
        out: &mut stdout,
        err: &mut stderr,
    };
    let code = match run(cli, Env::from_process(), &mut io) {
        Ok(code) => code,
        // A reader that closed the pipe early (`agyva list | head`) is not an error.
        Err(e) if is_broken_pipe(&e) => 0,
        Err(e) => {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if e.kind() == ErrorKind::SimulatedCrash {
                eprintln!("{}", e.message());
            } else {
                eprintln!("error: {}", e.message());
                if let Some(h) = e.user_hint() {
                    eprintln!("hint: {h}");
                }
            }
            e.exit_code()
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
