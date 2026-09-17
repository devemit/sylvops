use std::{
    env, fs,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    thread,
    time::Duration,
};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("fake-agent error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<u8> {
    let mut arguments = env::args_os().skip(1);
    let scenario = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "interactive".into());

    match scenario.as_str() {
        "interactive" => interactive(),
        "burst" => {
            let count = arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(128 * 1024);
            let output = vec![b'x'; count];
            io::stdout().write_all(&output)?;
            io::stdout().flush()?;
            Ok(0)
        }
        "spawn-child" => {
            let heartbeat = arguments.next().map(PathBuf::from).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing heartbeat path")
            })?;
            spawn_child(&heartbeat)
        }
        "heartbeat" => {
            let heartbeat = arguments.next().map(PathBuf::from).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing heartbeat path")
            })?;
            heartbeat_forever(&heartbeat)
        }
        "exit" => {
            let code = arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(0);
            Ok(code)
        }
        "cwd" => {
            println!("CWD={}", env::current_dir()?.display());
            io::stdout().flush()?;
            Ok(0)
        }
        unknown => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown scenario {unknown}"),
        )),
    }
}

fn interactive() -> io::Result<u8> {
    println!("FAKE_AGENT_READY");
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        let line = line?;
        println!("ECHO:{line}");
        io::stdout().flush()?;
        if line == "exit" {
            return Ok(0);
        }
    }
    Ok(0)
}

fn spawn_child(heartbeat: &Path) -> io::Result<u8> {
    let executable = env::current_exe()?;
    let child = Command::new(executable)
        .arg("heartbeat")
        .arg(heartbeat)
        .spawn()?;
    println!("CHILD_PID={}", child.id());
    io::stdout().flush()?;
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

fn heartbeat_forever(path: &Path) -> io::Result<u8> {
    let mut counter = 0_u64;
    loop {
        counter = counter.saturating_add(1);
        fs::write(path, counter.to_string())?;
        thread::sleep(Duration::from_millis(50));
    }
}
