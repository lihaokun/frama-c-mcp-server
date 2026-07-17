//! Regression tests for lazy spawn (Issue #95 / PR #96).
//!
//! 这里 3 个测试是 PR #96 review 要求的最小回归覆盖：
//! 1. **NoProjectLoaded 错误结构**：未 reload 就调 main tool，应返回含
//!    `suggestion.tool=reload_project` 的结构化 JSON。
//! 2. **SandboxNotFound 错误结构**：调 sandbox 工具用不存在的 sandbox_name，
//!    应返回含 `existing_sandboxes: []` 的结构化 JSON。
//! 3. **in-place reload 保留 PID**：reload 不同 files 但 same rte，frama-c child
//!    PID 应保持不变（in-place 路径，不 respawn）。
//!
//! 这些测试用 raw stdio JSON-RPC 直接驱动 binary，避免 rmcp client 的高级封装
//! 屏蔽底层进程细节。Pattern 复用 `sigterm_cleanup_test.rs`。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

// ─── Helpers ──────────────────────────────────────────────────────────────

fn workspace_path(rel: &str) -> PathBuf {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    PathBuf::from(crate_dir).join(rel)
}

fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{}/status", pid)) {
        Ok(s) => {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("State:") {
                    return rest.split_whitespace().next().unwrap_or("?") != "Z";
                }
            }
            true
        }
        Err(_) => false,
    }
}

fn first_child_pid(parent_pid: u32) -> Option<u32> {
    let out = Command::new("pgrep").arg("-P").arg(parent_pid.to_string()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

fn wait_until_some<T, F: FnMut() -> Option<T>>(mut f: F, timeout: Duration) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct McpHandle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pid: u32,
}

impl McpHandle {
    fn spawn() -> Self {
        let binary = workspace_path("target/release/frama-c-mcp-server");
        assert!(
            binary.exists(),
            "MCP binary missing: {}\nRun `cargo build --release` first.",
            binary.display()
        );
        let frama_c = std::env::var("FRAMA_C_BIN").unwrap_or_else(|_| "frama-c".into());
        let mut child = Command::new(&binary)
            .arg("--frama-c")
            .arg(&frama_c)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn MCP server");
        let pid = child.id();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut h = McpHandle { child, stdin, stdout, pid };
        h.initialize();
        h
    }

    fn initialize(&mut self) {
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"lazy-spawn-test","version":"0"}}}"#;
        let notify = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        writeln!(self.stdin, "{}", init).unwrap();
        writeln!(self.stdin, "{}", notify).unwrap();
        self.stdin.flush().ok();
        // 读 init response（弃用）
        let mut buf = String::new();
        self.stdout.read_line(&mut buf).unwrap();
    }

    /// 调一个 tool，返回 JSON response（包含 result 或 error）
    fn call_tool(&mut self, name: &str, args_json: &str) -> serde_json::Value {
        // 用递增 id
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(2);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"{}","arguments":{}}}}}"#,
            id, name, args_json
        );
        writeln!(self.stdin, "{}", req).unwrap();
        self.stdin.flush().ok();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("parse response")
    }
}

impl Drop for McpHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ─── Test 1: NoProjectLoaded ──────────────────────────────────────────────

#[test]
fn no_project_loaded_returns_structured_error() {
    let mut mcp = McpHandle::spawn();
    // 不调 reload_project，直接调 list_functions（需要 require_client）
    let resp = mcp.call_tool("list_functions", "{}");
    eprintln!("[test] response: {}", serde_json::to_string_pretty(&resp).unwrap());

    // 应该 isError=true（rmcp 把 McpError 包成 tool_use_error）
    // 或者 .error 字段（JSON-RPC level error）
    // 看实际结构 — McpError::invalid_params 走 JSON-RPC error path
    let err_msg = if let Some(e) = resp.get("error") {
        e.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string()
    } else if let Some(r) = resp.get("result") {
        let content = r.get("content").and_then(|c| c.as_array()).and_then(|a| a.first());
        content.and_then(|c| c.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_string()
    } else {
        panic!("response has neither error nor result: {:?}", resp);
    };

    // err_msg 应该是 NoProjectLoaded 错误 JSON
    eprintln!("[test] err_msg: {}", err_msg);
    assert!(
        err_msg.contains("NoProjectLoaded"),
        "err 应含 NoProjectLoaded: {}", err_msg
    );
    assert!(
        err_msg.contains("reload_project"),
        "err 应含 suggestion.tool=reload_project: {}", err_msg
    );
    assert!(
        err_msg.contains("args_example"),
        "err 应含 args_example for follow-up: {}", err_msg
    );
}

// ─── Test 2: SandboxNotFound ──────────────────────────────────────────────

#[test]
fn sandbox_not_found_returns_structured_error_with_existing_list() {
    let test_c = workspace_path("test/test_abs.c");
    assert!(test_c.exists());
    let mut mcp = McpHandle::spawn();

    // reload_project 让 main client 就位
    let reload_args = format!(r#"{{"files":["{}"]}}"#, test_c.display());
    let r = mcp.call_tool("reload_project", &reload_args);
    assert!(r.get("error").is_none(), "reload_project failed: {:?}", r);

    // 调 add_annotation_sandbox 用不存在的 sandbox（带 `:` 分隔符的 function 名进 sandbox 路径）
    // function: "nonexistent_exp:abs" — require_sandbox 应失败
    let bogus_args = r#"{"function":"nonexistent_exp:abs","kind":"requires","acsl":"true"}"#;
    let resp = mcp.call_tool("add_annotation_sandbox", bogus_args);
    eprintln!("[test] response: {}", serde_json::to_string_pretty(&resp).unwrap());

    let err_msg = if let Some(e) = resp.get("error") {
        e.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string()
    } else if let Some(r) = resp.get("result") {
        let content = r.get("content").and_then(|c| c.as_array()).and_then(|a| a.first());
        content.and_then(|c| c.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_string()
    } else {
        panic!("response has neither error nor result: {:?}", resp);
    };

    eprintln!("[test] err_msg: {}", err_msg);
    assert!(
        err_msg.contains("SandboxNotFound"),
        "err 应含 SandboxNotFound: {}", err_msg
    );
    assert!(
        err_msg.contains("existing_sandboxes"),
        "err 应含 existing_sandboxes 字段: {}", err_msg
    );
    assert!(
        err_msg.contains("create_sandbox"),
        "err 应含 suggestion.tool=create_sandbox: {}", err_msg
    );
}

// ─── Test 3: in-place reload 保留 frama-c PID ──────────────────────────────

#[test]
fn in_place_reload_preserves_frama_c_pid() {
    let test_c_a = workspace_path("test/test_abs.c");
    let test_c_b = workspace_path("test/factorial.c");
    assert!(test_c_a.exists());
    assert!(test_c_b.exists());

    let mut mcp = McpHandle::spawn();
    let mcp_pid = mcp.pid;

    // reload A
    let args_a = format!(r#"{{"files":["{}"],"rte":false}}"#, test_c_a.display());
    let r = mcp.call_tool("reload_project", &args_a);
    assert!(r.get("error").is_none(), "first reload failed: {:?}", r);

    // 等 frama-c child 出生
    let frama_pid_1 = wait_until_some(|| first_child_pid(mcp_pid), Duration::from_secs(10))
        .expect("frama-c child not spawned");
    eprintln!("[test] reload A: frama-c PID = {}", frama_pid_1);
    assert!(process_alive(frama_pid_1));

    // reload B（同 rte=false） — 应 in-place，不 respawn
    let args_b = format!(r#"{{"files":["{}"],"rte":false}}"#, test_c_b.display());
    let r = mcp.call_tool("reload_project", &args_b);
    assert!(r.get("error").is_none(), "second reload failed: {:?}", r);

    // 给点时间确认 PID 稳定（不是 spawn + kill 旧的）
    std::thread::sleep(Duration::from_millis(500));

    let frama_pid_2 = first_child_pid(mcp_pid).expect("frama-c child gone after in-place reload");
    eprintln!("[test] reload B: frama-c PID = {}", frama_pid_2);

    assert_eq!(
        frama_pid_1, frama_pid_2,
        "in-place reload (same rte) should preserve frama-c PID; \
         got A={} B={} (probably 走了 respawn 路径)",
        frama_pid_1, frama_pid_2
    );
    assert!(process_alive(frama_pid_1), "frama-c shouldn't have died");
}
