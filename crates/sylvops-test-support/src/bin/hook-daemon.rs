//! Runs the real daemon and hook relay from one executable, like the application CLI.

use std::{io, path::PathBuf, process::ExitCode};

use sylvops_daemon::{daemon, hook, runtime::RuntimePaths};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hook-daemon error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments == ["hook", "emit"] {
        hook::emit_from_environment()?;
    } else if let [command, directory] = arguments.as_slice()
        && command == "daemon"
    {
        let paths = RuntimePaths::discover(Some(&PathBuf::from(directory)))?;
        daemon::run(paths).await?;
    } else {
        return Err(io::Error::other("unsupported fixture arguments").into());
    }
    Ok(())
}
