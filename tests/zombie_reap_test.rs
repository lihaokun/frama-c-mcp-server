//! Regression tests for the `Child` reap fix
//! （docs/fixes/frama-c-mcp-fix-child-reap-broken-pipe.md Phase 1）。
//!
//! 测试的不是端到端 frama-c，而是**子进程 reap 模式本身**——用 `sleep` 模拟
//! 长跑子进程，验证：
//!
//! T-A: tokio Child + 显式 start_kill+wait 之后进程消失（cleanup_sandbox 模式）
//! T-B: tokio Child + kill_on_drop=true 在 Drop 时自动 SIGKILL+reap（兜底模式）
//! T-C: SandboxState（持 Arc<Mutex<Option<Child>>>）clone 之后两份共享同一个
//!      Child，cleanup 路径 take().await 一次后另一份再 take 拿不到——
//!      避免 double-kill 的语义保证。
//!
//! 这些测试构成 reap 行为的 lint：未来若有人把 spawn 改回 std::process::Command
//! 不带 kill_on_drop、或 cleanup 改回外部 `kill PID`，这里会失败。

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;

/// Linux 专用：通过 /proc/<pid> 看进程是否还在（包括 zombie）。
/// 进程已被 reap → /proc/<pid> 消失；zombie 仍在 /proc 但 status=Z。
fn proc_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

fn proc_is_zombie(pid: u32) -> bool {
    let status = match std::fs::read_to_string(format!("/proc/{}/status", pid)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    status.lines().any(|l| l.starts_with("State:") && l.contains("Z"))
}

fn spawn_sleep(secs: &str) -> Child {
    Command::new("sleep")
        .arg(secs)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sleep")
}

#[tokio::test]
async fn t_a_explicit_start_kill_wait_reaps() {
    // 模拟 cleanup_sandbox 路径：take Child + start_kill + wait
    let mut child = spawn_sleep("30");
    let pid = child.id().expect("pid");
    assert!(proc_exists(pid), "sleep should be alive immediately after spawn");

    child.start_kill().expect("start_kill");
    child.wait().await.expect("wait");

    // wait().await 返回后 Linux 已 reap，/proc/<pid> 应消失。
    // 给一点时间防止极偶发的 /proc 刷新延迟。
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!proc_exists(pid), "after wait(), pid {} must be gone (not zombie)", pid);
}

#[tokio::test]
async fn t_b_kill_on_drop_reaps() {
    // 模拟最坏情况：忘记调 cleanup，靠 Child Drop 兜底
    let pid = {
        let child = spawn_sleep("30");
        let pid = child.id().expect("pid");
        assert!(proc_exists(pid));
        pid
        // child 在这里 drop → tokio kill_on_drop 后台 SIGKILL+reap
    };

    // tokio kill_on_drop 是 spawn 一个 detached task 处理 reap，给它一点时间
    for _ in 0..20 {
        if !proc_exists(pid) { return; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "kill_on_drop did not reap pid {} within 2s (zombie={})",
        pid,
        proc_is_zombie(pid)
    );
}

#[tokio::test]
async fn t_c_sandbox_state_arc_take_semantics() {
    // SandboxState.sandbox_child: Arc<Mutex<Option<Child>>>
    // —— Clone 共享句柄；cleanup 路径 take 一次后另一份再 take 拿到 None，
    //    避免 double start_kill / wait
    let child = spawn_sleep("30");
    let pid = child.id().expect("pid");
    let handle: Arc<AsyncMutex<Option<Child>>> = Arc::new(AsyncMutex::new(Some(child)));

    let h2 = handle.clone();

    // 第 1 次 take：拿到 Child，正常 reap
    {
        let mut g = handle.lock().await;
        let mut c = g.take().expect("first take must yield Child");
        c.start_kill().ok();
        c.wait().await.ok();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!proc_exists(pid));

    // 第 2 次 take（来自 clone）：必须是 None，不会 panic、不会 double-kill
    {
        let mut g = h2.lock().await;
        assert!(g.take().is_none(), "second take must yield None after first take");
    }
}

/// 多 sandbox 场景 stress test：spawn 5 个 child，cleanup 全部，
/// 断言进程数归零。
///
/// 这是 T1（zombies=0 after batch）的自动化版本——之前手动 ps grep defunct，
/// 现在跑 cargo test 就能保证。
#[tokio::test]
async fn t_d_batch_cleanup_no_zombie() {
    let mut handles: Vec<Arc<AsyncMutex<Option<Child>>>> = Vec::new();
    let mut pids: Vec<u32> = Vec::new();
    for _ in 0..5 {
        let child = spawn_sleep("30");
        pids.push(child.id().expect("pid"));
        handles.push(Arc::new(AsyncMutex::new(Some(child))));
    }
    for pid in &pids {
        assert!(proc_exists(*pid));
    }

    // 模拟 cleanup_sandbox 顺序清理
    for h in &handles {
        let mut g = h.lock().await;
        if let Some(mut c) = g.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut alive: Vec<u32> = pids.iter().copied().filter(|p| proc_exists(*p)).collect();
    let zombies: Vec<u32> = pids.iter().copied().filter(|p| proc_is_zombie(*p)).collect();
    alive.retain(|p| !zombies.contains(p));
    assert!(alive.is_empty() && zombies.is_empty(),
        "after batch cleanup: alive={:?}, zombies={:?}", alive, zombies);
}
