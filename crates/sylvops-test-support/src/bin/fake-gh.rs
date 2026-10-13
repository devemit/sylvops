use std::{env, process::ExitCode, thread, time::Duration};

fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let host = arguments
        .windows(2)
        .find(|pair| pair[0] == "--hostname")
        .map_or("github.com", |pair| pair[1].as_str());
    if arguments.iter().any(|argument| argument == "graphql") {
        return graphql(host, &arguments);
    }
    match host {
        "auth.example.com" => fail("not logged into auth.example.com ghp_PRIVATE_SENTINEL"),
        "network.example.com" => fail("could not resolve host"),
        "rate.example.com" => fail("API rate limit exceeded"),
        "unavailable.example.com" => fail("unsupported hostname"),
        "oversized.example.com" => {
            print!("{}", "x".repeat(800 * 1024));
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

fn graphql(host: &str, arguments: &[String]) -> ExitCode {
    match host {
        "auth.example.com" => return fail("HTTP 401 authentication required"),
        "permission.example.com" => return fail("HTTP 403 forbidden"),
        "network.example.com" => return fail("could not resolve host"),
        "rate.example.com" => return fail("API rate limit exceeded (429)"),
        "timeout.example.com" => {
            thread::sleep(Duration::from_secs(10));
            return ExitCode::SUCCESS;
        }
        "oversized.example.com" => {
            print!("{}", "x".repeat(800 * 1024));
            return ExitCode::SUCCESS;
        }
        "malformed.example.com" => {
            print!("{{broken");
            return ExitCode::SUCCESS;
        }
        "nested.example.com" => {
            print!("{}", "[".repeat(200));
            print!("{}", "]".repeat(200));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    let variable = |name: &str| {
        arguments
            .iter()
            .find_map(|argument| argument.strip_prefix(&format!("{name}=")))
    };
    if variable("number") == Some("404") {
        println!(
            "{}",
            serde_json::json!({"data":{"repository":{"issueOrPullRequest":null}}})
        );
        return ExitCode::SUCCESS;
    }
    if let Some(number) = variable("number") {
        let item = if number == "7" {
            pull_request(true)
        } else {
            issue(true)
        };
        println!(
            "{}",
            serde_json::json!({"data":{"repository":{"issueOrPullRequest":item}}})
        );
        return ExitCode::SUCCESS;
    }

    let second_page = variable("after").is_some();
    let pull_requests = variable("search").is_some_and(|search| search.contains("is:pr"));
    let nodes = if second_page {
        Vec::new()
    } else if pull_requests {
        vec![pull_request(false)]
    } else {
        vec![issue(false)]
    };
    println!(
        "{}",
        serde_json::json!({
            "data":{"search":{
                "nodes":nodes,
                "pageInfo":{
                    "hasNextPage":!second_page,
                    "endCursor":if second_page { serde_json::Value::Null } else { serde_json::json!("cursor-one") }
                }
            }}
        })
    );
    ExitCode::SUCCESS
}

fn issue(detail: bool) -> serde_json::Value {
    let mut value = serde_json::json!({
        "__typename":"Issue",
        "number":42,
        "state":"OPEN",
        "title":"unsafe\u{1b}[31m issue",
        "author":{"login":"octocat"},
        "labels":{"nodes":[{"name":"bug\nlabel"}]},
        "assignees":{"nodes":[{"login":"hubot"}]},
        "createdAt":"2026-10-01T00:00:00Z",
        "updatedAt":"2026-10-02T00:00:00Z"
    });
    if detail {
        value["body"] = serde_json::json!("safe\nbody\u{1b}[31m");
        value["comments"] = comments();
    }
    value
}

fn pull_request(detail: bool) -> serde_json::Value {
    let mut value = serde_json::json!({
        "__typename":"PullRequest",
        "number":7,
        "state":"OPEN",
        "merged":false,
        "title":"Add bounded GitHub browsing",
        "author":{"login":"octocat"},
        "labels":{"nodes":[{"name":"enhancement"}]},
        "assignees":{"nodes":[]},
        "createdAt":"2026-10-01T00:00:00Z",
        "updatedAt":"2026-10-03T00:00:00Z",
        "headRefName":"feature/github",
        "baseRefName":"main",
        "isDraft":false,
        "reviewDecision":"APPROVED",
        "changedFiles":3,
        "additions":120,
        "deletions":10,
        "statusCheckRollup":{"state":"SUCCESS","contexts":{"totalCount":4}}
    });
    if detail {
        value["body"] = serde_json::json!("Pull Request body");
        value["comments"] = comments();
    }
    value
}

fn comments() -> serde_json::Value {
    serde_json::json!({
        "nodes":[{
            "author":{"login":"reviewer"},
            "body":"looks good\u{1b}[0m",
            "createdAt":"2026-10-03T01:00:00Z",
            "updatedAt":"2026-10-03T01:00:00Z"
        }],
        "pageInfo":{"hasNextPage":true,"endCursor":"comment-cursor"}
    })
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}
