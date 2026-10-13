use std::{env, process::ExitCode, thread, time::Duration};

fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let host = arguments
        .windows(2)
        .find(|pair| pair[0] == "--hostname")
        .map_or("github.com", |pair| pair[1].as_str());
    match host {
        "auth.example.com" => fail("not logged into auth.example.com ghp_PRIVATE_SENTINEL"),
        "network.example.com" => fail("could not resolve host"),
        "rate.example.com" => fail("API rate limit exceeded"),
        "unavailable.example.com" => fail("unsupported hostname"),
        "oversized.example.com" => {
            print!("{}", "x".repeat(70 * 1024));
            ExitCode::SUCCESS
        }
        "timeout.example.com" => {
            thread::sleep(Duration::from_secs(10));
            ExitCode::SUCCESS
        }
        _ if arguments
            .iter()
            .any(|argument| argument.contains("denied/repository")) =>
        {
            fail("HTTP 403 forbidden")
        }
        _ => {
            println!("fake-user");
            ExitCode::SUCCESS
        }
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}
