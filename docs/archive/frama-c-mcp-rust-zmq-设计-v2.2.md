# Frama-C MCP Server — Rust + ZMQ 实现设计（v2.2）

> **更新日期**: 2025-02-17  
> **变更摘要**: 升级 rmcp 至 v0.15+，适配 MCP 2025-11-25 规范，引入 Task 支持（长时间运行的验证操作），更新宏 API（`#[tool_router]` / `#[tool_handler]` 替代 `#[tool(tool_box)]`），升级 schemars 至 v1.0+，调整 Rust edition 至 2024。新增 CIL AST 代码导航 tool 组（替代 LSP）。新增 Agentic Search tool 组（聚合查询，减少 Agent 调用轮次）。

---

## 变更日志

| 版本 | 日期 | 主要变更 |
|------|------|---------|
| v1 | 2025-02-04 | 初始设计，基于 rmcp 0.8+ |
| v2 | 2025-02-17 | 升级 rmcp 0.15+，MCP 2025-11-25 规范，Task 支持，宏 API 重构 |
| v2.1 | 2025-02-17 | 新增第六组 CIL 代码导航 tool（替代 LSP），5 个新 tool |
| v2.2 | 2025-02-17 | 新增第七组 Agentic Search tool（聚合查询），2 个新 tool。总计 7 组 20 tool |

### v1 → v2 关键差异

| 维度 | v1 | v2 |
|------|----|----|
| **rmcp 版本** | 0.8+ | 0.15+（最新稳定） |
| **MCP 协议版本** | 2024-11-05 | 2025-11-25 |
| **Rust edition** | 2021 | 2024 |
| **schemars** | 0.8 (隐式) | 1.0+（显式依赖） |
| **Tool 宏** | `#[tool(tool_box)]` impl Server | `#[tool_router]` + `#[tool_handler]` 分离 |
| **参数包装** | `#[tool(param)]` / `#[tool(aggr)]` | `Parameters<T>` wrapper |
| **长时间任务** | 不支持 | Task 生命周期（create/get/result/cancel） |
| **ServerHandler** | 直接 impl | 通过 `#[tool_handler]` 宏自动生成路由 |
| **Capabilities** | 手动构造 | `ServerCapabilities::builder().enable_tools().build()` |
| **Transport feature** | `transport-stdio` | `transport-io` |
| **新增能力** | — | Tool annotations（只读/破坏性标记）、Structured output |

---

## 1. 架构总览

```
┌─────────────────────────────────────────────────────────┐
│                    LLM Agent (Claude)                    │
│               Claude Agent SDK / API call                │
└──────────────┬──────────────────────────────┬────────────┘
               │ MCP (JSON-RPC / stdio)       │
               │ Protocol: 2025-11-25         │
               ▼                              ▼
┌──────────────────────────────────────────────────────────┐
│              frama-c-mcp-server  (Rust)                   │
│                                                           │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────────┐  │
│  │ MCP Layer   │  │ Tool Router  │  │ State Manager   │  │
│  │ (rmcp 0.15) │  │ + Task Mgr   │  │ (项目/会话状态)  │  │
│  │ stdio/HTTP  │  │ 20 tools     │  │                 │  │
│  └──────┬──────┘  └──────┬───────┘  └────────┬────────┘  │
│         │                │                    │           │
│  ┌──────▼────────────────▼────────────────────▼────────┐  │
│  │              Frama-C Client (ZMQ)                    │  │
│  │  ┌────────────────┐  ┌─────────────────────────┐    │  │
│  │  │ ZMQ Socket     │  │ Request/Response Codec  │    │  │
│  │  │ (zeromq crate) │  │ (serde_json)            │    │  │
│  │  └───────┬────────┘  └─────────────────────────┘    │  │
│  └──────────┼──────────────────────────────────────────┘  │
└─────────────┼────────────────────────────────────────────┘
              │ ZMQ (tcp://127.0.0.1:5555)
              ▼
┌──────────────────────────────────────────────────────────┐
│              Frama-C Server 进程                          │
│                                                           │
│  ┌──────────┐  ┌──────┐  ┌──────┐  ┌──────────────────┐  │
│  │ Kernel   │  │ EVA  │  │ WP   │  │ VP-Bridge Plugin │  │
│  │ AST/Prop │  │      │  │      │  │ (自定义 OCaml)   │  │
│  └──────────┘  └──────┘  └──────┘  └──────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

**关键设计决策**：
- MCP 层用 `rmcp` 官方 Rust SDK（v0.15+），对外暴露 stdio 或 Streamable HTTP
- 与 Frama-C 通信用 ZMQ `REQ/REP` 模式，序列化格式为 Frama-C Server 原生 JSON 协议
- 整个 MCP Server 是单进程异步架构（tokio），管理 Frama-C 子进程的生命周期
- 适配 MCP 2025-11-25 规范：支持 Task（长时间运行的 EVA/WP 操作）和 Tool annotations

---

## 2. 项目结构

```
frama-c-mcp-server/
├── Cargo.toml
├── rust-toolchain.toml          # 指定 nightly（edition 2024 需要）
├── src/
│   ├── main.rs                  # 入口：启动 MCP server + Frama-C 子进程
│   ├── lib.rs                   # 库 re-export
│   │
│   ├── mcp/                     # MCP 层
│   │   ├── mod.rs
│   │   ├── server.rs            # MCP ServerHandler 实现（#[tool_handler]）
│   │   └── tasks.rs             # Task 管理（长时间运行操作）  ← 新增
│   │
│   ├── frama_c/                 # Frama-C 通信层
│   │   ├── mod.rs
│   │   ├── client.rs            # ZMQ client：请求/响应/信号处理
│   │   ├── protocol.rs          # Frama-C Server JSON 协议编解码
│   │   ├── process.rs           # Frama-C 子进程管理（启动/停止/健康检查）
│   │   └── types.rs             # Frama-C 原生类型映射（Rust struct）
│   │
│   ├── tools/                   # Tool 实现（每个文件一组 tool）
│   │   ├── mod.rs
│   │   ├── project.rs           # load_project, get_callgraph, get_function_info
│   │   ├── eva.rs               # run_eva, get_eva_alarms, get_eva_value
│   │   ├── wp.rs                # run_wp, get_wp_goals
│   │   ├── annotation.rs        # inject_acsl, remove_acsl, get_current_annotations
│   │   ├── planner.rs           # get_verification_status, suggest_verification_plan
│   │   ├── navigation.rs        # CIL 代码导航（替代 LSP）  ← v2.1 新增
│   │   └── search.rs            # Agentic Search 聚合查询   ← v2.2 新增
│   │
│   ├── state.rs                 # 会话状态管理（项目状态、分析结果缓存）
│   └── error.rs                 # 统一错误类型
│
├── tests/
│   ├── integration/             # 集成测试（需要 Frama-C）
│   │   ├── eva_flow.rs
│   │   └── wp_flow.rs
│   └── unit/                    # 单元测试（mock ZMQ）
│       ├── protocol_test.rs
│       └── tool_test.rs
│
└── examples/
    ├── sort.c                   # 测试用 C 程序
    └── demo_session.rs          # 端到端演示
```

---

## 3. 依赖选型

```toml
[package]
name = "frama-c-mcp-server"
version = "0.1.0"
edition = "2024"                 # ← 升级：rmcp 0.15 要求 edition 2024

[dependencies]
# MCP 协议（官方 Rust SDK）
rmcp = { version = "0.15", features = [
    "server",
    "transport-io",              # ← 改名：原 transport-stdio → transport-io
    "macros",                    # ← 新增：启用 #[tool] / #[tool_router] / #[tool_handler] 宏
] }

# 异步运行时
tokio = { version = "1", features = ["full"] }

# ZMQ 通信
zeromq = "0.4"              # 纯 Rust ZMQ 实现（无 C 依赖）
# 备选: zmq = "0.10"        # libzmq 绑定（更成熟但需要 C 库）

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Schema 生成（rmcp 0.15 要求 schemars 1.0+）
schemars = "1.0"                 # ← 升级：v1 设计中未显式声明

# 工具
thiserror = "2"             # 错误类型
tracing = "0.1"             # 日志/追踪
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
uuid = { version = "1", features = ["v4"] }  # 请求 ID
anyhow = "1.0"              # ← 新增：简化 main 错误处理

# CLI 参数
clap = { version = "4", features = ["derive"] }
```

**Rust 工具链配置**（新增）：

```toml
# rust-toolchain.toml
[toolchain]
channel = "nightly"
components = ["rustfmt", "clippy"]
```

> **注意**：rmcp 0.15 要求 Rust edition 2024，目前仅在 nightly 通道可用。  
> 待 edition 2024 稳定后可切回 stable。

**ZMQ crate 选择说明**（无变化）：
- `zeromq`（纯 Rust）：无外部 C 依赖，编译简单，tokio 原生集成。但功能不如 libzmq 完整。
- `zmq`（libzmq 绑定）：更成熟稳定，但需要系统安装 `libzmq`。

推荐 Phase 1 用 `zeromq`（零依赖快速开始），如果遇到兼容性问题再切 `zmq`。

---

## 4. 核心模块设计

### 4.1 Frama-C ZMQ Client（无实质变化）

```rust
// src/frama_c/client.rs

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeromq::{Socket, SocketRecv, SocketSend, ReqSocket};
use tokio::sync::Mutex;

/// Frama-C Server 请求类型
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum FramaCRequest {
    /// GET: 只读查询，可在分析运行中调用
    #[serde(rename = "GET")]
    Get {
        id: String,
        request: String,          // e.g. "kernel.ast.getFunctions"
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// SET: 配置修改，在分析间处理
    #[serde(rename = "SET")]
    Set {
        id: String,
        request: String,
        data: Value,
    },
    /// EXEC: 启动分析计算
    #[serde(rename = "EXEC")]
    Exec {
        id: String,
        request: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
}

/// Frama-C Server 响应
#[derive(Debug, Deserialize)]
pub struct FramaCResponse {
    pub id: String,
    #[serde(flatten)]
    pub result: FramaCResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
pub enum FramaCResult {
    /// 请求成功
    #[serde(rename = "DATA")]
    Data { data: Value },
    /// 请求失败
    #[serde(rename = "ERROR")]
    Error { data: Value },
    /// EXEC 进行中的中间信号
    #[serde(rename = "SIGNAL")]
    Signal { data: Value },
    /// 请求被拒绝（分析正在运行）
    #[serde(rename = "REJECTED")]
    Rejected { data: Value },
}

/// ZMQ 客户端，封装与 Frama-C Server 的通信
pub struct FramaCClient {
    socket: Mutex<ReqSocket>,
    endpoint: String,
}

impl FramaCClient {
    pub async fn connect(endpoint: &str) -> Result<Self, FramaCError> {
        let mut socket = ReqSocket::new();
        socket.connect(endpoint).await?;
        Ok(Self {
            socket: Mutex::new(socket),
            endpoint: endpoint.to_string(),
        })
    }

    /// 发送 GET 请求
    pub async fn get(&self, request: &str, data: Option<Value>) -> Result<Value, FramaCError> {
        let req = FramaCRequest::Get {
            id: uuid::Uuid::new_v4().to_string(),
            request: request.to_string(),
            data,
        };
        self.send_request(req).await
    }

    /// 发送 SET 请求
    pub async fn set(&self, request: &str, data: Value) -> Result<Value, FramaCError> {
        let req = FramaCRequest::Set {
            id: uuid::Uuid::new_v4().to_string(),
            request: request.to_string(),
            data,
        };
        self.send_request(req).await
    }

    /// 发送 EXEC 请求（阻塞直到完成，中间处理 SIGNAL）
    pub async fn exec(
        &self,
        request: &str,
        data: Option<Value>,
        on_signal: Option<&dyn Fn(Value)>,
    ) -> Result<Value, FramaCError> {
        let req = FramaCRequest::Exec {
            id: uuid::Uuid::new_v4().to_string(),
            request: request.to_string(),
            data,
        };
        self.send_exec_request(req, on_signal).await
    }

    /// 核心发送逻辑（GET/SET）
    async fn send_request(&self, req: FramaCRequest) -> Result<Value, FramaCError> {
        let mut socket = self.socket.lock().await;
        let json = serde_json::to_string(&req)?;

        socket.send(json.into()).await?;
        let reply = socket.recv().await?;
        let reply_str = String::from_utf8(reply.into())?;

        let resp: FramaCResponse = serde_json::from_str(&reply_str)?;
        match resp.result {
            FramaCResult::Data { data } => Ok(data),
            FramaCResult::Error { data } => Err(FramaCError::ServerError(data)),
            FramaCResult::Rejected { data } => Err(FramaCError::Rejected(data)),
            _ => Err(FramaCError::UnexpectedResponse),
        }
    }

    /// EXEC 请求：循环接收直到得到 DATA 或 ERROR
    async fn send_exec_request(
        &self,
        req: FramaCRequest,
        on_signal: Option<&dyn Fn(Value)>,
    ) -> Result<Value, FramaCError> {
        let mut socket = self.socket.lock().await;
        let json = serde_json::to_string(&req)?;

        socket.send(json.into()).await?;

        // EXEC 可能返回多个 SIGNAL，最后一个 DATA/ERROR
        loop {
            let reply = socket.recv().await?;
            let reply_str = String::from_utf8(reply.into())?;
            let resp: FramaCResponse = serde_json::from_str(&reply_str)?;

            match resp.result {
                FramaCResult::Data { data } => return Ok(data),
                FramaCResult::Error { data } => return Err(FramaCError::ServerError(data)),
                FramaCResult::Signal { data } => {
                    if let Some(cb) = on_signal {
                        cb(data);
                    }
                    // 继续等待最终响应
                }
                FramaCResult::Rejected { data } => return Err(FramaCError::Rejected(data)),
            }
        }
    }
}
```

### 4.2 Frama-C 子进程管理（无实质变化）

```rust
// src/frama_c/process.rs

use tokio::process::{Command, Child};
use std::time::Duration;

pub struct FramaCProcess {
    child: Child,
    zmq_endpoint: String,
}

impl FramaCProcess {
    /// 启动 Frama-C Server 子进程
    pub async fn spawn(
        c_files: &[String],
        zmq_port: u16,
        extra_args: &[String],
    ) -> Result<Self, FramaCError> {
        let endpoint = format!("tcp://127.0.0.1:{}", zmq_port);

        let mut cmd = Command::new("frama-c");
        cmd.arg("-server-zmq")
           .arg(&endpoint)
           .args(c_files);

        for arg in extra_args {
            cmd.arg(arg);
        }

        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn()?;

        let process = Self {
            child,
            zmq_endpoint: endpoint,
        };

        process.wait_ready(Duration::from_secs(30)).await?;

        Ok(process)
    }

    /// 轮询等待 Server 就绪
    async fn wait_ready(&self, timeout: Duration) -> Result<(), FramaCError> {
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed() > timeout {
                return Err(FramaCError::StartupTimeout);
            }
            match FramaCClient::connect(&self.zmq_endpoint).await {
                Ok(client) => {
                    if client.get("kernel.project.getInfo", None).await.is_ok() {
                        return Ok(());
                    }
                }
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.zmq_endpoint
    }

    pub async fn shutdown(&mut self) -> Result<(), FramaCError> {
        self.child.kill().await?;
        Ok(())
    }
}

impl Drop for FramaCProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
```

### 4.3 MCP Server 与 Tool 注册（⚠️ 重大变更）

**v1 → v2 宏 API 变更说明**：

| v1 (`rmcp 0.8`) | v2 (`rmcp 0.15`) | 说明 |
|---|---|---|
| `#[tool(tool_box)]` on impl block | `#[tool_router]` on tool impl block | 工具路由器宏 |
| `#[tool(tool_box)]` on ServerHandler | `#[tool_handler]` on ServerHandler | 自动生成 `call_tool` / `list_tools` |
| `#[tool(param, description = "...")]` | `Parameters<T>` wrapper + `schemars::JsonSchema` | 参数传递方式 |
| `#[tool(aggr)] SumReq { a, b }` | `Parameters<SumReq>` 解构 | 聚合参数 |
| 隐式 `CallToolResult` | 显式 `Result<CallToolResult, McpError>` | 返回类型 |
| `ServerInfo { name, version, .. }` | `ServerInfo { capabilities, instructions, .. }` | Server 信息结构 |

```rust
// src/mcp/server.rs

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::tool::ToolRouter,
    model::*,
    tool, tool_handler, tool_router,
    ErrorData as McpError,
};
use serde::Deserialize;
use schemars::JsonSchema;
use crate::frama_c::client::FramaCClient;
use crate::state::SessionState;
use std::sync::Arc;
use tokio::sync::RwLock;

/// MCP Server 主结构
#[derive(Clone)]
pub struct FramaCMcpServer {
    client: Arc<FramaCClient>,
    state: Arc<RwLock<SessionState>>,
    tool_router: ToolRouter<Self>,
}

// ═══════════════════════════════════════════
//  参数类型定义（使用 schemars 1.0 JsonSchema）
// ═══════════════════════════════════════════

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadProjectParams {
    #[schemars(description = "List of C source file paths")]
    pub files: Vec<String>,
    #[schemars(description = "Additional C preprocessor arguments")]
    pub cpp_args: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunEvaParams {
    #[schemars(description = "EVA precision level 1-11 (default 3)")]
    pub precision: Option<u8>,
    #[schemars(description = "Entry function (default 'main')")]
    pub main_function: Option<String>,
    #[schemars(description = "Slevel for loop unrolling (0 = default)")]
    pub slevel: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetEvaAlarmsParams {
    #[schemars(description = "Filter by function name")]
    pub function: Option<String>,
    #[schemars(description = "Filter by alarm type (mem_access, division_by_zero, etc.)")]
    pub alarm_type: Option<String>,
    #[schemars(description = "Filter by status: red, orange, green")]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetEvaValueParams {
    #[schemars(description = "Function name")]
    pub function: String,
    #[schemars(description = "Statement marker/ID")]
    pub marker: String,
    #[schemars(description = "C expression to query")]
    pub expression: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunWpParams {
    #[schemars(description = "Functions to verify (empty = all annotated)")]
    pub functions: Option<Vec<String>>,
    #[schemars(description = "SMT prover: alt-ergo, z3, cvc5")]
    pub prover: Option<String>,
    #[schemars(description = "Prover timeout in seconds")]
    pub timeout: Option<u32>,
    #[schemars(description = "Also verify RTE assertions")]
    pub rte: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetWpGoalsParams {
    #[schemars(description = "Filter by function")]
    pub function: Option<String>,
    #[schemars(description = "Filter by status: valid, unknown, timeout, failed")]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InjectAcslParams {
    #[schemars(description = "Target function")]
    pub function: String,
    #[schemars(description = "Annotation type: requires, ensures, loop_invariant, assert, assigns, loop_variant")]
    pub annotation_type: String,
    #[schemars(description = "ACSL expression, e.g. '\\valid(p + (0..n-1))'")]
    pub content: String,
    #[schemars(description = "Location hint (for loop_invariant/assert): line number or loop ID")]
    pub location: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveAcslParams {
    #[schemars(description = "Annotation ID to remove")]
    pub annotation_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetAnnotationsParams {
    #[schemars(description = "Function name")]
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFunctionInfoParams {
    #[schemars(description = "Function name")]
    pub function_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SuggestPlanParams {
    #[schemars(description = "Target: 'all', function name, or alarm ID")]
    pub target: Option<String>,
    #[schemars(description = "Strategy: fast, balanced, thorough")]
    pub strategy: Option<String>,
}

// ─── 第六组参数类型：CIL 代码导航 ─── (v2.1 新增)

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindCallersParams {
    #[schemars(description = "Function name to find callers of")]
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDataDepsParams {
    #[schemars(description = "Function name")]
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindMemoryOpsParams {
    #[schemars(description = "Function name")]
    pub function: String,
    #[schemars(description = "Filter by kind: deref, array, alloc, or null for all")]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupSymbolParams {
    #[schemars(description = "Identifier name (function, variable, type, struct field)")]
    pub name: String,
    #[schemars(description = "Restrict to scope of this function (optional, for local variables)")]
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetCfgParams {
    #[schemars(description = "Function name")]
    pub function: String,
    #[schemars(description = "Output format: json (structured) or dot (Graphviz)")]
    pub format: Option<String>,
}

// ─── 第七组参数类型：Agentic Search 聚合查询 ─── (v2.2 新增)

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceCallChainParams {
    #[schemars(description = "Starting function name")]
    pub function: String,
    #[schemars(description = "Direction: 'callers' (who calls me, upward) or 'callees' (who I call, downward)")]
    pub direction: String,
    #[schemars(description = "Max traversal depth (default 5, max 20)")]
    pub max_depth: Option<u32>,
    #[schemars(description = "Stop traversal at these functions (e.g. ['main', 'entry_point'])")]
    pub stop_at: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InvestigateAlarmParams {
    #[schemars(description = "Alarm ID from get_eva_alarms result")]
    pub alarm_id: String,
    #[schemars(description = "Investigation depth: 'quick' (alarm + values), 'normal' (+ callers + deps), 'deep' (+ full call chain + CFG)")]
    pub depth: Option<String>,
}

// ═══════════════════════════════════════════
//  Tool 实现（使用 #[tool_router] 宏）
// ═══════════════════════════════════════════

use rmcp::handler::server::tool::Parameters;

#[tool_router]
impl FramaCMcpServer {
    pub fn new(client: FramaCClient) -> Self {
        Self {
            client: Arc::new(client),
            state: Arc::new(RwLock::new(SessionState::default())),
            tool_router: Self::tool_router(),
        }
    }

    // ─── 第一组：项目初始化与全局信息 ───

    #[tool(description = "Load C source files into Frama-C and return project overview including function list, global variables, LOC, and parse warnings.")]
    async fn load_project(
        &self,
        Parameters(params): Parameters<LoadProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        let functions = self.client.get("kernel.ast.getFunctions", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;
        let metrics = self.client.get("metrics.getMetrics", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let result = serde_json::json!({
            "functions": functions,
            "total_loc": metrics.get("sloc"),
            "status": "loaded"
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default()
        )]))
    }

    #[tool(description = "Get function call graph with entry points, leaf functions, and cycles.")]
    async fn get_callgraph(&self) -> Result<CallToolResult, McpError> {
        let data = self.client.get("callgraph.getGraph", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&data).unwrap_or_default()
        )]))
    }

    #[tool(description = "Get detailed info for a function: params, complexity, loops, pointer ops, annotations.")]
    async fn get_function_info(
        &self,
        Parameters(params): Parameters<GetFunctionInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({ "function": params.function_name });
        let ast_info = self.client.get("kernel.ast.getFunctionInfo", Some(query.clone())).await
            .map_err(|e| McpError::internal(&e.to_string()))?;
        let metrics = self.client.get("metrics.getFunctionMetrics", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let result = serde_json::json!({
            "ast": ast_info,
            "metrics": metrics,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default()
        )]))
    }

    // ─── 第二组：EVA 分析 ───

    #[tool(description = "Run EVA abstract interpretation analysis. Returns alarm summary, coverage stats, and duration. This is a long-running operation.")]
    async fn run_eva(
        &self,
        Parameters(params): Parameters<RunEvaParams>,
    ) -> Result<CallToolResult, McpError> {
        let eva_params = serde_json::json!({
            "precision": params.precision.unwrap_or(3),
            "main": params.main_function.unwrap_or_else(|| "main".into()),
            "slevel": params.slevel.unwrap_or(0),
        });
        self.client.set("eva.setParams", eva_params).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let _result = self.client.exec("eva.compute", None, None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let summary = self.client.get("eva.getSummary", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;
        let alarms = self.client.get("eva.getAlarms", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let output = serde_json::json!({
            "status": "completed",
            "summary": summary,
            "alarm_count": alarms.as_array().map(|a| a.len()).unwrap_or(0),
        });

        {
            let mut state = self.state.write().await;
            state.eva_completed = true;
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&output).unwrap_or_default()
        )]))
    }

    #[tool(description = "Get EVA alarms filtered by function, type, or status (red/orange/green).")]
    async fn get_eva_alarms(
        &self,
        Parameters(params): Parameters<GetEvaAlarmsParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "function": params.function,
            "type": params.alarm_type,
            "status": params.status,
        });

        let alarms = self.client.get("eva.getAlarms", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&alarms).unwrap_or_default()
        )]))
    }

    #[tool(description = "Query EVA value range for an expression at a specific program point.")]
    async fn get_eva_value(
        &self,
        Parameters(params): Parameters<GetEvaValueParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "function": params.function,
            "marker": params.marker,
            "expression": params.expression,
        });

        let value = self.client.get("eva.getValues", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&value).unwrap_or_default()
        )]))
    }

    // ─── 第三组：WP 演绎验证 ───

    #[tool(description = "Run WP deductive verification on specified functions. Returns proof goal statistics by prover and function. This is a long-running operation.")]
    async fn run_wp(
        &self,
        Parameters(params): Parameters<RunWpParams>,
    ) -> Result<CallToolResult, McpError> {
        let wp_params = serde_json::json!({
            "functions": params.functions,
            "prover": params.prover.unwrap_or_else(|| "alt-ergo".into()),
            "timeout": params.timeout.unwrap_or(10),
            "rte": params.rte.unwrap_or(true),
        });
        self.client.set("wp.setParams", wp_params).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let _result = self.client.exec("wp.compute", None, None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let goals = self.client.get("wp.getGoals", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        {
            let mut state = self.state.write().await;
            state.wp_completed = true;
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&goals).unwrap_or_default()
        )]))
    }

    #[tool(description = "Get WP proof goals filtered by function or status (valid/unknown/timeout/failed).")]
    async fn get_wp_goals(
        &self,
        Parameters(params): Parameters<GetWpGoalsParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "function": params.function,
            "status": params.status,
        });

        let goals = self.client.get("wp.getGoals", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&goals).unwrap_or_default()
        )]))
    }

    // ─── 第四组：ACSL 注解操作 ───

    #[tool(description = "Inject an ACSL annotation (requires/ensures/loop_invariant/assert/assigns) into a function. Returns parse status and annotation ID.")]
    async fn inject_acsl(
        &self,
        Parameters(params): Parameters<InjectAcslParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "function": params.function,
            "type": params.annotation_type,
            "content": params.content,
            "location": params.location,
        });

        let result = self.client.set("vp.injectAnnotation", query).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default()
        )]))
    }

    #[tool(description = "Remove an ACSL annotation by its ID.")]
    async fn remove_acsl(
        &self,
        Parameters(params): Parameters<RemoveAcslParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({ "id": params.annotation_id });
        let result = self.client.set("vp.removeAnnotation", query).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default()
        )]))
    }

    #[tool(description = "List all ACSL annotations on a function with their verification status.")]
    async fn get_current_annotations(
        &self,
        Parameters(params): Parameters<GetAnnotationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({ "function": params.function });

        let properties = self.client.get("kernel.properties.getStatus", Some(query.clone())).await
            .map_err(|e| McpError::internal(&e.to_string()))?;
        let annotations = self.client.get("vp.getAnnotations", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let result = serde_json::json!({
            "annotations": annotations,
            "verification_status": properties,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default()
        )]))
    }

    // ─── 第五组：验证规划器专用 ───

    #[tool(description = "Get comprehensive verification status: property counts by category, per-function status, unresolved issues with recommended actions.")]
    async fn get_verification_status(&self) -> Result<CallToolResult, McpError> {
        let properties = self.client.get("kernel.properties.getAll", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;
        let eva_summary = self.client.get("eva.getSummary", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;
        let wp_summary = self.client.get("wp.getSummary", None).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let state = self.state.read().await;

        let result = serde_json::json!({
            "properties": properties,
            "eva": eva_summary,
            "wp": wp_summary,
            "session": {
                "eva_completed": state.eva_completed,
                "wp_completed": state.wp_completed,
            }
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default()
        )]))
    }

    #[tool(description = "Generate a suggested verification plan based on current analysis state. Recommends next actions (refine EVA, add annotations, run WP) with rationale.")]
    async fn suggest_verification_plan(
        &self,
        Parameters(params): Parameters<SuggestPlanParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "target": params.target.unwrap_or_else(|| "all".into()),
            "strategy": params.strategy.unwrap_or_else(|| "balanced".into()),
        });

        let plan = self.client.get("vp.suggestPlan", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&plan).unwrap_or_default()
        )]))
    }

    // ─── 第六组：CIL 代码导航（替代 LSP）  ← v2.1 新增 ───
    //
    // 设计思路：验证场景下 Frama-C 已启动，CIL AST 已在内存中，
    // 代码修改仅通过 inject_acsl/remove_acsl 进行。
    // 此时额外启动 clangd 是冗余的——CIL AST 比 clang AST 更丰富：
    //   - 带 ACSL 注解和验证状态
    //   - 带 EVA 值域信息
    //   - 带函数间依赖分析（From 插件）
    //   - CIL 规范化保证了一致的查询结果
    //
    // 实现分层：
    //   Phase 1-2: 使用 Frama-C 内置 kernel.ast.* / callgraph.* API
    //   Phase 3:   通过 VP-Bridge OCaml 插件暴露更细粒度的 CIL 遍历

    #[tool(description = "Find all call sites of a function across the project. Returns caller function name, file, line, and call context.")]
    async fn find_callers(
        &self,
        Parameters(params): Parameters<FindCallersParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({ "function": params.function });
        let callers = self.client.get("callgraph.getCallers", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&callers).unwrap_or_default()
        )]))
    }

    #[tool(description = "Get data dependencies of a function: which globals and formal parameters it reads and writes. Uses Frama-C's From plugin for precise dataflow analysis.")]
    async fn get_data_deps(
        &self,
        Parameters(params): Parameters<GetDataDepsParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({ "function": params.function });

        // From 插件提供函数级别的读写依赖分析
        let deps = self.client.get("from.getFunctionDeps", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&deps).unwrap_or_default()
        )]))
    }

    #[tool(description = "List all pointer dereferences, array accesses, and dynamic memory operations in a function. Essential for identifying verification targets (potential mem_access / division_by_zero alarms).")]
    async fn find_memory_ops(
        &self,
        Parameters(params): Parameters<FindMemoryOpsParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "function": params.function,
            "kind": params.kind,  // "deref", "array", "alloc", or null for all
        });

        // Phase 1-2: 从 EVA alarms 中提取内存操作点
        // Phase 3:   VP-Bridge 直接遍历 CIL Mem/Index/Call 节点
        let ops = self.client.get("vp.getMemoryOps", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&ops).unwrap_or_default()
        )]))
    }

    #[tool(description = "Look up the type signature, definition location, and scope of any C identifier (function, variable, type, struct field). Resolves through typedefs.")]
    async fn lookup_symbol(
        &self,
        Parameters(params): Parameters<LookupSymbolParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "name": params.name,
            "scope": params.scope,  // optional: function name to restrict scope
        });

        let decl = self.client.get("kernel.ast.getDecl", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&decl).unwrap_or_default()
        )]))
    }

    #[tool(description = "Get the control flow graph of a function: basic blocks, loop headers (with nesting depth), branch conditions, and goto targets. Returns a structured representation suitable for verification planning.")]
    async fn get_cfg(
        &self,
        Parameters(params): Parameters<GetCfgParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = serde_json::json!({
            "function": params.function,
            "format": params.format.unwrap_or_else(|| "json".into()), // "json" or "dot"
        });

        let cfg = self.client.get("kernel.ast.getCFG", Some(query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&cfg).unwrap_or_default()
        )]))
    }

    // ─── 第七组：Agentic Search 聚合查询  ← v2.2 新增 ───
    //
    // 设计思路：Agent 做 agentic search 时最常见的模式是"从一个线索
    // 出发，多步追踪上下文"。如果每步都是独立 tool call，会产生
    // 5-10 轮往返，增加延迟和 token 消耗。
    //
    // 这组 tool 在 MCP server 端组合多个内部查询，一次返回结构化的
    // 完整上下文。Agent 仍然可以用第一~六组 tool 做细粒度查询，
    // 但对常见的 search pattern，这组 tool 更高效。
    //
    // 实现：纯 Rust 端组合已有 Frama-C 查询，不需要 OCaml 插件。
    //        Phase 2 即可实现。

    #[tool(description = "Trace call chains upward (callers) or downward (callees) from a function, up to max_depth. Returns the complete call tree with source locations and edge annotations. Supports stop_at to prune uninteresting branches (e.g. stop at 'main').")]
    async fn trace_call_chain(
        &self,
        Parameters(params): Parameters<TraceCallChainParams>,
    ) -> Result<CallToolResult, McpError> {
        let max_depth = params.max_depth.unwrap_or(5).min(20);
        let stop_set: std::collections::HashSet<String> =
            params.stop_at.unwrap_or_default().into_iter().collect();

        // BFS/DFS 遍历调用图
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        queue.push_back((params.function.clone(), 0u32));
        visited.insert(params.function.clone());

        while let Some((func, depth)) = queue.pop_front() {
            if depth >= max_depth { continue; }

            let request = match params.direction.as_str() {
                "callers" => "callgraph.getCallers",
                _ => "callgraph.getCallees",
            };
            let query = serde_json::json!({ "function": func });
            let neighbors = self.client.get(request, Some(query)).await
                .map_err(|e| McpError::internal(&e.to_string()))?;

            if let Some(arr) = neighbors.as_array() {
                for n in arr {
                    let name = n.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    result.push(serde_json::json!({
                        "from": if params.direction == "callers" { name } else { &func },
                        "to": if params.direction == "callers" { &func } else { name },
                        "depth": depth + 1,
                        "location": n.get("location"),
                    }));
                    if !visited.contains(name) && !stop_set.contains(name) {
                        visited.insert(name.to_string());
                        queue.push_back((name.to_string(), depth + 1));
                    }
                }
            }
        }

        let output = serde_json::json!({
            "root": params.function,
            "direction": params.direction,
            "depth_reached": max_depth,
            "edges": result,
            "nodes_visited": visited.len(),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&output).unwrap_or_default()
        )]))
    }

    #[tool(description = "Deep investigation of an EVA alarm: combines alarm details, relevant variable value ranges, call chain to the alarm site, data dependencies, control flow context, and existing annotations near the alarm into a single structured report. Depth levels: 'quick' (alarm + values), 'normal' (+ callers + deps), 'deep' (+ full call chain + CFG).")]
    async fn investigate_alarm(
        &self,
        Parameters(params): Parameters<InvestigateAlarmParams>,
    ) -> Result<CallToolResult, McpError> {
        let depth = params.depth.as_deref().unwrap_or("normal");

        // ── Step 1: alarm 详情（所有 depth 级别）
        let alarm_query = serde_json::json!({ "id": params.alarm_id });
        let alarm = self.client.get("eva.getAlarmDetails", Some(alarm_query)).await
            .map_err(|e| McpError::internal(&e.to_string()))?;

        let function = alarm.get("function").and_then(|v| v.as_str()).unwrap_or("");
        let marker = alarm.get("marker").and_then(|v| v.as_str()).unwrap_or("");
        let expression = alarm.get("expression").and_then(|v| v.as_str()).unwrap_or("");

        // ── Step 2: 相关变量值域（所有 depth 级别）
        let value_query = serde_json::json!({
            "function": function, "marker": marker, "expression": expression,
        });
        let values = self.client.get("eva.getValues", Some(value_query)).await
            .unwrap_or(serde_json::json!(null));

        // ── Step 3: 函数注解状态（所有 depth 级别）
        let ann_query = serde_json::json!({ "function": function });
        let annotations = self.client.get("kernel.properties.getStatus", Some(ann_query)).await
            .unwrap_or(serde_json::json!(null));

        let mut report = serde_json::json!({
            "alarm": alarm,
            "value_ranges": values,
            "existing_annotations": annotations,
        });

        // ── Step 4: 调用者 + 数据依赖（normal / deep）
        if depth == "normal" || depth == "deep" {
            let func_query = serde_json::json!({ "function": function });
            let callers = self.client.get("callgraph.getCallers", Some(func_query.clone())).await
                .unwrap_or(serde_json::json!(null));
            let data_deps = self.client.get("from.getFunctionDeps", Some(func_query)).await
                .unwrap_or(serde_json::json!(null));

            report["callers"] = callers;
            report["data_dependencies"] = data_deps;
        }

        // ── Step 5: 完整调用链 + CFG（deep only）
        if depth == "deep" {
            let chain_query = serde_json::json!({ "function": function });
            let call_chain = self.client.get("callgraph.getCallers", Some(chain_query)).await
                .unwrap_or(serde_json::json!(null));
            // TODO: 复用 trace_call_chain 逻辑做多层追踪
            let cfg = self.client.get("kernel.ast.getCFG",
                Some(serde_json::json!({ "function": function, "format": "json" }))).await
                .unwrap_or(serde_json::json!(null));

            report["call_chain"] = call_chain;
            report["control_flow"] = cfg;
        }

        report["investigation_depth"] = serde_json::json!(depth);

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&report).unwrap_or_default()
        )]))
    }
}

// ═══════════════════════════════════════════
//  ServerHandler 实现（使用 #[tool_handler] 宏）
// ═══════════════════════════════════════════

#[tool_handler]
impl ServerHandler for FramaCMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Frama-C formal verification MCP server. Provides CIL AST \
                 code navigation (replacing LSP for verification workflows), \
                 EVA abstract interpretation, WP deductive verification, \
                 ACSL annotation management, and verification planning."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            ..Default::default()
        }
    }
}
```

### 4.4 入口与启动（⚠️ 小幅变更）

```rust
// src/main.rs

use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};  // ← 改：transport::stdio 函数
use tracing_subscriber::{self, EnvFilter};

mod frama_c;
mod mcp;
mod state;
mod error;
mod tools;

use crate::frama_c::{client::FramaCClient, process::FramaCProcess};
use crate::mcp::server::FramaCMcpServer;

#[derive(Parser)]
#[command(name = "frama-c-mcp-server")]
struct Cli {
    /// Frama-C Server ZMQ endpoint (if connecting to existing server)
    #[arg(long, default_value = "tcp://127.0.0.1:5555")]
    zmq_endpoint: String,

    /// C source files to load (starts Frama-C automatically)
    #[arg(long)]
    files: Option<Vec<String>>,

    /// ZMQ port for spawned Frama-C server
    #[arg(long, default_value = "5555")]
    zmq_port: u16,

    /// MCP transport: stdio or http
    #[arg(long, default_value = "stdio")]
    transport: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ← 改：使用 EnvFilter 支持 RUST_LOG 环境变量
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // 可选：自动启动 Frama-C 子进程
    let _frama_c_process = if let Some(files) = &cli.files {
        Some(
            FramaCProcess::spawn(files, cli.zmq_port, &[])
                .await?
        )
    } else {
        None
    };

    // 连接 Frama-C Server
    let client = FramaCClient::connect(&cli.zmq_endpoint).await?;

    // 创建 MCP Server
    let server = FramaCMcpServer::new(client);

    // 启动 MCP（默认 stdio）
    // ← 改：使用 stdio() 函数而不是 (stdin(), stdout()) 元组
    let service = server.serve(stdio()).await?;

    tracing::info!("frama-c-mcp-server running on stdio");

    // 等待退出
    service.waiting().await?;

    Ok(())
}
```

---

## 5. Frama-C Server JSON 协议详解（无变化）

Frama-C Server 的 ZMQ 通信遵循严格的 JSON 消息格式：

### 5.1 请求格式

```json
// GET 请求
{ "kind": "GET", "id": "req-001", "request": "kernel.ast.getFunctions" }

// 带参数的 GET
{ "kind": "GET", "id": "req-002", "request": "eva.getValues",
  "data": { "marker": "#s42", "expression": "x" } }

// SET 请求
{ "kind": "SET", "id": "req-003", "request": "eva.setParams",
  "data": { "precision": 5 } }

// EXEC 请求
{ "kind": "EXEC", "id": "req-004", "request": "eva.compute" }
```

### 5.2 响应格式

```json
// 成功
{ "id": "req-001", "kind": "DATA", "data": [...] }

// 错误
{ "id": "req-001", "kind": "ERROR", "data": { "message": "..." } }

// EXEC 中间信号
{ "id": "req-004", "kind": "SIGNAL",
  "data": { "signal": "eva.progress", "data": { "percent": 50 } } }

// 请求被拒绝
{ "id": "req-003", "kind": "REJECTED",
  "data": { "message": "Analysis is running" } }
```

### 5.3 ZMQ 通信模式

```
Client (REQ)  ────────►  Server (REP)

  GET/SET:     REQ ──JSON──► REP
               REQ ◄──JSON── REP (DATA or ERROR)

  EXEC:        REQ ──JSON──► REP
               REQ ◄──JSON── REP (SIGNAL)  ← 可能多次
               REQ ◄──JSON── REP (DATA)    ← 最终结果
```

> **注意**：ZMQ REQ/REP 是严格交替的，所以 EXEC 的多次 SIGNAL
> 实际上可能需要 Frama-C Server 端特殊处理（如打包多个 signal
> 到一条消息），或使用 DEALER/ROUTER 模式。具体行为需要实际测试
> `frama-c -server-zmq` 确认。

---

## 6. 类型系统设计（无变化）

```rust
// src/frama_c/types.rs

use serde::{Deserialize, Serialize};

/// ──── 项目信息 ────

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub functions: Vec<FunctionSummary>,
    pub global_vars: Vec<GlobalVar>,
    pub total_loc: u32,
    pub parse_warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionSummary {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub loc: u32,
    pub has_annotations: bool,
}

/// ──── EVA 结果 ────

#[derive(Debug, Serialize, Deserialize)]
pub struct EvaAlarm {
    pub id: String,
    #[serde(rename = "type")]
    pub alarm_type: AlarmType,
    pub status: AlarmStatus,
    pub function: String,
    pub file: String,
    pub line: u32,
    pub expression: String,
    pub message: String,
    pub value_info: Option<ValueRange>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlarmType {
    MemAccess,
    DivisionByZero,
    SignedOverflow,
    UnsignedOverflow,
    Initialization,
    PointerValue,
    FloatToInt,
    ShiftWidth,
    Other(String),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlarmStatus {
    Red,     // 确定错误
    Orange,  // 潜在错误（EVA 无法排除）
    Green,   // 已证安全
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValueRange {
    pub variable: String,
    pub range: String,        // e.g. "[0..255]"
    pub bound_needed: Option<String>,
}

/// ──── WP 结果 ────

#[derive(Debug, Serialize, Deserialize)]
pub struct WpGoal {
    pub id: String,
    pub function: String,
    pub property_kind: PropertyKind,
    pub property: String,     // ACSL 表达式
    pub status: GoalStatus,
    pub prover_results: std::collections::HashMap<String, String>,
    pub location: SourceLocation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    Requires,
    Ensures,
    LoopInvariant,
    LoopVariant,
    Assert,
    Assigns,
    Rte,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Valid,
    Unknown,
    Timeout,
    Failed,
}

/// ──── 注解 ────

#[derive(Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    #[serde(rename = "type")]
    pub ann_type: PropertyKind,
    pub content: String,
    pub status: GoalStatus,
    pub verified_by: Option<String>,
}

/// ──── 通用 ────

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GlobalVar {
    pub name: String,
    #[serde(rename = "type")]
    pub var_type: String,
    pub file: String,
}
```

---

## 7. 错误处理（⚠️ 小幅变更）

```rust
// src/error.rs

use thiserror::Error;
use serde_json::Value;

#[derive(Error, Debug)]
pub enum FramaCError {
    #[error("ZMQ communication error: {0}")]
    Zmq(#[from] zeromq::ZmqError),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Frama-C server error: {0}")]
    ServerError(Value),

    #[error("Request rejected (analysis running): {0}")]
    Rejected(Value),

    #[error("Frama-C startup timeout")]
    StartupTimeout,

    #[error("Frama-C process error: {0}")]
    Process(#[from] std::io::Error),

    #[error("UTF-8 decode error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("Unexpected response from server")]
    UnexpectedResponse,

    #[error("Tool error: {0}")]
    Tool(String),
}

/// ← 改：使用 rmcp 0.15 的 ErrorData 类型
impl FramaCError {
    /// 转换为 MCP ErrorData
    pub fn into_mcp_error(self) -> rmcp::ErrorData {
        rmcp::ErrorData::internal(&self.to_string())
    }
}
```

---

## 8. 会话状态（无变化）

```rust
// src/state.rs

use std::collections::HashMap;
use crate::frama_c::types::*;

/// 跟踪当前验证会话的状态
#[derive(Debug, Default)]
pub struct SessionState {
    /// 项目是否已加载
    pub project_loaded: bool,

    /// EVA 是否已执行
    pub eva_completed: bool,
    /// 上次 EVA 参数
    pub eva_params: Option<EvaParams>,

    /// WP 是否已执行
    pub wp_completed: bool,

    /// 已注入的注解（annotation_id → Annotation）
    pub injected_annotations: HashMap<String, Annotation>,

    /// 已知的函数列表（缓存）
    pub functions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EvaParams {
    pub precision: u8,
    pub main_function: String,
    pub slevel: u32,
}
```

---

## 9. MCP Task 支持（⚠️ 新增章节）

MCP 2025-11-25 规范引入了 Task 生命周期，用于处理长时间运行的操作。对 Frama-C 来说，EVA 和 WP 分析可能耗时数分钟，非常适合用 Task 模式。

### 9.1 Task 概念

```
Client                              Server
  │                                    │
  │ call_tool(run_eva, task: true)     │
  │ ─────────────────────────────────► │
  │ ◄── CreateTaskResult(task_id)      │
  │                                    │  ← Server 异步执行 EVA
  │ tasks/get(task_id)                 │
  │ ─────────────────────────────────► │
  │ ◄── TaskInfo(status: working)      │
  │                                    │
  │ tasks/result(task_id)              │
  │ ─────────────────────────────────► │
  │ ◄── CallToolResult(data)           │  ← 完成后返回结果
  │                                    │
```

### 9.2 Task 状态机

```
created → working → completed
                  → failed
                  → cancelled
          → input_required → working
```

### 9.3 实现策略

rmcp 0.15 提供了 `#[task_handler]` 宏和 `OperationProcessor` 工具来管理 Task 生命周期。对于 `run_eva` 和 `run_wp` 这类长时间操作，我们可以：

1. 接收 Task 请求时，启动 EXEC 操作并立即返回 `CreateTaskResult`
2. 后台通过 ZMQ SIGNAL 跟踪进度
3. 客户端通过 `tasks/get` 查询进度
4. 完成后通过 `tasks/result` 返回最终结果

> **Phase 1 策略**：先用同步模式实现（直接等待 EXEC 完成），Task 支持在 Phase 3 或 Phase 4 添加。  
> 这样可以在不增加复杂度的情况下先跑通核心流程。

---

## 10. CIL 代码导航替代 LSP（⚠️ v2.1 新增章节）

### 10.1 设计动机

在验证工作流中，Agent 需要理解代码结构来制定验证策略。通常的做法是接入 clangd 等 LSP server。但在我们的场景下，Frama-C 已经启动且 CIL AST 已在内存中，此时 CIL 是比 LSP 更优的代码导航后端：

| 对比维度 | clangd (LSP) | Frama-C CIL (本方案) |
|---------|-------------|-------------------|
| **启动开销** | 需要额外进程 | Frama-C 已在运行，零额外开销 |
| **AST 信息** | 裸 C AST | 增强 AST：带 ACSL 注解 + 验证状态 + EVA 值域 |
| **依赖分析** | 无 | From 插件：精确的函数级读写依赖 |
| **调用图** | 近似（基于索引） | 精确（CIL 规范化后的解析结果） |
| **值域信息** | 无 | EVA 提供每个程序点的变量值域 |
| **代码修改** | 增量解析，面向频繁编辑 | 不需要——修改仅通过 inject/remove_acsl |
| **一致性** | 独立于分析工具 | AST 和分析结果天然一致 |

### 10.2 关键前提

CIL 替代 LSP 成立的前提条件：

1. **Frama-C 已启动**：CIL AST 已在内存中，不存在额外解析延迟
2. **代码修改受控**：仅通过 `inject_acsl` / `remove_acsl` 修改注解，不直接编辑 C 源码
3. **查询目的是验证**：Agent 需要理解代码结构来决定验证策略，不是在写新代码
4. **Frama-C 自动维护一致性**：注解修改后 CIL AST 和分析结果会同步更新

如果未来需要支持**源码编辑**场景（如 Agent 自动修复 C 代码），CIL 就不够了——那时应该同时接入 clangd 处理编辑状态，而 Frama-C 只在编辑完成后重新加载。

### 10.3 第六组 Tool 总览（5 个 tool）

| Tool | 功能 | Frama-C 后端 | 阶段 |
|------|------|-------------|------|
| `find_callers` | 查找函数的所有调用点 | `callgraph.getCallers` | Phase 1 |
| `get_data_deps` | 函数的数据依赖（读写的全局变量/参数） | `from.getFunctionDeps` | Phase 2 |
| `find_memory_ops` | 函数中所有指针解引用/数组访问/动态分配 | VP-Bridge CIL 遍历 | Phase 3 |
| `lookup_symbol` | 标识符的类型签名、定义位置、作用域 | `kernel.ast.getDecl` | Phase 1 |
| `get_cfg` | 控制流图：基本块、循环头、分支条件 | `kernel.ast.getCFG` | Phase 2 |

### 10.4 Agent 工具链全景

```
Claude Agent (验证规划器)
│
├── 文本工具（偶尔需要看原始源码）
│   ├── Read file             → 查看带行号的源码
│   └── Grep / Glob           → 文件名/文本模式搜索
│
└── frama-c MCP Server        → 一站式验证后端（20 tools）
    │
    ├── ① 项目初始化 (3 tools)
    │   ├── load_project       → 加载 C 文件，获取项目概览
    │   ├── get_callgraph      → 全项目调用图
    │   └── get_function_info  → 单函数详细信息
    │
    ├── ② EVA 分析 (3 tools)
    │   ├── run_eva            → 运行抽象解释
    │   ├── get_eva_alarms     → 获取/过滤 alarm 列表
    │   └── get_eva_value      → 查询程序点值域
    │
    ├── ③ WP 验证 (2 tools)
    │   ├── run_wp             → 运行演绎验证
    │   └── get_wp_goals       → 获取/过滤证明目标
    │
    ├── ④ ACSL 注解 (3 tools)   ← 唯一的代码修改通道
    │   ├── inject_acsl        → 注入注解
    │   ├── remove_acsl        → 移除注解
    │   └── get_current_annotations → 查看注解及状态
    │
    ├── ⑤ 验证规划 (2 tools)
    │   ├── get_verification_status  → 全局验证状态
    │   └── suggest_verification_plan → 推荐下一步
    │
    ├── ⑥ CIL 代码导航 (5 tools) ← 替代 LSP
    │   ├── find_callers       → "谁调用了这个函数？"
    │   ├── get_data_deps      → "这个函数读写了什么？"
    │   ├── find_memory_ops    → "哪里有指针操作？"
    │   ├── lookup_symbol      → "这个类型/变量的定义？"
    │   └── get_cfg            → "这个函数的控制流？"
    │
    └── ⑦ Agentic Search (2 tools) ← 聚合查询
        ├── trace_call_chain   → 多层调用链一次追踪
        └── investigate_alarm  → alarm 深度调查（一次获取完整上下文）
```

### 10.5 第七组 Tool：Agentic Search 聚合查询（v2.2 新增）

**设计动机**：Agent 做验证分析时最常见的模式是"从一个线索出发，多步追踪上下文"——拿到一个 alarm 后需要查值域、查调用链、查数据依赖、查已有注解。如果每步都是独立 tool call，会产生 5-10 轮往返。第七组在 MCP server 端组合多个内部查询，一次返回完整上下文。

| Tool | 功能 | 实现方式 | 阶段 |
|------|------|---------|------|
| `trace_call_chain` | 多层调用链追踪（向上/向下，可设深度和终止点） | Rust BFS 遍历 callgraph | Phase 2 |
| `investigate_alarm` | alarm 深度调查（alarm 详情 + 值域 + 调用者 + 依赖 + 注解 + CFG） | Rust 组合多个 GET 查询 | Phase 2 |

**Agentic Search 典型工作流示例**：

```
Agent 任务："找出程序中所有可能的缓冲区溢出风险"

步骤 1: get_callgraph()
        → 理解全局结构，识别入口点和叶子函数

步骤 2: run_eva(precision: 3)
        → 粗粒度分析，快速定位可疑区域

步骤 3: get_eva_alarms(alarm_type: "mem_access")
        → 定位: sort() line 42, process_input() line 87

步骤 4: investigate_alarm(alarm_id: "alarm-87", depth: "deep")   ← 聚合查询
        → 一次获得:
          - alarm 详情: process_input:87, *(buf + offset)
          - 值域: offset ∈ [0..4294967295]（无上界！）
          - 调用链: main → parse_args → process_input
          - 数据依赖: reads global input_buffer, param size
          - 控制流: 无 if(offset < buf_size) 分支
          - 已有注解: 无

步骤 5: inject_acsl(function: "process_input",
                    type: "requires", content: "offset < buf_size")

步骤 6: run_wp(functions: ["process_input"])
        → 验证修复是否充分
```

如果没有 `investigate_alarm`，步骤 4 需要分解为 5-6 次独立 tool call。

**实现说明**：这两个 tool 是纯 Rust 端逻辑——在 MCP server 内部组合调用 `FramaCClient` 的多个 GET 请求，不需要 OCaml 插件支持，Phase 2 即可实现。

### 10.6 OCaml VP-Bridge 插件需要暴露的 CIL 遍历 API

Phase 3 需要开发的自定义 OCaml 插件中，以下 CIL 遍历是第六组 tool 的后端：

```ocaml
(* vp_bridge.ml — 需要注册到 Frama-C Server 的 request *)

(* find_memory_ops: 遍历 CIL AST 中的内存操作节点 *)
(* 使用 Frama-C Visitor API:
   - Mem(e)        → 指针解引用 *p
   - Index(e,i)    → 数组访问 a[i]
   - Call(_, Malloc/Free, _) → 动态内存操作
   返回: [{kind, expression, function, file, line, marker}] *)

(* get_data_deps: 如果 From 插件未运行，需要先触发 *)
(* 封装 From.access / From.deps_of_fundec *)

(* get_cfg: 将 CIL 的 stmt.succs/preds 转为 JSON 图结构 *)
(* 标注 loop headers (Cil.is_loop), switch 分支等 *)
```

> **注意**：`find_callers` 和 `lookup_symbol` 可以直接使用 Frama-C 内置 API（`callgraph.*` 和 `kernel.ast.*`），不需要自定义插件。`get_cfg` 可以先用 `-cfg` 命令行输出 DOT 格式，Phase 3 再改为 JSON 直接查询。

---

## 11. Claude Desktop / Agent 集成配置

### 11.1 Claude Desktop `claude_desktop_config.json`

```json
{
  "mcpServers": {
    "frama-c": {
      "command": "/path/to/frama-c-mcp-server",
      "args": [
        "--zmq-endpoint", "tcp://127.0.0.1:5555",
        "--transport", "stdio"
      ]
    }
  }
}
```

### 11.2 Claude Agent SDK 集成

```python
# agent.py — Claude Agent SDK 调用示例
import anthropic

client = anthropic.Anthropic()

response = client.messages.create(
    model="claude-sonnet-4-20250514",
    max_tokens=4096,
    tools=[
        {"type": "mcp", "server": "frama-c"}
    ],
    messages=[{
        "role": "user",
        "content": "验证 sort.c 的内存安全性，请一步步进行分析"
    }]
)
```

### 11.3 自动启动模式

```bash
# 模式 A：连接已运行的 Frama-C Server
frama-c -server-zmq tcp://127.0.0.1:5555 sort.c &
frama-c-mcp-server --zmq-endpoint tcp://127.0.0.1:5555

# 模式 B：MCP Server 自动管理 Frama-C 生命周期
frama-c-mcp-server --files sort.c --zmq-port 5555
# → 内部自动 spawn frama-c -server-zmq，退出时自动清理
```

---

## 12. 构建与测试

```bash
# 构建（需要 nightly，edition 2024）
rustup override set nightly
cargo build --release

# 单元测试（不需要 Frama-C）
cargo test --lib

# 集成测试（需要 frama-c 在 PATH 中）
cargo test --test integration

# 运行
cargo run -- --zmq-endpoint tcp://127.0.0.1:5555

# 使用 MCP Inspector 调试
npx @modelcontextprotocol/inspector cargo run
```

---

## 13. 对比：Python vs Rust 实现（更新）

| 维度 | Python (原方案) | Rust (新方案) |
|------|----------------|--------------|
| **启动速度** | ~500ms (Python 解释器) | ~5ms (原生二进制) |
| **内存占用** | ~50MB | ~5MB |
| **类型安全** | 运行时检查 | 编译时保证 |
| **ZMQ 集成** | pyzmq（C 绑定） | zeromq（纯 Rust）或 zmq（C 绑定） |
| **MCP SDK** | mcp-python | rmcp 0.15+（官方） |
| **MCP 协议版本** | 取决于 SDK 版本 | 2025-11-25（最新） |
| **Task 支持** | 需要手动实现 | rmcp 内置 `#[task_handler]` |
| **错误处理** | Exception（可遗漏） | Result<T,E>（强制处理） |
| **分发** | 需要 Python 环境 | 单一静态二进制 |
| **开发速度** | 更快迭代 | 初始慢，后续更稳定 |
| **异步模型** | asyncio | tokio（更成熟） |
| **适合阶段** | 快速原型 | 生产部署 |

---

## 14. 实施路线图（更新版）

### Phase 1: 基础通信（2 周）
- [x] 设计文档 v1
- [x] 设计文档 v2（rmcp 0.15 适配）
- [x] 设计文档 v2.1（CIL 代码导航 tool）
- [x] 设计文档 v2.2（Agentic Search tool）
- [ ] `cargo init` 项目骨架 + rust-toolchain.toml
- [ ] 实现 `FramaCClient`（ZMQ REQ/REP）
- [ ] 实现 `FramaCProcess`（子进程管理）
- [ ] 本地 `frama-c -server-zmq` 联调，确认协议细节
- [ ] 3 个基础 tool: `load_project`, `run_eva`, `get_eva_alarms`
- [ ] 2 个导航 tool（内置 API）: `find_callers`, `lookup_symbol`
- [ ] MCP Inspector 端到端测试

### Phase 2: 完整 Tool 集（2-3 周）
- [ ] 补全剩余验证 tool（第二～五组）
- [ ] 导航 tool: `get_cfg`, `get_data_deps`（需要 From 插件）
- [ ] Agentic Search: `trace_call_chain`, `investigate_alarm`  ← v2.2
- [ ] 实现 SessionState 状态管理
- [ ] 类型系统完善（处理 Frama-C 实际返回的 JSON 结构）
- [ ] 集成测试：sort.c 端到端验证流程
- [ ] 错误处理完善 + tracing 日志

### Phase 3: 自定义 Frama-C 插件 + Task 支持（3-4 周）
- [ ] OCaml `verification-planner-bridge` 插件
- [ ] `inject_acsl` / `remove_acsl` 实现
- [ ] `get_verification_status` 聚合查询
- [ ] `find_memory_ops`（VP-Bridge CIL Visitor 遍历）
- [ ] MCP Task 支持（`run_eva` / `run_wp` 异步执行）
- [ ] LLM → ACSL → WP → 反馈迭代循环

### Phase 4: Agent 集成（2 周）
- [ ] Claude Agent SDK / Claude Code 接入
- [ ] Agent prompt engineering（验证策略 + CIL 导航 + Agentic Search 联动）
- [ ] Agentic Search 效果评估：对比逐步调用 vs 聚合查询的轮次和 token 消耗  ← v2.2
- [ ] 千行级程序测试
- [ ] 性能调优 + 文档
