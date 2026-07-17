use std::sync::Arc;
use tokio::sync::RwLock;

use clap::Parser;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use frama_c_mcp_server::mcp::server::FramaCMcpServer;
use frama_c_mcp_server::state::SessionState;

#[derive(clap::Parser)]
#[command(name = "frama-c-mcp-server")]
#[command(about = "MCP server for Frama-C formal verification (lazy spawn)")]
struct Cli {
    /// [DEPRECATED, Issue #95] Socket path was used by launch-mcp.sh wrapper.
    /// Lazy mode auto-generates `/tmp/frama-c-mcp-<server_pid>.sock`. 该参数被忽略，
    /// 留着只为不 break 旧 `.mcp.json`（重跑 install.sh 即可清理）。
    #[arg(long)]
    socket: Option<String>,

    /// Path to frama-c binary (for spawning sandbox instances + main lazy)
    #[arg(long, default_value = "frama-c")]
    frama_c: String,

    /// OS safety ceiling for concurrent sandboxes (each spawns a Frama-C process).
    /// fsmint-3: 不再当调度限制——调度并发由 v-p-fsm max_sandboxes var 决定
    /// （薄壳 enter_fsm 时按 min(nproc, mem_gb//8) 自适应）。本值仅防失控的高安全顶。
    #[arg(long, default_value = "32")]
    max_sandboxes: usize,
}

fn main() -> anyhow::Result<()> {
    // 手动构造 runtime + shutdown_timeout，而非 #[tokio::main]。
    // 原因：tokio stdin 用 blocking pool thread 做 sync read，runtime Drop 会
    // 永远等这个 blocking thread 退出，但 thread 阻塞在 kernel pipe_read 上
    // 永不返回（parent 不关闭 stdin → 无 EOF）。手动 shutdown_timeout 强制
    // 在 N 秒后放弃 blocking thread 让进程退出。
    // 在 shutdown_timeout 之前，async_main 的 Drop chain 已经把 MainFramaCState
    // 的 frama-c child drop 掉了（kill_on_drop SIGKILL），所以 child 不会孤儿。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(async_main());
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

async fn async_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let state = Arc::new(RwLock::new(SessionState::default()));

    if let Some(sock) = &cli.socket {
        tracing::warn!(
            "--socket {} is deprecated (Issue #95). Lazy mode auto-generates PID-based socket. Re-run install.sh.",
            sock
        );
    }

    // Lazy: don't connect frama-c at startup. First reload_project call will
    // ensure_main_spawned() which spawns frama-c + connects client.
    tracing::info!("MCP server starting (lazy spawn mode, frama-c not connected yet)");
    let server = FramaCMcpServer::new_lazy(state, cli.frama_c, cli.max_sandboxes);

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    tracing::info!("MCP server running on stdio");

    // Graceful shutdown：spawn 一个信号 handler task，收到 SIGTERM/SIGINT 后调
    // service 的 cancellation_token.cancel()。rmcp 内部 serve_loop 的 select! 含
    // `_ = serve_loop_ct.cancelled() => break QuitReason::Cancelled` 分支，cancel
    // 后立刻退出 loop，service.waiting() 返回。然后 main return → Drop chain →
    // MainFramaCState/Sandbox 的 Child Drop → kill_on_drop SIGKILL frama-c。
    //
    // **不能**用 tokio::select! 包 service.waiting()：rmcp 内部 serve_loop 是
    // tokio::spawn 的独立 task，外层 select 只取消 waiting() 的 await，inner
    // task 仍 blocking 在 transport.receive() (stdin pipe_read)，runtime
    // shutdown 等它永远不退 → 死锁。
    //
    // 详见 docs/fixes/sigterm-handler-frama-c-orphan.md。
    //
    // 仍未覆盖：SIGKILL / OOM / crash 路径下 frama-c 仍会孤儿（已知限制，
    // userspace 无法响应 SIGKILL，需 kernel-level PR_SET_PDEATHSIG 才能兜底）。
    let token = service.cancellation_token();
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("install SIGTERM handler failed: {}", e);
                return;
            }
        };
        // SIGHUP：terminal pane 关闭时父 shell 发的信号，必须处理避免 orphan
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("install SIGHUP handler failed: {}", e);
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sighup.recv() => tracing::info!("received SIGHUP"),
        }
        tracing::info!("cancelling MCP service for graceful shutdown");
        token.cancel();
    });

    // Returns QuitReason::Cancelled (我们 cancel) / Closed (stdin EOF disconnected)
    // / JoinError (内部 task panic).
    match service.waiting().await {
        Ok(reason) => tracing::info!("MCP service exited: {:?}", reason),
        Err(e) => tracing::warn!("MCP service exited with error: {}", e),
    }

    Ok(())
}
