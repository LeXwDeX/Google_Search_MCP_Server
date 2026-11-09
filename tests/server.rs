//! End-to-end tests: spawn the real binary and compare its wire output with
//! fixtures captured from the TypeScript build (the IO parity contract).

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_google-search-mcp")
}

struct RunOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> RunOutput {
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            panic!("server did not exit within {timeout:?} (orphan)");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    RunOutput {
        code: status.code(),
        stdout: stdout_thread.join().expect("stdout reader"),
        stderr: stderr_thread.join().expect("stderr reader"),
    }
}

fn isolated_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gsmcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test cwd");
    dir
}

/// Remove inherited config so a developer's shell or repository `.env` cannot
/// change the expected wire output.
fn clear_config_env(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("GOOGLE_") || name == "LOG_LEVEL" {
            command.env_remove(&key);
        }
    }
}

fn configure_stdio(command: &mut Command) {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn isolated_command() -> Command {
    let mut command = Command::new(binary());
    clear_config_env(&mut command);
    command.current_dir(isolated_dir());
    configure_stdio(&mut command);
    command
}

fn command() -> Command {
    let mut command = isolated_command();
    command
        .env("GOOGLE_API_KEY", "dummy-key")
        .env("GOOGLE_CSE_ID", "dummy-cse")
        .env("LOG_LEVEL", "info");
    command
}

fn run_with_command(mut command: Command, input: &str) -> RunOutput {
    let mut child = command.spawn().expect("spawn server");
    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin.write_all(input.as_bytes()).expect("write input");
    }
    wait_with_timeout(child, Duration::from_secs(10))
}

fn run(input: &str) -> RunOutput {
    run_with_command(command(), input)
}

#[test]
fn modern_tools_list_matches_typescript_fixture() {
    let output = run(include_str!("fixtures/modern_tools_list.in"));
    assert_eq!(output.code, Some(0));
    assert_eq!(
        output.stdout,
        include_str!("fixtures/modern_tools_list.jsonl")
    );
}

#[test]
fn legacy_session_matches_typescript_fixture() {
    let output = run(include_str!("fixtures/legacy_session.in"));
    assert_eq!(output.code, Some(0));
    assert_eq!(output.stdout, include_str!("fixtures/legacy_session.jsonl"));
}

#[test]
fn unsupported_version_matches_typescript_fixture() {
    let output = run(include_str!("fixtures/unsupported_version.in"));
    assert_eq!(output.code, Some(0));
    assert_eq!(
        output.stdout,
        include_str!("fixtures/unsupported_version.jsonl")
    );
}

#[test]
fn missing_client_capabilities_matches_typescript_fixture() {
    let output = run(include_str!("fixtures/missing_client_capabilities.in"));
    assert_eq!(output.code, Some(0));
    assert_eq!(
        output.stdout,
        include_str!("fixtures/missing_client_capabilities.jsonl")
    );
}

#[test]
fn invalid_tools_call_matches_typescript_fixture() {
    let output = run(include_str!("fixtures/invalid_tools_call.in"));
    assert_eq!(output.code, Some(0));
    assert_eq!(
        output.stdout,
        include_str!("fixtures/invalid_tools_call.jsonl")
    );
}

#[test]
fn config_errors_are_fatal_and_exact() {
    let tmp = std::env::temp_dir().join(format!("gsmcp-config-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut command = isolated_command();
    command.current_dir(&tmp).stdin(Stdio::null());
    let child = command.spawn().expect("spawn server");
    let output = wait_with_timeout(child, Duration::from_secs(10));
    assert_eq!(output.code, Some(1));
    assert_eq!(
        output.stderr,
        "Invalid configuration:\n  - googleApiKey: GOOGLE_API_KEY environment variable is required\n  - googleCseId: GOOGLE_CSE_ID environment variable is required\n"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn dotenv_file_is_loaded() {
    let tmp = std::env::temp_dir().join(format!("gsmcp-dotenv-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join(".env"),
        "GOOGLE_API_KEY=from-dotenv\nGOOGLE_CSE_ID=from-dotenv-cx\n",
    )
    .unwrap();

    let mut command = isolated_command();
    command.current_dir(&tmp);
    let output = run_with_command(
        command,
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n",
    );
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.contains("\"protocolVersion\":\"2025-06-18\""));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn clean_disconnect_exits_zero() {
    let mut child = command().spawn().expect("spawn server");
    drop(child.stdin.take());
    let started = Instant::now();
    let output = wait_with_timeout(child, Duration::from_secs(6));
    assert_eq!(output.code, Some(0));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(output.stderr.contains("\"msg\":\"shutting down\""));
}

/// Regression for the orphaned-process / exception-storm bug: a dead stderr
/// must trigger shutdown even while stdin stays open.
#[test]
fn dead_stderr_triggers_shutdown_with_stdin_open() {
    let mut child = command().spawn().expect("spawn server");
    // Let the startup log land before closing the read end.
    std::thread::sleep(Duration::from_millis(150));
    // Close the read end of stderr; the server's next log write hits EPIPE.
    drop(child.stderr.take());

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });

    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"google_web_search\",\"arguments\":{\"query\":\"\"}}}\n",
        )
        .unwrap();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > Duration::from_secs(6) {
            let _ = child.kill();
            panic!("server stayed alive with dead stderr (orphan regression)");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Keep stdin open until the process has exited; dropping it early would
    // mask the EPIPE path behind the normal stdin-EOF shutdown.
    drop(stdin);
    let _ = stdout_thread.join();
    assert_eq!(status.code(), Some(0));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// A dead stdout (client crashed without closing sockets) must also shut down.
#[test]
fn dead_stdout_triggers_shutdown_with_stdin_open() {
    let mut child = command().spawn().expect("spawn server");
    // Let the startup log land on stderr before closing stdout.
    std::thread::sleep(Duration::from_millis(150));
    drop(child.stdout.take());

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n",
        )
        .unwrap();

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > Duration::from_secs(6) {
            let _ = child.kill();
            panic!("server stayed alive with dead stdout");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    let _ = stderr_thread.join();
    assert_eq!(status.code(), Some(0));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn listens_cancel_and_teardown_match_sdk() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"subscriptions/listen\",\"params\":{\"notifications\":{\"toolsListChanged\":true},\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":7}}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    let lines: Vec<&str> = output.stdout.lines().collect();
    assert_eq!(lines.len(), 1, "only the acknowledgement is expected");
    assert!(lines[0].contains("notifications/subscriptions/acknowledged"));
}

#[test]
fn malformed_lines_are_dropped_silently() {
    let input = concat!(
        "{not json\n",
        "\n",
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":1.5,\"method\":\"ping\"}\n",
        "[]\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.is_empty());
}

#[test]
fn discover_probe_falls_back_to_legacy_handshake() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    let lines: Vec<&str> = output.stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("\"supportedVersions\":[\"2026-07-28\"]"));
    assert!(lines[1].contains("\"protocolVersion\":\"2025-06-18\""));
    assert!(lines[2].contains("\"result\":{}"));
}

#[test]
fn modern_ping_is_method_not_found() {
    let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n";
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output
        .stdout
        .contains("\"error\":{\"code\":-32601,\"message\":\"Method not found\"}"));
}

#[test]
fn pinned_modern_connection_rejects_envelope_less_requests() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    let lines: Vec<&str> = output.stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"result\""));
    assert!(lines[1].contains("Request is missing the required _meta envelope"));
    assert!(lines[1].contains("\"code\":-32602"));
}

#[test]
fn nested_envelope_shape_is_rejected() {
    let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{\"sampling\":{\"tools\":5}}}}}\n";
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.contains("\"code\":-32602"));
    assert!(output.stdout.contains(
        "io.modelcontextprotocol/clientCapabilities.sampling.tools: Invalid input: expected record, received number"
    ));
    assert!(output.stdout.contains(
        "\"data\":{\"envelope\":{\"key\":\"io.modelcontextprotocol/clientCapabilities.sampling.tools\""
    ));
}

#[test]
fn arguments_null_is_invalid_params() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"google_web_search\",\"arguments\":null}}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.contains("\"code\":-32602"));
    assert!(output.stdout.contains("expected record, received null"));
    assert!(!output.stdout.contains("\"isError\":true"));
}

#[test]
fn legacy_unknown_version_negotiates_down() {
    let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"1999-01-01\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n";
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.contains("\"protocolVersion\":\"2025-11-25\""));
}

#[test]
fn pinned_modern_checks_method_before_envelope() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\",\"params\":{}}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output
        .stdout
        .contains("\"id\":2,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}")
        || output
            .stdout
            .contains("\"error\":{\"code\":-32601,\"message\":\"Method not found\"},\"jsonrpc\":\"2.0\",\"id\":2}"));
}

#[test]
fn pinned_modern_envelope_errors_join_messages_without_data() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\"}}}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.contains(
        "Invalid _meta envelope for protocol revision 2026-07-28: Invalid input: expected object, received undefined"
    ));
    assert!(!output.stdout.contains("\"data\":{\"envelope\""));
}

#[test]
fn listen_pinned_uses_router_envelope_messages() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{\"_meta\":{\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"io.modelcontextprotocol/clientCapabilities\":{}}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"subscriptions/listen\",\"params\":{\"notifications\":{}}}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.contains(
        "Invalid _meta envelope: _meta: the per-request envelope is required on protocol revision 2026-07-28"
    ));
}

#[test]
fn malformed_transport_meta_is_dropped() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{\"_meta\":5}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    let lines: Vec<&str> = output.stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"id\":1"));
    assert!(lines[1].contains("\"id\":3"));
}

#[test]
fn unsafe_numeric_ids_are_dropped() {
    let input = "{\"jsonrpc\":\"2.0\",\"id\":9007199254740993,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n";
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.is_empty());
}

#[test]
fn legacy_task_validation() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"google_web_search\",\"arguments\":{\"query\":\"\"},\"task\":5}}\n",
    );
    let output = run(input);
    assert_eq!(output.code, Some(0));
    assert!(output.stdout.contains("\"code\":-32602"));
    assert!(output.stdout.contains("task"));
    assert!(output
        .stdout
        .contains("Invalid input: expected object, received number"));
}

/// Regression for the blocking-stderr liveness bug: a full, undrained stderr
/// pipe must not stop stdin/stdout/signal handling.
#[test]
fn full_stderr_does_not_block_runtime() {
    let mut child = command().spawn().expect("spawn server");
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");

    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n",
        )
        .unwrap();
    for id in 1..700 {
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"google_web_search\",\"arguments\":{{\"query\":\"\"}}}}}}\n"
        );
        stdin.write_all(line.as_bytes()).unwrap();
    }
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9000,\"method\":\"ping\"}\n")
        .unwrap();
    stdin.flush().unwrap();

    let stdout_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });

    // Give the server time to fill the 64 KiB stderr pipe, then verify a
    // later request is still answered.
    std::thread::sleep(Duration::from_millis(800));
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9001,\"method\":\"ping\"}\n")
        .unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));

    drop(stdin);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if started.elapsed() > Duration::from_secs(6) {
            let _ = child.kill();
            panic!("server blocked by a full stderr pipe");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout_thread.join().unwrap();
    assert_eq!(status.code(), Some(0));
    assert!(stdout.contains("\"id\":9000"), "ping 9000 missing");
    assert!(stdout.contains("\"id\":9001"), "ping 9001 missing");
}

/// Regression for responses being dropped when stdin closes immediately after
/// a batch of synchronous tool calls.
#[test]
fn stdin_eof_keeps_inline_responses() {
    let input = include_str!("fixtures/legacy_session.in");
    for _ in 0..30 {
        let output = run(input);
        assert_eq!(output.code, Some(0));
        assert_eq!(
            output.stdout.lines().count(),
            4,
            "a response was dropped on stdin EOF"
        );
    }
}
