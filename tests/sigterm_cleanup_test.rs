//! Verify SIGTERM/SIGINT cleanup: MCP server graceful exit triggers `kill_on_drop`
//! → frama-c child dies.
//!
//! See `docs/fixes/sigterm-handler-frama-c-orphan.md` for root cause analysis.
//!
//! Before main.rs fix:
//! - Rust 默认 SIGTERM → immediate exit → Drop chain 不跑 → kill_on_drop 不触发
//! - frama-c child 被 reparent 到 init/Relay，永远 idle 在 hrtimer_nanosleep
//!
//! After main.rs fix:
//! - tokio::select! 捕 SIGTERM → main 正常 return → Drop chain → kill_on_drop SIGKILL frama-c

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn workspace_path(rel: &str) -> PathBuf {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    PathBuf::from(crate_dir).join(rel)
}

/// True iff process exists AND not zombie. Zombie processes still have a `/proc/<pid>`
/// entry (until reaped) but are effectively dead — they don't consume CPU/memory.
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{}/status", pid)) {
        Ok(s) => {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("State:") {
                    let state = rest.split_whitespace().next().unwrap_or("?");
                    return state != "Z"; // Z = zombie (effectively dead)
                }
            }
            true
        }
        Err(_) => false,
    }
}

/// pgrep -P <parent_pid> → first child PID（找 MCP server 的 frama-c 子进程）
fn first_child_pid(parent_pid: u32) -> Option<u32> {
    let out = Command::new("pgrep")
        .arg("-P")
        .arg(parent_pid.to_string())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next()?.trim().parse::<u32>().ok()
}

/// `kill -TERM <pid>` — 用 shell 工具避免引入 libc/nix 仅为发信号。
fn sigterm(pid: u32) {
    let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
}

/// 等条件成立或 timeout（默认 3 秒）。
fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn sigterm_kills_frama_c_child() {
    let binary = workspace_path("target/release/frama-c-mcp-server");
    assert!(
        binary.exists(),
        "MCP binary missing: {}\nRun `cargo build --release` first.",
        binary.display()
    );
    let frama_c = std::env::var("FRAMA_C_BIN").unwrap_or_else(|_| "frama-c".into());
    let test_c = workspace_path("test/test_abs.c");
    assert!(test_c.exists(), "test C file missing: {}", test_c.display());

    // 1. spawn MCP server with stdio pipes
    let mut mcp = Command::new(&binary)
        .arg("--frama-c")
        .arg(&frama_c)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MCP server");
    let mcp_pid = mcp.id();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());

    eprintln!("[test] MCP server PID = {}", mcp_pid);

    // 2. JSON-RPC initialize + initialized notification + reload_project
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"sigterm-test","version":"0"}}}"#;
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    let reload = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"reload_project","arguments":{{"files":["{}"]}}}}}}"#,
        test_c.display()
    );

    writeln!(stdin, "{}", init).expect("write init");
    writeln!(stdin, "{}", initialized).expect("write initialized");
    writeln!(stdin, "{}", reload).expect("write reload");
    stdin.flush().ok();

    // 3. 读响应（init response + reload response）—— 简化：读 2 行
    for i in 0..2 {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).expect("read line");
        assert!(n > 0, "MCP closed stdout unexpectedly at response {}", i);
        eprintln!("[test] resp {}: {}", i, &line[..line.len().min(120)]);
    }

    // 4. 找 frama-c child PID
    // reload_project 内部 spawn 是 async，可能需要短暂等待
    let frama_pid = wait_until_some(|| first_child_pid(mcp_pid), Duration::from_secs(10))
        .expect("frama-c child not spawned within 10s after reload_project");
    eprintln!("[test] frama-c child PID = {}", frama_pid);
    assert!(process_alive(frama_pid), "frama-c should be alive immediately");

    // 5. SIGTERM MCP server（不是 SIGKILL — 验证 handler 工作）
    eprintln!("[test] sending SIGTERM to MCP {}", mcp_pid);
    sigterm(mcp_pid);

    // 6. MCP server 应该在 ~3 秒内退出
    //    （frama-c kill_on_drop 在 Drop chain 即刻触发，约 50ms；MCP 本身在
    //    shutdown_timeout(2s) 后退出 → 总 ≤3 秒。给 5 秒 buffer。）
    let mcp_died = wait_until(|| !process_alive(mcp_pid), Duration::from_secs(5));
    assert!(mcp_died, "MCP server {} still alive 5s after SIGTERM (zombie 也算死)", mcp_pid);
    eprintln!("[test] MCP server exited gracefully");

    // 7. frama-c child 应该早于 MCP 死（kill_on_drop 在 Drop chain 中 ~50ms）
    let child_died = wait_until(|| !process_alive(frama_pid), Duration::from_secs(3));
    if !child_died {
        // 残留时找出 PPID 验证 orphan 状态用于调试
        let ppid = std::fs::read_to_string(format!("/proc/{}/status", frama_pid))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("PPid:"))
                    .map(|l| l.split_whitespace().nth(1).unwrap_or("?").to_string())
            })
            .unwrap_or_else(|| "?".into());
        // 清理 orphan 防止测试残留
        let _ = Command::new("kill").arg("-9").arg(frama_pid.to_string()).status();
        panic!(
            "REGRESSION: frama-c child {} still alive 3s after MCP {} SIGTERM (orphan, PPID={}). \
             main.rs SIGTERM handler not working — kill_on_drop didn't fire.",
            frama_pid, mcp_pid, ppid
        );
    }
    eprintln!("[test] frama-c child {} cleaned up ✓", frama_pid);

    // wait() reaps zombie if any
    let _ = mcp.wait();
}

/// 等待闭包返回 Some(T) 或 timeout
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
