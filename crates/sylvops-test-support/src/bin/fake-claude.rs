//! Deterministic Claude Code CLI stand-in for discovery and health tests.

use std::{env, fs, process::ExitCode};

fn main() -> ExitCode {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if arguments == ["--version"] {
        println!("{} (Claude Code)", fixture("version", "2.1.145"));
        return ExitCode::SUCCESS;
    }
    if arguments == ["auth", "status"] {
        return match fixture("auth", "claude.ai").as_str() {
            "logged-out" => {
                println!(r#"{{"loggedIn":false,"authMethod":"none"}}"#);
                ExitCode::FAILURE
            }
            "console" => {
                println!(
                    r#"{{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}}"#
                );
                ExitCode::SUCCESS
            }
            "oauth" => {
                println!(
                    r#"{{"loggedIn":true,"authMethod":"oauth_token","apiProvider":"firstParty"}}"#
                );
                ExitCode::SUCCESS
            }
            "failed-supported" => {
                println!(
                    r#"{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}}"#
                );
                ExitCode::FAILURE
            }
            "unsupported" => {
                println!(
                    r#"{{"loggedIn":true,"authMethod":"third_party","apiProvider":"thirdParty"}}"#
                );
                ExitCode::SUCCESS
            }
            "malformed" => {
                println!("not-json PRIVATE_AUTH_DETAIL");
                ExitCode::SUCCESS
            }
            "oversized" => {
                println!("{}", "x".repeat(65 * 1024));
                ExitCode::SUCCESS
            }
            "hang" => {
                std::thread::sleep(std::time::Duration::from_secs(30));
                ExitCode::SUCCESS
            }
            _ => {
                println!(
                    r#"{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"private@example.invalid"}}"#
                );
                ExitCode::SUCCESS
            }
        };
    }
    eprintln!("unsupported fake Claude command");
    ExitCode::FAILURE
}

fn fixture(name: &str, default: &str) -> String {
    let path = env::current_exe().ok().and_then(|path| {
        path.parent()
            .map(|parent| parent.join(format!("fake-claude-{name}")))
    });
    path.and_then(|path| fs::read_to_string(path).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}
