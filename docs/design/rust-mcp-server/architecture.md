# Rust MCP Server 架构文档

> **阶段**：架构设计
> **日期**：2026-02-19
> **前置**：[调研报告](../../research/frama-c-server-api-research.md)
> **架构决策**：[architecture.md](../../architecture.md)

---

## 目录

1. [核心数据流](#1-核心数据流)
2. [模块划分](#2-模块划分)
3. [接口规约](#3-接口规约)
4. [关键设计决策](#4-关键设计决策)
5. [Phase 1 工具实现映射](#5-phase-1-工具实现映射)
6. [依赖列表](#6-依赖列表)

---

## 1. 核心数据流

### 1.1 整体请求路径

```
Agent ←─ MCP (JSON-RPC/stdio) ─→ Rust (rmcp 0.16)
                                    │
                                    ├─ Tool Handler（编排逻辑）
                                    │    ├─ 解析 MCP 参数
                                    │    ├─ name→marker 解析（查 SessionState 缓存）
                                    │    ├─ 调用 FramaCClient.get/set/exec/fetch_all
                                    │    ├─ 组装返回结果
                                    │    └─ 更新 SessionState
                                    │
                                    └─ FramaCClient
                                         ├─ codec: 序列化 JSON + S/L 分帧编码
                                         ├─ transport: tokio UnixStream 写入/读取
                                         └─ codec: S/L 分帧解码 + 反序列化 JSON
                                              │
                                    Unix Socket (/tmp/frama-c-<pid>.sock)
                                              │
                                         Frama-C Server
```

### 1.2 连接生命周期

```
                  Rust Client                         Frama-C Server
                      │                                    │
     connect()        │ ──── TCP/Unix connect ──────────→  │
                      │                                    │ 解析命令行参数
                      │ ◄──── "CMDLINEON" ──────────────── │ （若有 -then 等命令行指令）
                      │                                    │ 执行命令行指令...
                      │ ◄──── "CMDLINEOFF" ─────────────── │ 就绪
     ready!           │                                    │
                      │ ──── GET/SET/EXEC ──────────────→  │
                      │ ◄──── DATA/ERROR ───────────────── │
                      │                                    │
                      │            ...                     │
                      │                                    │
     shutdown()       │ ──── "SHUTDOWN" ────────────────→  │
                      │                                    │ 退出
```

**连接流程**：

1. `FramaCClient::connect(socket_path)` → 建立 tokio `UnixStream` 连接
2. 进入接收循环，等待 `"CMDLINEOFF"` 信号
3. 如果先收到 `"CMDLINEON"`，说明 Frama-C 正在执行命令行指令，继续等待
4. 收到 `"CMDLINEOFF"` 后标记就绪
5. 自动执行 `fetch_all("kernel.ast.fetchFunctions")` 填充 SessionState marker 缓存
6. `connect()` 返回
7. 如果 30 秒内未收到 `"CMDLINEOFF"`，返回超时错误

**设计说明**：Frama-C Server 在启动时已通过命令行加载 C 文件并构建 AST（CMDLINEON/CMDLINEOFF 阶段）。无需 `load_project` 工具重复加载——连接就绪后自动获取项目信息即可。

### 1.3 GET 流程（同步请求-响应）

```
Client                                Server
  │                                      │
  │  ── S/L frame ──────────────────→    │
  │  { "cmd":"GET", "id":"RQ.0",         │
  │    "request":"kernel.ast.getFiles",  │
  │    "data": null }                    │  立即执行（不排队）
  │                                      │
  │  ◄── S/L frame ─────────────────     │
  │  { "res":"DATA", "id":"RQ.0",       │
  │    "data": ["/tmp/test.c"] }         │
  │                                      │
```

**特性**：GET 在 Frama-C Server 中立即执行（不进入命令队列），即使 EXEC 正在运行也能响应。

### 1.4 EXEC 流程（异步 + POLL）

```
Client                                       Server
  │                                              │
  │  ── EXEC ─────────────────────────────→      │
  │  { "cmd":"EXEC", "id":"RQ.1",               │  排队，开始执行
  │    "request":"plugins.eva.general.compute",  │
  │    "data": null }                            │
  │                                              │
  │  ── POLL ─────────────────────────────→      │  （100ms 后）
  │  "POLL"                                      │
  │  ◄── SIGNAL ──────────────────────────       │  中间信号
  │  { "res":"SIGNAL", "id":"RQ.1" }            │
  │                                              │
  │  ── POLL ─────────────────────────────→      │  （又 100ms）
  │  "POLL"                                      │
  │  ◄── SIGNAL ──────────────────────────       │
  │  { "res":"SIGNAL", "id":"RQ.1" }            │
  │                                              │
  │  ── POLL ─────────────────────────────→      │  （又 100ms）
  │  "POLL"                                      │
  │  ◄── DATA ────────────────────────────       │  最终结果
  │  { "res":"DATA", "id":"RQ.1",               │
  │    "data": null }                            │
  │                                              │
```

**POLL 机制**：

- EXEC 发送后，Server 不会主动推送结果
- Client 必须发送 `"POLL"` 命令触发 Server 检查并返回待发送的响应
- 每次 POLL 最多返回一条响应（SIGNAL / DATA / ERROR / KILLED）
- 无待发送响应时 POLL 无回复（需超时处理）
- POLL 间隔：100ms（Ivette 默认 50ms，考虑 MCP 场景 100ms 即可）
- 终止条件：收到 `DATA`、`ERROR` 或 `KILLED` 响应

**POLL 超时策略**：

- 单次 POLL 读取超时：200ms（无响应视为"无待发送消息"）
- EXEC 总超时：由调用方设定（EVA/WP 可能运行数分钟，默认 600 秒）
- 超时后发送 `KILL` 命令取消操作

### 1.5 Fetch 分页流程

```
Client                                       Server
  │                                              │
  │  ── GET(fetchFunctions, 20000) ────────→     │
  │  { "cmd":"GET", "id":"RQ.2",                │
  │    "request":"kernel.ast.fetchFunctions",   │
  │    "data": 20000 }                          │
  │                                              │
  │  ◄── DATA ────────────────────────────       │
  │  { "res":"DATA", "id":"RQ.2",               │
  │    "data": {                                │
  │      "reload": false,                       │
  │      "updated": [ ...batch... ],            │
  │      "removed": [],                         │
  │      "pending": 5                           │  ← 还有 5 条
  │    }                                        │
  │  }                                          │
  │                                              │
  │  ── GET(fetchFunctions, 20000) ────────→     │  继续取
  │  ...                                         │
  │  ◄── DATA ────────────────────────────       │
  │  { "data": {                                │
  │      "updated": [ ...剩余... ],             │
  │      "pending": 0                           │  ← 全部取完
  │    }                                        │
  │  }                                          │
```

**分页协议**（参考 Ivette `states.ts:429-442`，验证 `server/states.ml:297-330`）：

- 请求参数 `data` 为 **batch capacity**（整数，最大返回条目数）
  - Ivette 使用 20000
  - 注意：调研报告 §2.4 描述为"起始行号"有误；§4.1 已纠正为容量限制
  - 验证依据：`states.ml:302` `capacity = n`，每条目消耗 1 capacity（`states.ml:287`）
- 响应 `data` 包含 `{ reload, updated, removed, pending }`
- **终止条件**：`pending == 0`
- `reload == true` 时需清空本地缓存，用 `updated` 重建
- `updated` 是本批次新增/更新的条目数组
- `removed` 是本批次删除的条目 key 数组
- 每次调用都发送相同的 batch capacity（20000），不需要递增偏移量

---

## 2. 模块划分

### 2.0 源文件结构

```
src/
├── main.rs                 # CLI 参数、启动 Frama-C 连接、启动 MCP 服务
├── error.rs                # 统一错误类型 FramaCError + 到 McpError 的转换
├── state.rs                # SessionState：marker 缓存、分析状态标志
├── frama_c/
│   ├── mod.rs              # 模块 re-export
│   ├── codec.rs            # S/L/W 分帧 + JSON 序列化/反序列化
│   ├── transport.rs        # tokio UnixStream 连接、读写
│   └── client.rs           # 高层 API：get/set/exec/fetch_all + POLL 循环
└── mcp/
    ├── mod.rs              # 模块 re-export
    ├── server.rs           # FramaCMcpServer + #[tool_router] Phase 1 工具
    └── types.rs            # MCP 参数/返回类型（Deserialize + JsonSchema）
```

### 2.1 模块 `frama_c::codec`

**功能描述**：实现 Frama-C Server 自定义协议的编解码，包括 S/L/W 分帧和 JSON 命令/响应的序列化/反序列化。

**前置条件（Requires）**：
- 输入的 JSON 值是合法的 `serde_json::Value`
- 网络字节流以 Frama-C Server 协议格式编码

**后置条件（Ensures）**：
- `encode_frame(data) → bytes`：输出的字节流以 `S`/`L`/`W` + 小写 hex 长度 + payload 格式编码
  - `len(data) ≤ 0xFFF` → `S` + 3 hex digits
  - `0xFFF < len(data) ≤ 0xFFFFFFF` → `L` + 7 hex digits
  - `len(data) > 0xFFFFFFF` → `W` + 15 hex digits
- `decode_frame(buf) → Option<(String, usize)>`：从 buf 中尝试解码一个完整帧
  - 返回 `Some((payload_string, consumed_bytes))` 或 `None`（数据不完整）
  - 不会 panic，不完整帧返回 None
- `encode_command(cmd) → String`：将 `FramaCCommand` 序列化为 JSON 字符串
  - GET/SET/EXEC → `{"cmd":"<kind>","id":"<id>","request":"<name>","data":<json>}`
  - POLL → `"POLL"`
  - SHUTDOWN → `"SHUTDOWN"`
  - KILL → `{"cmd":"KILL","id":"<request-id>"}`
  - SIGON/SIGOFF → `{"cmd":"SIGON"/"SIGOFF","id":"<signal-id>"}`
- `decode_response(json_str) → FramaCResponse`：将 JSON 字符串反序列化为 `FramaCResponse`
  - 对象类型 → 按 `"res"` 字段分派（DATA/ERROR/SIGNAL/REJECTED/KILLED）
  - 字符串类型 → CMDLINEON / CMDLINEOFF

**不变式（Invariants）**：
- hex 编码使用小写字母（与 OCaml `Printf.sprintf "%03x"` 一致）
- 编解码互逆：`decode_frame(encode_frame(data)) == data`

**副作用**：无

**关键类型**：

```rust
/// 客户端发送的命令
pub enum FramaCCommand {
    Get { id: String, request: String, data: serde_json::Value },
    Set { id: String, request: String, data: serde_json::Value },
    Exec { id: String, request: String, data: serde_json::Value },
    Poll,
    Shutdown,
    Kill { id: String },
    SigOn { id: String },
    SigOff { id: String },
}

/// 服务端返回的响应
pub enum FramaCResponse {
    Data { id: String, data: serde_json::Value },
    Error { id: String, msg: String },
    Signal { id: String },
    Rejected { id: String },
    Killed { id: String },
    CmdLineOn,
    CmdLineOff,
}
```

### 2.2 模块 `frama_c::transport`

**功能描述**：管理 tokio `UnixStream` 连接，提供帧级别的读写操作。内部维护读缓冲区以处理 TCP 流的粘包/拆包。

**前置条件（Requires）**：
- `connect(path)`：`path` 指向一个已存在的 Unix Socket 文件
- `send_frame(data)`：连接已建立且未关闭
- `recv_frame()`：连接已建立且未关闭

**后置条件（Ensures）**：
- `connect(path) → Result<Transport>`：成功建立 UnixStream 连接，初始化读缓冲区
- `send_frame(data) → Result<()>`：将 data 编码为帧（通过 `codec::encode_frame`）并写入 stream，确保完整写入
- `recv_frame(timeout) → Result<Option<String>>`：
  - 从 stream 读取数据追加到缓冲区，尝试解码一帧
  - 返回 `Ok(Some(payload))` 表示成功解码一帧
  - 返回 `Ok(None)` 表示超时（timeout 内未收到完整帧）
  - 返回 `Err(...)` 表示连接断开或 I/O 错误
- `close() → Result<()>`：关闭连接，释放资源

**不变式（Invariants）**：
- 读缓冲区中始终保存尚未解码完成的部分帧数据
- 单次 `recv_frame` 最多返回一帧（即使缓冲区中有多帧数据）

**副作用**：网络 I/O

**关键类型**：

```rust
pub struct Transport {
    stream: tokio::net::UnixStream,
    read_buf: bytes::BytesMut,
}
```

### 2.3 模块 `frama_c::client`

**功能描述**：提供与 Frama-C Server 交互的高层 API。封装连接握手（等待 CMDLINEOFF）、GET/SET 同步调用、EXEC + POLL 异步等待循环、fetch_all 分页循环。内部持有 Transport 和请求 ID 计数器。通过 `Mutex` 保证请求串行化。

**前置条件（Requires）**：
- `connect(path)`：Frama-C Server 已通过 `-server-socket <path>` 启动
- `get/set/exec`：`connect` 已成功返回（CMDLINEOFF 已收到）
- `exec`：`timeout > 0`

**后置条件（Ensures）**：
- `connect(path, state) → Result<FramaCClient>`：
  - 建立 Transport 连接
  - 等待 CMDLINEOFF（最多 30 秒）
  - 自动 `fetch_all("kernel.ast.fetchFunctions")` 填充 state 的 marker 缓存
  - 成功后返回 ready 状态的 client（SessionState.project_loaded == true）
- `get(request, data) → Result<Value>`：
  - 发送 GET 命令（GET 由服务器立即执行）
  - 通过 `wait_for_id` 等待匹配请求 ID 的 DATA 响应
  - 跳过 SIGNAL、CMDLINE 和其他 ID 的过期响应
  - 返回 `Ok(data)` 或 `Err(ServerError(msg))` / `Err(Rejected)`
- `set(request, data) → Result<Value>`：
  - 发送 SET 命令（SET 被服务器排队，非即时执行）
  - 通过 `poll_loop` 反复发送 POLL 触发服务器处理队列
  - 等待匹配请求 ID 的 DATA 响应
  - 返回与 GET 相同的类型
- `exec(request, data, timeout) → Result<Value>`：
  - 发送 EXEC 命令（EXEC 被服务器排队，异步执行）
  - 启动 POLL 循环（100ms 间隔），直到收到 DATA/ERROR/KILLED/REJECTED
  - 超时后发送 KILL 并返回 `Err(Timeout)`
  - SIGNAL 响应被消费但不返回（Phase 1 不需要进度回报）
- `fetch_all(request) → Result<Vec<Value>>`：
  - 发送 GET(request, 20000) 循环
  - 累积 `updated` 数组内容
  - `pending == 0` 时终止循环
  - 返回所有 `updated` 条目的合并数组
- `shutdown() → Result<()>`：
  - 发送 SHUTDOWN 命令
  - 关闭 Transport 连接

**不变式（Invariants）**：
- 请求 ID 单调递增：`format!("RQ.{}", counter)`，counter 从 0 开始
- Mutex 保证同一时刻只有一个请求在通信中（Phase 1 串行模型）
- POLL 循环期间不接受其他请求（由 Mutex 保证）

**副作用**：通过 Transport 进行网络 I/O

**并发规约**：

```
并发单元：FramaCClient

共享资源：
  - inner: Mutex<ClientInner>（包含 Transport + counter）

锁协议：
  - get/set/exec/shutdown：获取 inner 锁，持有锁完成完整的请求-响应交互（包括 POLL 循环）
  - fetch_all：多次独立调用 get()，每次 get() 独立获取和释放锁
    - 不在循环中长期持有锁
    - Phase 1 MCP 串行调用下不会出现交错
  - 不存在嵌套锁

顺序约束：
  - connect() must happen-before 任何 get/set/exec 调用
  - 同一时刻最多一个 get/set/exec 请求在处理中

线程安全性结论：
  - FramaCClient 是线程安全的（Mutex 保护）
  - Phase 1 下所有 MCP 工具调用通过 Mutex 串行化
```

**关键类型**：

```rust
pub struct FramaCClient {
    inner: tokio::sync::Mutex<ClientInner>,
}

struct ClientInner {
    transport: Transport,
    counter: u64,
}
```

### 2.4 模块 `state`

**功能描述**：维护 MCP 会话的状态信息，包括函数 marker 缓存（name → marker 映射）、分析状态标志（EVA/WP 是否已运行）、项目加载状态。被 MCP 工具层读写。

**前置条件（Requires）**：
- `resolve_function(name)`：`functions` 缓存已填充（`connect()` 已成功返回）
- `invalidate_all()`：无

**后置条件（Ensures）**：
- `resolve_function(name) → Option<FunctionInfo>`：
  - 在缓存中查找函数名，返回对应的 marker 和元信息
  - 未找到返回 None
- `update_functions(entries)`：
  - 用 `fetch_all("kernel.ast.fetchFunctions")` 的结果更新缓存
  - 构建 name → FunctionInfo 映射
- `invalidate_all()`：
  - 清空所有缓存和状态标志
  - `project_loaded = false`，`eva_completed = false`，`wp_completed = false`
- `set_eva_completed()` / `set_wp_completed()`：
  - 设置对应标志为 true

**不变式（Invariants）**：
- `project_loaded == true` ⟹ `functions` 缓存非空
- `eva_completed == true` ⟹ `project_loaded == true`
- `wp_completed == true` ⟹ `project_loaded == true`

**副作用**：无（纯内存状态）

**关键类型**：

```rust
pub struct SessionState {
    pub project_loaded: bool,
    pub eva_completed: bool,
    pub wp_completed: bool,
    /// 函数名 → FunctionInfo（包含 marker、文件、行号）
    pub functions: HashMap<String, FunctionInfo>,
}

pub struct FunctionInfo {
    pub name: String,
    pub marker: String,       // e.g. "kf#24"（函数 marker）
    pub declaration: String,  // e.g. "#F24"（声明 marker，用于 printDeclaration）
    pub signature: String,    // e.g. "int abs_val(int x);"
    pub file: String,         // 来自 sloc.file
    pub line: u32,            // 来自 sloc.line
}
```

### 2.5 模块 `mcp::server`

**功能描述**：实现 MCP ServerHandler，使用 rmcp 的 `#[tool_router]` 和 `#[tool_handler]` 宏注册 Phase 1 的 8 个工具。每个工具方法编排一个或多个 `FramaCClient` 调用，处理 name→marker 解析，组装 MCP 返回结果。

**前置条件（Requires）**：
- 构造时传入已连接就绪的 `FramaCClient`

**后置条件（Ensures）**：
- 每个 `#[tool]` 方法返回 `Result<CallToolResult, McpError>`
- 工具方法内部错误通过 `FramaCError → McpError` 转换传播
- 状态变更通过 `Arc<RwLock<SessionState>>` 原子更新

**不变式（Invariants）**：
- 工具层不直接操作 Transport 或 codec，只通过 `FramaCClient` 的 get/set/exec/fetch_all 方法
- 所有 Frama-C request 名称使用调研报告中验证过的正确名称

**副作用**：通过 FramaCClient 间接进行网络 I/O；读写 SessionState

### 2.6 模块 `mcp::types`

**功能描述**：定义 MCP 工具的参数类型和返回类型。参数类型实现 `Deserialize + JsonSchema`（用于 MCP schema 自动生成）。返回类型为纯 Rust 结构体，通过 `serde_json::to_string_pretty` 转为 JSON 文本。

**前置条件（Requires）**：无（纯数据类型定义）

**后置条件（Ensures）**：
- 所有参数类型都实现 `Deserialize` 和 `schemars::JsonSchema`
- JsonSchema 的 `description` 字段准确描述每个参数的语义和取值范围

**副作用**：无

### 2.7 模块 `error`

**功能描述**：定义统一错误类型 `FramaCError`，覆盖通信、协议、业务三层错误。提供到 rmcp `McpError` 的转换。

**前置条件（Requires）**：无

**后置条件（Ensures）**：
- `FramaCError` 涵盖：I/O 错误、JSON 编解码错误、协议错误（无效帧格式）、Server 业务错误（ERROR/REJECTED/KILLED）、超时
- `impl From<FramaCError> for McpError`：所有 FramaCError 变体都能转换为 McpError（`McpError` 即 `rmcp::ErrorData`）
  - ServerError → `McpError::internal_error(msg, None)` (code -32603)
  - Rejected → `McpError::invalid_request(format!("rejected: {id}"), None)` (code -32600)
  - Timeout → `McpError::internal_error("operation timed out", None)` (code -32603)
  - 其他 → `McpError::internal_error(error.to_string(), None)` (code -32603)

**副作用**：无

**关键类型**：

```rust
#[derive(thiserror::Error, Debug)]
pub enum FramaCError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid frame: {0}")]
    InvalidFrame(String),

    #[error("Frama-C server error [{id}]: {msg}")]
    ServerError { id: String, msg: String },

    #[error("Request rejected [{id}]")]
    Rejected { id: String },

    #[error("Request killed [{id}]")]
    Killed { id: String },

    #[error("Connection timeout: waiting for CMDLINEOFF")]
    ConnectTimeout,

    #[error("Operation timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Unexpected response: {0}")]
    UnexpectedResponse(String),
}
```

---

## 3. 接口规约

### 3.1 接口：`main` → `FramaCClient`

**功能**：建立连接并完成就绪握手。

```
接口：main → FramaCClient

输入数据：
  - socket_path: &str（Unix Socket 路径，如 "/tmp/frama-c.sock"）
  - state: Arc<RwLock<SessionState>>（用于填充 marker 缓存）
输出数据：Result<FramaCClient, FramaCError>

协议约定：
  - 调用方的责任：
    - 确保 Frama-C Server 已通过 `frama-c <files> -server-socket <path>` 启动
    - socket_path 与 Frama-C Server 的 -server-socket 参数一致
  - 被调用方的责任：
    - 建立 Unix Socket 连接
    - 接收并正确解码 S/L 帧
    - 等待 CMDLINEOFF 信号（最多 30 秒）
    - CMDLINEOFF 收到后自动 fetch_all("kernel.ast.fetchFunctions") 填充 state
    - 返回 ready 状态的 client（state.project_loaded == true）
    - 连接失败或超时返回错误
```

### 3.2 接口：`main` → `FramaCMcpServer`

**功能**：注入 client 和初始状态，启动 MCP stdio 服务。

```
接口：main → FramaCMcpServer

输入数据：
  - client: FramaCClient（已连接就绪）
输出数据：FramaCMcpServer 实例

协议约定：
  - 调用方的责任：
    - client 已通过 connect() 成功返回
    - 调用 server.serve(stdio()).await 启动 MCP 服务
    - 调用 service.waiting().await 等待退出
  - 被调用方的责任：
    - 将 client 包装为 Arc 共享给所有工具方法
    - 构造时通过 `Self::tool_router()` 初始化 `ToolRouter<Self>`（由 `#[tool_router]` 宏生成）
    - 接收 main 传入的 SessionState（connect() 阶段已填充 marker 缓存）
    - 通过 `#[tool_handler(router = self.tool_router)]` 自动注册 list_tools 和 call_tool
  - rmcp 补充：
    - `serve()` 方法来自 `rmcp::ServiceExt` trait，main 需 `use rmcp::ServiceExt`
    - stdio 传输通过 `rmcp::transport::io::stdio()` 获取
```

### 3.3 接口：`mcp::server` → `frama_c::client`

**功能**：工具层通过 client 高层 API 与 Frama-C Server 交互。

```
接口：mcp::server → frama_c::client

输入数据：
  - get(request: &str, data: Value) → Result<Value>
  - set(request: &str, data: Value) → Result<Value>
  - exec(request: &str, data: Value, timeout: Duration) → Result<Value>
  - fetch_all(request: &str) → Result<Vec<Value>>

输出数据：Result<Value, FramaCError> 或 Result<Vec<Value>, FramaCError>

协议约定：
  - 调用方（工具层）的责任：
    - request 名称使用调研报告中验证过的正确名称
      （如 "kernel.ast.fetchFunctions"，不是 "kernel.ast.getFunctionInfo"）
    - data 参数格式符合 Frama-C Server 该 request 的预期
    - 对返回的 Value 做类型检查（可能为 null、array、object）
    - 将 FramaCError 转换为 McpError 返回给 MCP 层
  - 被调用方（client）的责任：
    - 自动分配唯一递增的 request ID
    - GET/SET: 发送命令、等待对应 ID 的响应、返回 data 或 error
    - EXEC: 发送命令、POLL 循环、消费 SIGNAL、返回最终 DATA 或 error
    - fetch_all: 分页循环、累积 updated 数组、pending==0 终止
    - 超时后返回 Err(Timeout)
```

### 3.4 接口：`mcp::server` → `state`

**功能**：工具层读写会话状态（marker 缓存和分析标志）。

```
接口：mcp::server → state::SessionState

输入数据：通过 Arc<RwLock<SessionState>> 访问
输出数据：读取返回 Clone 的数据；写入无返回

协议约定：
  - 调用方的责任：
    - 读操作用 read()，写操作用 write()
    - connect() 就绪后由 client 自动调用 update_functions() 和 set project_loaded=true
    - run_eva 工具成功后必须调用 set_eva_completed()
    - run_wp 工具成功后必须调用 set_wp_completed()
    - reload_project 工具调用时必须先 invalidate_all() 再重新 fetchFunctions
  - 被调用方的责任：
    - resolve_function(name) 在 O(1) 时间返回
    - invalidate_all() 清空所有状态
    - 状态一致性由不变式保证（见 §2.4）
```

### 3.5 接口：`frama_c::client` → `codec` + `transport`

**功能**：client 通过 codec 编解码命令/响应，通过 transport 进行网络 I/O。

```
接口：frama_c::client → frama_c::codec + frama_c::transport

数据流向：
  发送：FramaCCommand → codec::encode_command → JSON String
                      → codec::encode_frame → Bytes
                      → transport::send_frame → Unix Socket

  接收：Unix Socket → transport::recv_frame → Payload String
                    → codec::decode_response → FramaCResponse

协议约定：
  - codec 的责任：
    - encode_command 输出合法 JSON（字段名正确：cmd/id/request/data）
    - encode_frame 输出正确的 S/L/W 分帧
    - decode_response 正确解析所有 7 种响应类型
    - decode_frame 正确处理不完整帧（返回 None）
  - transport 的责任：
    - send_frame 确保帧完整写入（处理短写入）
    - recv_frame 维护读缓冲区，正确处理粘包/拆包
    - 连接断开时返回 I/O 错误
```

---

## 4. 关键设计决策

| # | 决策 | 选择 | 理由 |
|---|------|------|------|
| 1 | 并发模型 | Phase 1 单 Mutex 串行 | MCP tool call 天然串行（一个 Agent 同时只发一个请求）；Frama-C Server 本身也是单线程处理（GET 可并发但 SET/EXEC 排队）；避免请求-响应关联的解复用复杂度 |
| 2 | Marker 缓存 | SessionState 中 HashMap<name, FunctionInfo> | Frama-C API 大量使用 marker（函数 marker 如 `kf#24`，声明 marker 如 `#F24`），用户（Agent）只知道函数名；`connect()` 时自动通过 `fetchFunctions` 填充缓存，`reload_project` 时刷新，后续工具用 `resolve_function` 转换 |
| 3 | 分页策略 | `fetch_all()` 内部循环，batch=20000 | 与 Ivette 客户端一致（`states.ts:429`）；pending==0 终止；batch 大小由 Frama-C Server 框架决定每批返回多少条目，20000 是 Ivette 实测值 |
| 4 | 进程管理 | Phase 1 外部启动 Frama-C | 用户自行启动 `frama-c <files> -server-socket <path>`，MCP Server 连接现有进程；减少 Phase 1 范围；用户决定 C 文件列表和启动选项 |
| 5 | 错误传播 | FramaCError → McpError 在工具层转换 | frama_c 模块内部用 `Result<T, FramaCError>` 保持类型信息；工具方法在返回前统一转为 McpError；清晰的分层边界 |
| 6 | 状态失效 | reload_project 清全部；run_eva/wp 仅设标志 | 保守策略：重载项目时所有缓存失效（marker 可能变化）；EVA/WP 不改变 AST，只设 completed 标志。Phase 2 可细化 |
| 7 | EXEC POLL 间隔 | 100ms | Ivette 默认 50ms（GUI 需更快响应），MCP 场景对延迟不敏感，100ms 减少 CPU 空转。单次 POLL 读取超时 200ms |
| 8 | 分帧 hex 大小写 | 小写 hex | OCaml 端 `Printf.sprintf "%03x"` 输出小写 hex；虽然 Ivette TypeScript 端发送大写 hex，但 OCaml 解码用 `int_of_string("0x"^hex)` 对大小写不敏感。统一用小写与 Server 输出一致 |

---

## 5. Phase 1 工具实现映射

**与调研报告 §5 的 Phase 1 范围差异**：

| 调研报告 Phase 1 | 本架构 Phase 1 | 原因 |
|-----------------|--------------|------|
| `load_project` (Phase 1) | → `reload_project` | 首次加载在 Frama-C 启动时通过命令行完成（见 §1.2）；`reload_project` 用于源文件变更后重新解析 |
| `find_callers` (Phase 1) | → Phase 2 | `plugins.eva.general.getCallers` 需要 EVA 先运行；调研报告也提到"需 EVA 分析结果"；Phase 1 聚焦不依赖 EVA 的工具 |
| `get_verification_status` (未明确) | ← Phase 1 | 仅依赖 `kernel.properties.fetchStatus` + SessionState 标志，无外部依赖 |
| `run_wp` (Phase 2) | ← Phase 1 | WP API 已完整验证（`setProvers`+`setTimeout`+`startProofs`），编排清晰；EVA + WP 是两大核心分析能力，Phase 1 应同时覆盖 |

### 5.1 `reload_project`

**描述**：源文件变更后重新解析并刷新项目状态。典型场景：Agent 通过文件编辑工具修改了 C 源码或 ACSL 注解后，需要 Frama-C 重新加载。

**参数**：`files: Option<Vec<String>>`（可选，不传则重载当前已加载的文件）

**Frama-C 请求编排**：

```
1. (如果指定了 files)
   SET  kernel.ast.setFiles     data: ["/path/to/file.c", ...]
2. EXEC kernel.ast.compute      data: null
   → POLL 循环等待完成（重新解析 AST）
3. fetch_all("kernel.ast.fetchFunctions")
   → 收集所有函数信息
4. GET  kernel.ast.getFiles     data: null
   → 获取已加载的文件列表
```

**状态更新**：
- `invalidate_all()`（清空全部缓存：marker 可能变化、EVA/WP 结果失效）
- `update_functions(fetch_result)`
- `project_loaded = true`

**返回**：函数列表（名称、文件、行号）+ 已加载文件列表

### 5.2 `get_function_info`

**描述**：获取单个函数的详细信息。

**参数**：`function_name: String`

**Frama-C 请求编排**：

```
1. SessionState.resolve_function(function_name)
   → 获取 FunctionInfo（marker 如 "kf#24"，declaration 如 "#F24"）
   → 若缓存命中则跳过步骤 2
2. (缓存未命中) fetch_all("kernel.ast.fetchFunctions") 更新缓存
3. GET  kernel.ast.printDeclaration  data: "<decl_marker>"
   → 注意：data 是纯字符串（如 "#F24"），非对象（已验证）
   → 获取函数声明文本（含 ACSL 注解，返回标注 AST 数组）
```

**状态更新**：可能更新 functions 缓存

**返回**：函数名、marker、文件路径、行号、声明文本

### 5.3 `get_callgraph`

**描述**：获取项目调用图。

**Frama-C 请求编排**：

```
1. EXEC plugins.callgraph.compute    data: null
   → POLL 循环等待完成
2. GET  plugins.callgraph.getCallgraph  data: null
   → 获取调用图数据
```

**状态更新**：无

**返回**：调用图的节点和边

### 5.4 `run_eva`

**描述**：运行 EVA 抽象解释分析。

**参数**：无（Phase 1 使用 Frama-C 默认值）

**Frama-C 请求编排**：

```
1. EXEC plugins.eva.general.compute   data: null
   → POLL 循环等待完成（EVA 可能运行数分钟，timeout=600s）
2. GET  plugins.eva.general.getComputationState  data: null
   → 获取计算状态（验证 EVA 是否成功完成）
3. GET  plugins.eva.general.getProgramStats  data: null
   → 获取分析统计（覆盖率等）
```

**状态更新**：`set_eva_completed()`

**返回**：计算状态 + 程序统计

**注意**：v2.2 设计中的 `eva.setParams` 和 `eva.getSummary` 不存在。EVA 参数（precision/slevel/main function）通过 `kernel.parameters.*` 全局参数系统设置，具体 request 名需进一步调研。Phase 1 不暴露这些参数，使用 Frama-C 默认值。Phase 2 可按需添加 `main_function`、`precision` 等参数。

### 5.5 `get_eva_alarms`

**描述**：获取 EVA 分析产生的 alarm 列表。

**参数**：`function: Option<String>`，`alarm_kind: Option<String>`，`status: Option<String>`

**Frama-C 请求编排**：

```
1. fetch_all("kernel.properties.fetchStatus")
   → 获取所有属性状态（包含 alarm）
2. (可选，如果 EVA 已运行) fetch_all("plugins.eva.general.fetchProperties")
   → 获取 EVA 特有的属性信息（优先级、污点标记）
   → 与 step 1 的结果按 property ID 合并
3. Rust 端过滤：
   - 按 function 过滤（需用 marker 匹配或从 property 信息中提取函数名）
   - 按 alarm_kind 过滤（propKindTags 中 alarm 相关类型）
   - 按 status 过滤（propStatusTags 中的验证状态）
```

**状态更新**：无

**返回**：过滤后的 alarm 列表（每条包含 marker，可用于 `get_eva_value` 查询值域）

**注意**：v2.2 的 `eva.getAlarms` 不存在。使用 `kernel.properties.fetchStatus` 获取全部属性，在 Rust 端按类型/状态过滤出 alarm。`plugins.eva.general.fetchProperties` 提供补充的 EVA 优先级和污点信息。

### 5.6 `get_eva_value`

**描述**：查询某个程序点的变量值域。

**参数**：`marker: String`（AST marker，如 `#S42`，由 Agent 从其他工具结果中获取）

**Frama-C 请求编排**：

```
1. GET  plugins.eva.values.getValues
   data: { "target": "<marker>", "callstack": null }
   → 获取指定位置的值域信息
```

**状态更新**：无

**返回**：值域信息

**注意**：调研报告确认 `getValues` 需要 `{target, callstack}` 参数。`target` 是 marker（如 `#S42`），非函数名。Frama-C 根据 marker 自动确定函数和表达式，无需 `function`、`expression` 参数。

**Marker 获取链**：Agent 通过以下路径获取 statement-level marker：
1. `get_eva_alarms` → 返回的每条 alarm 包含 property marker
2. `kernel.properties.fetchStatus` 返回的属性数据中包含 source location 和关联 marker
3. Phase 2 的 `lookup_symbol`（`kernel.ast.getMarkerAt`）可按文件/行/列获取 marker

### 5.7 `run_wp`

**描述**：运行 WP 演绎验证（指定函数）。

**参数**：`function_name: String`（必需），`prover: Option<String>`，`timeout: Option<u32>`

**Frama-C 请求编排**：

```
1. (如果指定了 prover)
   SET  plugins.wp.setProvers    data: ["<prover>"]
   → SET 是排队执行的，需通过 poll_loop 等待 DATA 响应
2. (如果指定了 timeout)
   SET  plugins.wp.setTimeout    data: <seconds>
3. GET  kernel.ast.printDeclaration  data: "<decl_marker>"
   → 必须先调用，在服务器 marker 表中注册 PVDecl 等标记
4. EXEC plugins.wp.startProofs   data: "#v<vid>"
   → startProofs 接受 AST.Marker (PVDecl 类型)，非 AST.Decl (#F)
   → 将 #F<vid> 转换为 #v<vid>（共用 Cil varinfo.vid）
   → POLL 循环等待完成
5. GET  plugins.wp.getScheduledTasks  data: null
   → 获取证明任务状态 {todo, procs, done, active}
```

**状态更新**：`set_wp_completed()`

**返回**：证明任务统计

**关键协议发现**（集成测试验证）：
- SET 命令被服务器排队（同 EXEC），不像 GET 立即执行，需 POLL 触发处理
- `startProofs` 只接受 `AST.Marker`（`#v`, `#s`, `#k` 等），不接受 `AST.Decl`（`#F`）
- PVDecl 标记在 `printDeclaration` 调用前未注册，会被拒绝为 "invalid marker"
- v2.2 的 `wp.setParams` 和 `wp.compute` 不存在

### 5.8 `get_verification_status`

**描述**：获取综合验证状态。

**Frama-C 请求编排**：

```
1. fetch_all("kernel.properties.fetchStatus")
   → 获取所有属性及验证状态
2. (如果 eva_completed)
   GET  plugins.eva.general.getComputationState  data: null
3. (如果 wp_completed)
   GET  plugins.wp.getScheduledTasks  data: null
4. Rust 端汇总：
   - 按状态分类属性（valid/unknown/alarm 等）
   - 合并 EVA/WP 状态
   - 读取 SessionState 标志
```

**状态更新**：无

**返回**：属性统计（按类型和状态分类） + EVA/WP 运行状态

---

## 6. 依赖列表

```toml
[package]
name = "frama-c-mcp-server"
version = "0.1.0"
edition = "2021"

[dependencies]
# MCP 协议（官方 Rust SDK）
rmcp = { version = "0.16", features = ["server", "transport-io", "macros"] }

# 异步运行时
tokio = { version = "1", features = ["full"] }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Schema 生成（rmcp 要求 schemars 1.0+）
schemars = "1.0"

# 字节缓冲区（分帧编解码用）
bytes = "1"

# 错误类型
thiserror = "2"

# 日志
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# CLI 参数
clap = { version = "4", features = ["derive"] }

# main 错误处理
anyhow = "1"
```

**与 v2.2 设计文档的差异**：
- 移除 `zeromq`（ZMQ 不可用，使用 tokio Unix Socket）
- 移除 `uuid`（请求 ID 改用自增计数器 `RQ.<n>`，与 Ivette 一致）
- edition 改为 `2021`（2024 仅 nightly 可用，Phase 1 不需要）
- 新增 `bytes`（分帧编解码的高效缓冲区管理）

---

## 附录 A：v2.2 设计纠正对照表

| 维度 | v2.2 假设 | 调研确认的实际值 | 本架构采用 |
|------|----------|----------------|-----------|
| 命令字段名 | `"kind"` | `"cmd"` | `"cmd"` |
| 响应字段名 | `"kind"` | `"res"` | `"res"` |
| ERROR 格式 | `{"kind":"ERROR","data":{...}}` | `{"res":"ERROR","id":"...","msg":"..."}` | msg 字段 |
| POLL 命令 | 未提及 | `"POLL"` (JSON string) | `"POLL"` |
| SHUTDOWN | 未提及 | `"SHUTDOWN"` (JSON string) | `"SHUTDOWN"` |
| 通信方式 | ZMQ REQ/REP | Unix Socket + 自定义分帧 | Unix Socket |
| `kernel.ast.getFunctionInfo` | 存在 | 不存在 | `fetchFunctions` + `printDeclaration` |
| `metrics.getMetrics` | 存在 | 不存在 | 移除，不提供 metrics |
| `eva.setParams` | 存在 | 不存在 | `kernel.parameters.set*`（Phase 1 使用默认值） |
| `eva.getAlarms` | 存在 | 不存在 | `kernel.properties.fetchStatus` 过滤 |
| `eva.getSummary` | 存在 | 不存在 | `plugins.eva.general.getProgramStats` |
| `wp.setParams` | 存在 | 不存在 | `plugins.wp.setProvers` + `setTimeout` |
| `wp.compute` | 存在 | 不存在 | `plugins.wp.startProofs` |
| `callgraph.getCallers` | 存在 | callgraph 无此 request | `plugins.eva.general.getCallers`（需 EVA） |
| `kernel.ast.getDecl` | 存在 | 不存在 | `fetchFunctions` + `fetchGlobals` |

## 附录 A.2：rmcp 0.16 API 实现适配

实现阶段确认的 rmcp 0.16 宏和 API 模式，与设计文档初始假设的差异：

| 维度 | 设计初始假设 | rmcp 0.16 实际 API | 适配 |
|------|------------|-------------------|------|
| 工具注册宏 | `#[tool_handler]` 同时注册工具和 ServerHandler | 分两个宏：`#[tool_router]`（工具 impl）+ `#[tool_handler]`（ServerHandler impl） | 使用双宏模式 |
| 路由表 | 隐式，无需 struct 字段 | 显式 `tool_router: ToolRouter<Self>` 字段，构造时 `Self::tool_router()` 初始化 | struct 增加字段 |
| tool_handler 语法 | `#[tool_handler]` | `#[tool_handler(router = self.tool_router)]` | 指定 router 字段 |
| serve 方法 | `server.serve(stdio())` 直接可用 | 需 `use rmcp::ServiceExt` 引入 trait | main 增加 import |
| stdio 传输路径 | `rmcp::transport::stdio()` | `rmcp::transport::io::stdio()` | 修正路径 |
| McpError 别名 | `rmcp::Error` | `rmcp::ErrorData`（`Error` 已 deprecated） | 使用 `ErrorData` |

## 附录 B：Phase 2+ 预留工具

以下工具在 Phase 1 不实现，但架构预留了扩展点：

| 工具 | Phase | 依赖 |
|------|-------|------|
| `get_wp_goals` | 2 | `plugins.wp.fetchGoals` 分页 |
| `get_current_annotations` | 2 | `kernel.properties.fetchStatus` 过滤 |
| `lookup_symbol` | 2 | `fetchFunctions` / `fetchGlobals` / `getMarkerAt` |
| `find_callers` | 2 | `plugins.eva.general.getCallers`（需 EVA 先运行） |
| `trace_call_chain` | 2 | Rust BFS + callgraph 数据 |
| `investigate_alarm` | 2 | Rust 组合多个 GET 查询 |
| `suggest_verification_plan` | 2 | Rust 策略逻辑 |
| `inject_acsl` | 3 | 需 OCaml 插件 |
| `remove_acsl` | 3 | 需 OCaml 插件 |
| `find_memory_ops` | 3 | 需 OCaml CIL Visitor |
| `get_cfg` | 3 | 需 OCaml 插件 |
| `get_data_deps` | 3 | From 插件未注册 server request |
