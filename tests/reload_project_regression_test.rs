//! Regression tests for two `reload_project` bugs fixed in
//! `docs/fixes/reload-project-deser-and-cursor.md`:
//!
//! 1. **Stringified array deser**: Claude Code MCP 客户端偶发把 array 参数
//!    序列化成 stringified JSON (`files="[\"...\"]"`)，server 不接受报
//!    `invalid type: string, expected sequence`。修：`#[serde(default,
//!    deserialize_with="deserialize_vec_or_string")]`。
//!
//! 2. **fetchFunctions cursor 漏 reloadFunctions**：lazy spawn 后首次
//!    `reload_project` 返回 `functions: []`（cursor 在 "now" 无 delta）。
//!    依赖 agent 后续调 list_functions 兜底，违反 API contract。
//!    修：reload_project 在 fetch_all 前先 `kernel.ast.reloadFunctions`。
//!
//! 两测试都用 raw stdio JSON-RPC（绕开 rmcp client 的高级 serialize 层，
//! 直接控制 wire-level payload）。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn workspace_path(rel: &str) -> PathBuf {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    PathBuf::from(crate_dir).join(rel)
}

struct McpHandle {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpHandle {
    /// Spawn MCP server with `cwd` set to allow relative-path tests.
    fn spawn_in(cwd: &std::path::Path) -> Self {
        let binary = workspace_path("target/release/frama-c-mcp-server");
        assert!(binary.exists(), "Run `cargo build --release` first.");
        let frama_c = std::env::var("FRAMA_C_BIN").unwrap_or_else(|_| "frama-c".into());
        let mut child = Command::new(&binary)
            .arg("--frama-c").arg(&frama_c)
            .current_dir(cwd)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().expect("spawn MCP server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut h = Self { _child: child, stdin, stdout };
        h.initialize();
        h
    }

    fn initialize(&mut self) {
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"reload-test","version":"0"}}}"#;
        let notify = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        writeln!(self.stdin, "{}", init).unwrap();
        writeln!(self.stdin, "{}", notify).unwrap();
        self.stdin.flush().ok();
        let mut buf = String::new();
        self.stdout.read_line(&mut buf).unwrap();
    }

    /// Send a tool call with **raw** arguments JSON (caller controls exact wire format
    /// to test stringified arrays, etc.).
    fn call_tool_raw(&mut self, name: &str, args_json: &str) -> serde_json::Value {
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"{}","arguments":{}}}}}"#,
            name, args_json
        );
        writeln!(self.stdin, "{}", req).unwrap();
        self.stdin.flush().ok();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("parse JSON")
    }
}

impl Drop for McpHandle {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

/// 从 tool response 中抽 result.content[0].text 字段。
fn extract_result_text(resp: &serde_json::Value) -> String {
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|i| i.get("text"))
        .and_then(|t| t.as_str())
        .map(String::from)
        .unwrap_or_default()
}

// ─────────────────────────────────────────────────────────────────────────
// Bug 1: stringified array deser
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn stringified_files_array_is_accepted() {
    let test_c = workspace_path("test/test_abs.c");
    assert!(test_c.exists());
    // 用 binary 自己的 dir 当 cwd（不影响绝对路径测试）
    let mut mcp = McpHandle::spawn_in(std::path::Path::new("/tmp"));

    // 关键 wire payload：files 是 string 类型，内容是 JSON array
    // 这是 Claude Code MCP 客户端偶发序列化形式
    let stringified_args = format!(
        r#"{{"files": "[\"{}\"]"}}"#,
        test_c.display()
    );
    let resp = mcp.call_tool_raw("reload_project", &stringified_args);
    eprintln!("[test1] response: {}", serde_json::to_string_pretty(&resp).unwrap());

    // 必须成功（不是 deser error）
    assert!(
        resp.get("error").is_none(),
        "stringified array 应被 deserialize_vec_or_string 接受, got: {:?}",
        resp.get("error")
    );

    let body = extract_result_text(&resp);
    assert!(
        body.contains(&test_c.display().to_string()),
        "response 应回显文件路径, got: {}",
        body
    );
    // 修 #2 后这里也应该有 functions 非空（顺便测）
    assert!(
        body.contains("\"name\": \"abs_val\""),
        "首次 spawn 后 functions 应非空（cursor fix）, got: {}",
        body
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Bug 2: fetchFunctions cursor — 首次 spawn 返回 functions 非空
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn first_reload_returns_non_empty_functions() {
    let test_c = workspace_path("test/test_abs.c");
    assert!(test_c.exists());
    let mut mcp = McpHandle::spawn_in(std::path::Path::new("/tmp"));

    // 普通 JSON array 形式（**第一次** reload，触发 lazy spawn）
    let args = format!(r#"{{"files": ["{}"]}}"#, test_c.display());
    let resp = mcp.call_tool_raw("reload_project", &args);
    eprintln!("[test2] response: {}", serde_json::to_string_pretty(&resp).unwrap());

    assert!(resp.get("error").is_none(), "no error expected");

    let body = extract_result_text(&resp);
    // 首次 spawn 后应能看到 functions（修 #2 前是空数组）
    // test_abs.c 含 `abs_val` / `square` / `main` 函数
    assert!(
        body.contains("\"name\": \"abs_val\""),
        "首次 reload_project 应返回 functions（修 #2 前 cursor 在 'now' 返回空）, got: {}",
        body
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Bug 1 + 2 联合：stringified array + 首次 spawn 都正常
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn stringified_array_first_spawn_returns_functions() {
    let test_c = workspace_path("test/test_abs.c");
    assert!(test_c.exists());
    let mut mcp = McpHandle::spawn_in(std::path::Path::new("/tmp"));

    let stringified_args = format!(
        r#"{{"files": "[\"{}\"]"}}"#,
        test_c.display()
    );
    let resp = mcp.call_tool_raw("reload_project", &stringified_args);
    eprintln!("[test3] response: {}", serde_json::to_string_pretty(&resp).unwrap());

    assert!(resp.get("error").is_none());
    let body = extract_result_text(&resp);
    assert!(body.contains("\"name\": \"abs_val\""), "两 fix 串联必须 work");
}

// ─────────────────────────────────────────────────────────────────────────
// Gap 1 follow-up: 其他 tool params 也接受 stringified array
// （RunWpParams.functions / stop_at + StoreProjectStateParams.{source_files,
// verification_order} 都加了同款 helper，防 Bug 1 类报告在其他 tool 复发）
// ─────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────
// Gap 2 (PR #108 follow-up): in-place reload 后 functions 反映新文件内容
// （reloadFunctions 移到主函数后，分支 1 in-place 路径仍正确刷新 cursor）
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn in_place_reload_refreshes_functions_to_new_file() {
    let test_abs = workspace_path("test/test_abs.c");
    let factorial = workspace_path("test/factorial.c");
    assert!(test_abs.exists());
    assert!(factorial.exists());

    let mut mcp = McpHandle::spawn_in(std::path::Path::new("/tmp"));

    // 1st reload (分支 3: 首次 spawn): test_abs.c 含 abs_val/square/main
    let r1 = mcp.call_tool_raw(
        "reload_project",
        &format!(r#"{{"files": ["{}"]}}"#, test_abs.display()),
    );
    let body1 = extract_result_text(&r1);
    eprintln!("[in-place] reload #1 (spawn): {}", &body1[..body1.len().min(200)]);
    assert!(body1.contains("\"name\": \"abs_val\""), "首次 spawn 含 abs_val");
    assert!(!body1.contains("\"name\": \"factorial\""), "首次 spawn 不该含 factorial");

    // 2nd reload (分支 1: in-place reload, same rte=false): factorial.c 含 factorial 函数
    let r2 = mcp.call_tool_raw(
        "reload_project",
        &format!(r#"{{"files": ["{}"]}}"#, factorial.display()),
    );
    let body2 = extract_result_text(&r2);
    eprintln!("[in-place] reload #2 (in-place): {}", &body2[..body2.len().min(200)]);

    // 关键断言：in-place reload 后必须看到 factorial 函数（新文件内容）
    // 不能看到 abs_val（旧文件不再 loaded）
    // 如果 reloadFunctions 没在 in-place 路径生效，cursor 不重置 → 返回旧的或空
    assert!(
        body2.contains("\"name\": \"factorial\""),
        "in-place reload 后 functions 应反映 factorial.c (Gap 2 修保证 main reloadFunctions 覆盖此路径), got: {}",
        body2
    );
    assert!(
        !body2.contains("\"name\": \"abs_val\""),
        "in-place reload 后不应残留 abs_val (旧文件), got: {}",
        body2
    );
}

#[test]
fn run_wp_functions_accepts_stringified_array() {
    let test_c = workspace_path("test/test_abs.c");
    let mut mcp = McpHandle::spawn_in(std::path::Path::new("/tmp"));

    // 先 reload 让 project 就位
    let reload_args = format!(r#"{{"files": ["{}"]}}"#, test_c.display());
    let r = mcp.call_tool_raw("reload_project", &reload_args);
    assert!(r.get("error").is_none(), "reload pre-step failed");

    // 关键：run_wp(functions=...) 传 stringified array
    let stringified_args = r#"{"functions": "[\"abs_val\"]"}"#;
    let resp = mcp.call_tool_raw("run_wp", stringified_args);
    eprintln!("[test4] response: {}", serde_json::to_string_pretty(&resp).unwrap());

    // 必须**不是** deser error（业务 error 如"no annotations"是 OK 的）
    if let Some(err) = resp.get("error") {
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
        assert!(
            !msg.contains("invalid type") && !msg.contains("expected a sequence"),
            "stringified functions array 应被 helper 接受，不应 deser error: {}",
            msg
        );
    }
    // 不 assert success body（abs_val 没 annotation，run_wp 可能返业务错；
    // 只关心 deser 层不爆）
}

