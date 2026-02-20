# Rust MCP Server 细化设计文档

> **阶段**：细化设计
> **日期**：2026-02-19
> **前置**：[架构文档](./architecture.md)
> **约束**：不改变架构阶段确定的模块划分、接口规约和核心流程

---

## 目录

1. [模块 `error`](#1-模块-error)
2. [模块 `frama_c::codec`](#2-模块-frama_ccodec)
3. [模块 `frama_c::transport`](#3-模块-frama_ctransport)
4. [模块 `frama_c::client`](#4-模块-frama_cclient)
5. [模块 `state`](#5-模块-state)
6. [模块 `mcp::types`](#6-模块-mcptypes)
7. [模块 `mcp::server`](#7-模块-mcpserver)
8. [模块 `main`](#8-模块-main)
9. [错误处理策略](#9-错误处理策略)

---

## 1. 模块 `error`

**文件**：`src/error.rs`

### 1.1 `FramaCError`

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

    #[error("Function not found: {0}")]
    FunctionNotFound(String),
}
```

**新增 `FunctionNotFound`**：`resolve_function` 在缓存中找不到函数名时返回，工具层转为 McpError 的 invalid_params。

### 1.2 `impl From<FramaCError> for McpError`

rmcp 中 `McpError` 是 `rmcp::ErrorData` 的惯用别名（`use rmcp::ErrorData as McpError`）。

```rust
use rmcp::ErrorData as McpError;

impl From<FramaCError> for McpError {
    fn from(e: FramaCError) -> Self
}
```

**功能**：将内部错误转换为 MCP 协议错误。

**映射规则**：

所有 `ErrorData` 构造方法签名为 `fn xxx(message: impl Into<Cow<'static, str>>, data: Option<Value>) -> Self`。

| FramaCError 变体 | 构造 | code |
|-----------------|------|------|
| `ServerError { id, msg }` | `McpError::internal_error(msg, None)` | -32603 |
| `Rejected { id }` | `McpError::invalid_request(format!("rejected: {id}"), None)` | -32600 |
| `Killed { id }` | `McpError::internal_error(format!("killed: {id}"), None)` | -32603 |
| `ConnectTimeout` | `McpError::internal_error("connection timeout", None)` | -32603 |
| `Timeout(d)` | `McpError::internal_error(format!("timeout after {d:?}"), None)` | -32603 |
| `FunctionNotFound(name)` | `McpError::invalid_params(format!("function not found: {name}"), None)` | -32602 |
| `Io` / `Json` / `InvalidFrame` / `UnexpectedResponse` | `McpError::internal_error(e.to_string(), None)` | -32603 |

**调用关系**：被 `mcp::server` 每个工具方法中的 `?` 操作符自动调用（依赖 `From` trait）。

---

## 2. 模块 `frama_c::codec`

**文件**：`src/frama_c/codec.rs`

### 2.1 类型定义

```rust
/// 客户端发送的命令
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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

### 2.2 `encode_frame`

```rust
pub fn encode_frame(payload: &str) -> Vec<u8>
```

**功能**：将 JSON 字符串编码为 Frama-C Server 协议帧。

**算法**：
1. `len = payload.len()`（`&str::len()` 返回字节长度，与 `as_bytes().len()` 语义相同）
2. 选择前缀：
   - `len ≤ 0xFFF` → `format!("S{:03x}", len)`
   - `len ≤ 0xFFFFFFF` → `format!("L{:07x}", len)`
   - 否则 → `format!("W{:015x}", len)`
3. 拼接 header + payload bytes，返回 `Vec<u8>`

**调用关系**：被 `transport::send_frame` 调用。

### 2.3 `decode_frame`

```rust
pub fn decode_frame(buf: &[u8]) -> Result<Option<(String, usize)>, FramaCError>
```

**功能**：尝试从字节缓冲区中解码一个完整帧。

**参数**：
- `buf`：读缓冲区当前内容

**返回**：
- `Ok(Some((payload, consumed)))` — 成功解码一帧，`payload` 为 UTF-8 字符串，`consumed` 为消耗的字节数
- `Ok(None)` — 数据不完整，需要更多数据
- `Err(InvalidFrame)` — 帧格式错误（首字节不是 S/L/W，或 hex 解析失败，或 UTF-8 解码失败）

**算法**：
1. `buf.len() < 1` → `Ok(None)`
2. 读 `buf[0]`：`b'S'` → `hex_len=3`，`b'L'` → `hex_len=7`，`b'W'` → `hex_len=15`，其他 → `Err(InvalidFrame)`
3. `header_len = 1 + hex_len`
4. `buf.len() < header_len` → `Ok(None)`
5. `hex_str = std::str::from_utf8(&buf[1..header_len])?`
6. `payload_len = usize::from_str_radix(hex_str, 16)?`（失败 → `Err(InvalidFrame)`）
7. `total = header_len + payload_len`
8. `buf.len() < total` → `Ok(None)`
9. `payload = std::str::from_utf8(&buf[header_len..total])?`（失败 → `Err(InvalidFrame)`）
10. `Ok(Some((payload.to_string(), total)))`

**调用关系**：被 `transport::recv_frame` 调用。

### 2.4 `encode_command`

```rust
pub fn encode_command(cmd: &FramaCCommand) -> String
```

**功能**：将 `FramaCCommand` 序列化为 JSON 字符串。

**算法**（按变体）：
- `Get/Set/Exec { id, request, data }` → `serde_json::json!({"cmd": kind, "id": id, "request": request, "data": data}).to_string()`
  - `kind` = `"GET"` / `"SET"` / `"EXEC"`
- `Poll` → `"\"POLL\""`（即 JSON 字符串 `"POLL"`）
- `Shutdown` → `"\"SHUTDOWN\""`
- `Kill { id }` → `serde_json::json!({"cmd": "KILL", "id": id}).to_string()`
- `SigOn { id }` → `serde_json::json!({"cmd": "SIGON", "id": id}).to_string()`
- `SigOff { id }` → `serde_json::json!({"cmd": "SIGOFF", "id": id}).to_string()`

**注意**：POLL 和 SHUTDOWN 是 JSON 字符串字面量，不是 JSON 对象。序列化结果必须是 `"POLL"` 而不是 `{"cmd":"POLL"}`。

**调用关系**：被 `client` 的 `send_command` 内部方法调用。

### 2.5 `decode_response`

```rust
pub fn decode_response(json_str: &str) -> Result<FramaCResponse, FramaCError>
```

**功能**：将 JSON 字符串反序列化为 `FramaCResponse`。

**算法**：
1. `let value: serde_json::Value = serde_json::from_str(json_str)?`
2. 如果 `value` 是字符串：
   - `"CMDLINEON"` → `Ok(CmdLineOn)`
   - `"CMDLINEOFF"` → `Ok(CmdLineOff)`
   - 其他 → `Err(UnexpectedResponse(...))`
3. 如果 `value` 是对象：
   - 读 `value["res"]` 字符串
   - `"DATA"` → `Ok(Data { id: value["id"], data: value["data"] })`
   - `"ERROR"` → `Ok(Error { id: value["id"], msg: value["msg"] })`
   - `"SIGNAL"` → `Ok(Signal { id: value["id"] })`
   - `"REJECTED"` → `Ok(Rejected { id: value["id"] })`
   - `"KILLED"` → `Ok(Killed { id: value["id"] })`
   - 其他 → `Err(UnexpectedResponse(...))`
4. 否则 → `Err(UnexpectedResponse(...))`

**注意**：`id` 和 `msg` 字段通过 `as_str().unwrap_or_default().to_string()` 提取；`data` 字段用 `value["data"].clone()`（保留原始 JSON 值，可能为 null/array/object）。

**调用关系**：被 `client` 的 `recv_response` 内部方法调用。

---

## 3. 模块 `frama_c::transport`

**文件**：`src/frama_c/transport.rs`

### 3.1 `Transport`

```rust
pub struct Transport {
    stream: tokio::net::UnixStream,
    read_buf: bytes::BytesMut,
}
```

### 3.2 `Transport::connect`

```rust
pub async fn connect(path: &str) -> Result<Self, FramaCError>
```

**功能**：建立 Unix Socket 连接。

**算法**：
1. `let stream = tokio::net::UnixStream::connect(path).await?`
2. `Ok(Transport { stream, read_buf: BytesMut::with_capacity(8192) })`

**调用关系**：被 `FramaCClient::connect` 调用。

### 3.3 `Transport::send_frame`

```rust
pub async fn send_frame(&mut self, payload: &str) -> Result<(), FramaCError>
```

**功能**：编码帧并写入 stream。

**算法**：
1. `let frame = codec::encode_frame(payload)`
2. `self.stream.write_all(&frame).await?`

**调用关系**：被 `FramaCClient` 的 `send_command` 内部方法调用。

### 3.4 `Transport::recv_frame`

```rust
pub async fn recv_frame(&mut self, timeout: Duration) -> Result<Option<String>, FramaCError>
```

**功能**：从 stream 读取数据，尝试解码一帧。

**参数**：
- `timeout`：最大等待时间

**返回**：
- `Ok(Some(payload))` — 成功读到一帧
- `Ok(None)` — 超时
- `Err(...)` — 连接断开或帧格式错误

**算法**：
```
loop {
    // 先尝试从现有 buf 解码
    if let Some((payload, consumed)) = codec::decode_frame(&self.read_buf)? {
        self.read_buf.advance(consumed);
        return Ok(Some(payload));
    }
    // buf 不够，从 stream 读更多数据
    let mut tmp = [0u8; 4096];
    match tokio::time::timeout(timeout, self.stream.read(&mut tmp)).await {
        Ok(Ok(0)) => return Err(FramaCError::Io(
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "connection closed")
        )),
        Ok(Ok(n)) => self.read_buf.extend_from_slice(&tmp[..n]),
        Ok(Err(e)) => return Err(FramaCError::Io(e)),
        Err(_) => return Ok(None),  // timeout
    }
}
```

**调用关系**：被 `FramaCClient` 的 `recv_response` 内部方法调用。

### 3.5 `Transport::close`

```rust
pub async fn close(&mut self) -> Result<(), FramaCError>
```

**功能**：关闭 stream 的写端。

**算法**：
1. `self.stream.shutdown().await?`（`AsyncWriteExt::shutdown` 取 `&mut self`）
2. `Ok(())`

**调用关系**：被 `FramaCClient::shutdown` 调用。

---

## 4. 模块 `frama_c::client`

**文件**：`src/frama_c/client.rs`

### 4.1 类型定义

```rust
pub struct FramaCClient {
    inner: tokio::sync::Mutex<ClientInner>,
}

struct ClientInner {
    transport: Transport,
    counter: u64,
}
```

### 4.2 `ClientInner::next_id`

```rust
fn next_id(&mut self) -> String
```

**功能**：生成下一个请求 ID。

**算法**：
1. `let id = format!("RQ.{}", self.counter)`
2. `self.counter += 1`
3. `id`

**调用关系**：被 `send_command` 和 `send_get`/`send_set`/`send_exec` 调用。

### 4.3 `ClientInner::send_command`

```rust
async fn send_command(&mut self, cmd: &FramaCCommand) -> Result<(), FramaCError>
```

**功能**：编码命令并发送。

**算法**：
1. `let json = codec::encode_command(cmd)`
2. `self.transport.send_frame(&json).await`

**调用关系**：被 `send_get`、`send_set`、`send_exec`、`send_poll`、`send_kill`、`send_shutdown` 调用。

### 4.4 `ClientInner::recv_response`

```rust
async fn recv_response(&mut self, timeout: Duration) -> Result<Option<FramaCResponse>, FramaCError>
```

**功能**：接收并解码一条响应。

**算法**：
1. `let payload = self.transport.recv_frame(timeout).await?`
2. `match payload { Some(s) => Ok(Some(codec::decode_response(&s)?)), None => Ok(None) }`

**调用关系**：被 `wait_response`、`poll_loop` 调用。

### 4.5 `ClientInner::wait_response`

```rust
async fn wait_response(&mut self) -> Result<FramaCResponse, FramaCError>
```

**功能**：阻塞等待一条响应（用于 GET/SET，响应是立即的）。

**算法**：
1. 以 5 秒超时调用 `recv_response`
2. `Some(resp)` → `Ok(resp)`
3. `None` → `Err(Timeout(5s))`

**调用关系**：被 `FramaCClient::get`、`FramaCClient::set` 调用。

### 4.6 `ClientInner::poll_loop`

```rust
async fn poll_loop(
    &mut self,
    request_id: &str,
    timeout: Duration,
) -> Result<serde_json::Value, FramaCError>
```

**功能**：EXEC 请求的 POLL 循环，直到收到终止响应。

**参数**：
- `request_id`：EXEC 请求的 ID，用于匹配终止响应
- `timeout`：总超时（从循环开始计时）

**算法**：
```
let deadline = Instant::now() + timeout;
loop {
    if Instant::now() >= deadline {
        // 超时：发送 KILL 并返回错误
        self.send_command(&FramaCCommand::Kill { id: request_id.to_string() }).await?;
        return Err(FramaCError::Timeout(timeout));
    }

    // 等待 POLL 间隔
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 发送 POLL
    self.send_command(&FramaCCommand::Poll).await?;

    // 尝试读取响应（200ms 超时）
    let resp = self.recv_response(Duration::from_millis(200)).await?;
    match resp {
        Some(FramaCResponse::Data { id, data }) if id == request_id => {
            return Ok(data);
        }
        Some(FramaCResponse::Error { id, msg }) if id == request_id => {
            return Err(FramaCError::ServerError { id, msg });
        }
        Some(FramaCResponse::Killed { id }) if id == request_id => {
            return Err(FramaCError::Killed { id });
        }
        Some(FramaCResponse::Signal { .. }) => {
            // 消费 SIGNAL，继续 POLL
            continue;
        }
        Some(other) => {
            // 非预期响应（可能是其他请求的 SIGNAL），记日志后继续
            tracing::warn!("unexpected response during POLL: {:?}", other);
            continue;
        }
        None => {
            // 超时无响应，继续 POLL
            continue;
        }
    }
}
```

**调用关系**：被 `FramaCClient::exec` 调用。

### 4.7 `FramaCClient::connect`

```rust
pub async fn connect(
    path: &str,
    state: Arc<RwLock<SessionState>>,
) -> Result<Self, FramaCError>
```

**功能**：建立连接，等待 CMDLINEOFF，自动填充 marker 缓存。

**算法**：
```
1. let transport = Transport::connect(path).await?;
2. let mut inner = ClientInner { transport, counter: 0 };
3. // 等待 CMDLINEOFF（最多 30 秒）
   let deadline = Instant::now() + Duration::from_secs(30);
   loop {
       let remaining = deadline - Instant::now();
       if remaining.is_zero() { return Err(ConnectTimeout); }
       match inner.recv_response(remaining).await? {
           Some(CmdLineOff) => break,
           Some(CmdLineOn) => continue,  // 命令行执行中
           Some(other) => {
               tracing::warn!("unexpected during handshake: {:?}", other);
               continue;
           }
           None => return Err(ConnectTimeout),
       }
   }
4. let client = FramaCClient { inner: Mutex::new(inner) };
5. // 自动获取函数信息填充缓存
   let entries = client.fetch_all("kernel.ast.fetchFunctions").await?;
   {
       let mut st = state.write().await;
       st.update_functions(&entries);
       st.project_loaded = true;
   }
6. Ok(client)
```

**调用关系**：被 `main` 调用。

### 4.8 `FramaCClient::get`

```rust
pub async fn get(
    &self,
    request: &str,
    data: serde_json::Value,
) -> Result<serde_json::Value, FramaCError>
```

**功能**：发送 GET 请求，等待响应。

**算法**：
```
let mut inner = self.inner.lock().await;
let id = inner.next_id();
inner.send_command(&FramaCCommand::Get {
    id: id.clone(), request: request.to_string(), data,
}).await?;
let resp = inner.wait_response().await?;
match resp {
    FramaCResponse::Data { data, .. } => Ok(data),
    FramaCResponse::Error { id, msg } => Err(FramaCError::ServerError { id, msg }),
    FramaCResponse::Rejected { id } => Err(FramaCError::Rejected { id }),
    other => Err(FramaCError::UnexpectedResponse(format!("{:?}", other))),
}
```

**调用关系**：被 `mcp::server` 的各工具方法调用。

### 4.9 `FramaCClient::set`

```rust
pub async fn set(
    &self,
    request: &str,
    data: serde_json::Value,
) -> Result<serde_json::Value, FramaCError>
```

**功能**：发送 SET 请求，等待响应。

**算法**：与 `get` 相同，仅命令类型为 `Set`。

**调用关系**：被 `mcp::server` 的 `reload_project`、`run_wp` 调用。

### 4.10 `FramaCClient::exec`

```rust
pub async fn exec(
    &self,
    request: &str,
    data: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, FramaCError>
```

**功能**：发送 EXEC 请求，进入 POLL 循环等待结果。

**算法**：
```
let mut inner = self.inner.lock().await;
let id = inner.next_id();
inner.send_command(&FramaCCommand::Exec {
    id: id.clone(), request: request.to_string(), data,
}).await?;
inner.poll_loop(&id, timeout).await
```

**调用关系**：被 `mcp::server` 的 `reload_project`、`get_callgraph`、`run_eva`、`run_wp` 调用。

### 4.11 `FramaCClient::fetch_all`

```rust
pub async fn fetch_all(
    &self,
    request: &str,
) -> Result<Vec<serde_json::Value>, FramaCError>
```

**功能**：分页循环获取全部数据。

**算法**：
```
let mut all_entries = Vec::new();
loop {
    let data = self.get(request, serde_json::json!(20000)).await?;
    // 先检查 reload 标志（必须在 extend 之前）
    if data.get("reload").and_then(|v| v.as_bool()).unwrap_or(false) {
        // reload==true 表示服务端数据已重置，之前累积的 entries 无效
        // 清空后用本批次 updated 重建
        all_entries.clear();
    }
    // 提取 updated 数组
    if let Some(updated) = data.get("updated").and_then(|v| v.as_array()) {
        all_entries.extend(updated.iter().cloned());
    }
    // 检查 pending
    let pending = data.get("pending")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if pending == 0 {
        break;
    }
}
Ok(all_entries)
```

**注意**：`fetch_all` 内部调用 `self.get`，而 `get` 会获取 Mutex。由于 `fetch_all` 是 `&self` 方法，每次 `get` 调用独立获取和释放锁。这避免了在循环中长期持有锁。

**调用关系**：被 `connect`（初始化缓存）、`reload_project`、`get_function_info`（缓存未命中时）、`get_eva_alarms`、`get_verification_status` 调用。

### 4.12 `FramaCClient::shutdown`

```rust
pub async fn shutdown(&self) -> Result<(), FramaCError>
```

**功能**：发送 SHUTDOWN 并关闭连接写端。取 `&self` 而非 `self`，因为 client 被包装在 `Arc` 中（见 §7.1），无法获取 ownership。

**算法**：
```
let mut inner = self.inner.lock().await;
inner.send_command(&FramaCCommand::Shutdown).await?;
inner.transport.close().await
```

**调用关系**：被 `main` 在退出时调用（可选）。Transport 在 FramaCClient drop 时自动关闭 socket。

---

## 5. 模块 `state`

**文件**：`src/state.rs`

### 5.1 类型定义

```rust
#[derive(Debug, Default)]
pub struct SessionState {
    pub project_loaded: bool,
    pub eva_completed: bool,
    pub wp_completed: bool,
    pub functions: HashMap<String, FunctionInfo>,
}

#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub name: String,
    pub marker: String,
    pub declaration: String,
    pub signature: String,
    pub file: String,
    pub line: u32,
}
```

### 5.2 `SessionState::update_functions`

```rust
pub fn update_functions(&mut self, entries: &[serde_json::Value])
```

**功能**：用 `fetchFunctions` 的返回数据填充 functions 缓存。

**实际 JSON 格式**（已通过集成测试验证）：
```json
{
  "name": "abs_val",
  "key": "kf#24",           // 函数 marker（如 "kf#24"，非设计初期假设的 "#F990"）
  "decl": "#F24",            // 声明 marker（用于 printDeclaration）
  "signature": "int abs_val(int x);",
  "defined": true,
  "sloc": {                  // 源码位置是嵌套对象（非顶层 file/line）
    "file": "/path/to/file.c",
    "line": 6,
    "base": "file.c",
    "dir": "test"
  }
}
```

**算法**：
```
self.functions.clear();
for entry in entries {
    let name = entry["name"].as_str().unwrap_or_default().to_string();
    let marker = entry["key"].as_str().unwrap_or_default().to_string();
    let declaration = entry["decl"].as_str().unwrap_or_default().to_string();
    let signature = entry["signature"].as_str().unwrap_or_default().to_string();
    let file = entry["sloc"]["file"].as_str().unwrap_or_default().to_string();
    let line = entry["sloc"]["line"].as_u64().unwrap_or(0) as u32;
    if !name.is_empty() {
        self.functions.insert(name.clone(), FunctionInfo {
            name, marker, declaration, signature, file, line,
        });
    }
}
```

**调用关系**：被 `FramaCClient::connect` 和 `reload_project` 工具调用。

### 5.3 `SessionState::resolve_function`

```rust
pub fn resolve_function(&self, name: &str) -> Option<&FunctionInfo>
```

**功能**：按函数名查找缓存。

**算法**：`self.functions.get(name)`

**调用关系**：被 `resolve_function_or_refresh` 调用（间接被 `get_function_info`、`run_wp`、`get_eva_alarms` 使用）。

### 5.4 `SessionState::invalidate_all`

```rust
pub fn invalidate_all(&mut self)
```

**功能**：清空全部状态。

**算法**：
```
self.project_loaded = false;
self.eva_completed = false;
self.wp_completed = false;
self.functions.clear();
```

**调用关系**：被 `reload_project` 工具调用。

### 5.5 辅助 setter

```rust
pub fn set_eva_completed(&mut self) { self.eva_completed = true; }
pub fn set_wp_completed(&mut self) { self.wp_completed = true; }
```

**调用关系**：被 `run_eva`、`run_wp` 工具调用。

---

## 6. 模块 `mcp::types`

**文件**：`src/mcp/types.rs`

所有类型 derive `Debug, Deserialize, JsonSchema`。

### 6.1 `ReloadProjectParams`

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReloadProjectParams {
    /// C source file paths to reload. If omitted, reloads currently loaded files.
    pub files: Option<Vec<String>>,
}
```

### 6.2 `GetFunctionInfoParams`

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFunctionInfoParams {
    /// Function name to query
    pub function_name: String,
}
```

### 6.3 `run_eva` 参数

Phase 1 的 `run_eva` 无参数，使用 Frama-C 默认值。EVA 参数（precision/slevel/main function）通过 `kernel.parameters.*` 系统设置，具体 request 名需进一步调研，Phase 2 可按需添加。

工具方法签名为 `async fn run_eva(&self) -> Result<CallToolResult, McpError>`（无 Parameters 包装）。

### 6.4 `GetEvaAlarmsParams`

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetEvaAlarmsParams {
    /// Filter by function name
    pub function: Option<String>,
    /// Filter by alarm kind (e.g. "mem_access", "division_by_zero")
    pub alarm_kind: Option<String>,
    /// Filter by verification status
    pub status: Option<String>,
}
```

### 6.5 `GetEvaValueParams`

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetEvaValueParams {
    /// Statement or expression marker (e.g. "#s2")
    pub marker: String,
}
```

**注意**：移除 `function` 和 `expression` 参数。`getValues` 的 `target` 是 marker，Frama-C 根据 marker 自动确定函数和表达式。简化 Agent 使用。

### 6.6 `RunWpParams`

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunWpParams {
    /// SMT prover name: "alt-ergo", "z3", "cvc5" (default: current setting)
    pub prover: Option<String>,
    /// Prover timeout in seconds (default: current setting)
    pub timeout: Option<u32>,
}
```

**注意**：移除 `functions` 参数。Phase 1 的 `startProofs` 验证全部已注解函数。按函数过滤可在 Phase 2 添加。

---

## 7. 模块 `mcp::server`

**文件**：`src/mcp/server.rs`

### 7.1 类型定义

```rust
#[derive(Clone)]
pub struct FramaCMcpServer {
    client: Arc<FramaCClient>,
    state: Arc<RwLock<SessionState>>,
    tool_router: ToolRouter<Self>,  // rmcp 要求在 struct 中持有路由表
}
```

**注意**：rmcp 0.16 的 `#[tool_router]` 宏在 impl 块上生成 `Self::tool_router()` 关联函数，返回 `ToolRouter<Self>`。struct 必须持有此字段，`#[tool_handler]` 通过 `router = self.tool_router` 引用它。

### 7.2 构造

```rust
impl FramaCMcpServer {
    pub fn new(client: FramaCClient, state: Arc<RwLock<SessionState>>) -> Self {
        Self {
            client: Arc::new(client),
            state,
            tool_router: Self::tool_router(),  // 由 #[tool_router] 宏生成
        }
    }
}
```

**调用关系**：被 `main` 调用。

### 7.3 公共方法：`resolve_function_or_refresh`

```rust
async fn resolve_function_or_refresh(
    &self,
    name: &str,
) -> Result<FunctionInfo, McpError>
```

**功能**：按函数名解析 `FunctionInfo`，缓存未命中时自动刷新缓存再重试。

**算法**：
```
1. let info = {
       let state = self.state.read().await;
       state.resolve_function(name).cloned()
   };
2. if let Some(f) = info { return Ok(f); }
3. // 缓存未命中 — reload + fetch 刷新缓存
   self.client.get("kernel.ast.reloadFunctions", json!(null)).await?;
   let entries = self.client.fetch_all("kernel.ast.fetchFunctions").await?;
   {
       let mut state = self.state.write().await;
       state.update_functions(&entries);
   }
4. let state = self.state.read().await;
   state.resolve_function(name).cloned()
       .ok_or_else(|| McpError::from(FramaCError::FunctionNotFound(name.to_string())))
```

**调用关系**：被 `get_function_info`、`run_wp`、`get_eva_alarms`（按函数过滤时）调用。

**设计动机**：三个调用点在缓存未命中时的行为原本不一致（刷新重试 / 直接报错 / 静默跳过）。抽取公共方法统一为「刷新 → 重试 → 仍未命中才报错」，消除不一致。`reloadFunctions` 调用是必须的，因为 `fetchFunctions` 有增量消费语义（首次消费后再调返回空），需要先 reload 重置游标。

### 7.4 ServerHandler 实现

```rust
#[tool_handler(router = self.tool_router)]
impl ServerHandler for FramaCMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Frama-C formal verification server. Provides EVA abstract interpretation, \
                 WP deductive verification, and CIL AST navigation.".into()
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            ..Default::default()
        }
    }
}
```

**rmcp 宏分工**：
- `#[tool_router]` 标注在工具方法的 impl 块上（§7.5-§7.12），生成 `Self::tool_router()` 关联函数
- `#[tool_handler(router = self.tool_router)]` 标注在 `ServerHandler` impl 块上，自动实现 `list_tools`/`call_tool`/`get_tool`，委托给 `self.tool_router`

### 7.5 工具方法：`reload_project`

```rust
#[tool(description = "Reload C source files after modification. \
    Reparses AST and refreshes all cached state. \
    EVA/WP results are invalidated.")]
async fn reload_project(
    &self,
    Parameters(params): Parameters<ReloadProjectParams>,
) -> Result<CallToolResult, McpError>
```

**算法**：
```
1. if let Some(files) = params.files {
       self.client.set("kernel.ast.setFiles", json!(files)).await?;
   }
2. self.client.exec("kernel.ast.compute", json!(null), Duration::from_secs(120)).await?;
3. // reload 重置增量游标（compute 后 fetchFunctions 可能已被消费）
   self.client.get("kernel.ast.reloadFunctions", json!(null)).await?;
   let entries = self.client.fetch_all("kernel.ast.fetchFunctions").await?;
4. let files_list = self.client.get("kernel.ast.getFiles", json!(null)).await?;
5. {
       let mut state = self.state.write().await;
       state.invalidate_all();
       state.update_functions(&entries);
       state.project_loaded = true;
   }
6. 组装返回 JSON（函数列表 + 文件列表）
```

### 7.6 工具方法：`get_function_info`

```rust
#[tool(description = "Get detailed info for a function: \
    source location, declaration text with ACSL annotations.")]
async fn get_function_info(
    &self,
    Parameters(params): Parameters<GetFunctionInfoParams>,
) -> Result<CallToolResult, McpError>
```

**算法**：
```
1. let info = self.resolve_function_or_refresh(&params.function_name).await?;
2. let decl_text = self.client.get(
       "kernel.ast.printDeclaration",
       json!(info.declaration),  // 纯字符串参数，非对象（已验证）
   ).await?;
4. 组装返回 JSON（name, marker, signature, file, line, declaration_text）
```

### 7.7 工具方法：`get_callgraph`

```rust
#[tool(description = "Compute and return the function call graph.")]
async fn get_callgraph(&self) -> Result<CallToolResult, McpError>
```

**算法**：
```
1. self.client.exec(
       "plugins.callgraph.compute", json!(null), Duration::from_secs(60),
   ).await?;
2. let graph = self.client.get(
       "plugins.callgraph.getCallgraph", json!(null),
   ).await?;
3. 返回 graph JSON
```

### 7.8 工具方法：`run_eva`

```rust
#[tool(description = "Run EVA abstract interpretation analysis. \
    Returns computation state and program statistics. \
    This may take several minutes for large programs.")]
async fn run_eva(&self) -> Result<CallToolResult, McpError>
```

**算法**：
```
1. self.client.exec(
       "plugins.eva.general.compute", json!(null), Duration::from_secs(600),
   ).await?;
2. let comp_state = self.client.get(
       "plugins.eva.general.getComputationState", json!(null),
   ).await?;
3. let stats = self.client.get(
       "plugins.eva.general.getProgramStats", json!(null),
   ).await?;
4. {
       let mut state = self.state.write().await;
       state.set_eva_completed();
   }
5. 组装返回 JSON（computation_state + program_stats）
```

### 7.9 工具方法：`get_eva_alarms`

```rust
#[tool(description = "Get EVA analysis alarms (potential runtime errors). \
    Optionally filter by function, alarm kind, or verification status.")]
async fn get_eva_alarms(
    &self,
    Parameters(params): Parameters<GetEvaAlarmsParams>,
) -> Result<CallToolResult, McpError>
```

**算法**：
```
1. // 重置增量游标（fetchStatus 只能消费一次，重复调用返回空）
   self.client.get("kernel.properties.reloadStatus", json!(null)).await?;
   let properties = self.client.fetch_all("kernel.properties.fetchStatus").await?;
2. // 按函数过滤：properties 的 scope 字段是函数声明 marker（如 "#F24"）
   //   使用 resolve_function_or_refresh 统一解析，缓存未命中时自动刷新
   let scope_marker = if let Some(ref func) = params.function {
       Some(self.resolve_function_or_refresh(func).await?.declaration)
   } else {
       None
   };
   let filtered: Vec<_> = properties.iter().filter(|prop| {
       if let Some(ref marker) = scope_marker {
           let prop_scope = prop["scope"].as_str().unwrap_or_default();
           if prop_scope != marker { return false; }
       }
       if let Some(ref kind) = params.alarm_kind {
           let prop_kind = prop["kind"].as_str().unwrap_or_default();
           if prop_kind != kind { return false; }
       }
       if let Some(ref status) = params.status {
           let prop_status = prop["status"].as_str().unwrap_or_default();
           if prop_status != status { return false; }
       }
       true
   }).collect();
3. 返回 filtered JSON
```

**`fetchStatus` 属性字段格式**（已通过集成测试验证）：
```json
{
  "key": "#p10",              // 属性标识符
  "kind": "ensures",          // 类型：ensures, requires, instance, behavior, etc.
  "status": "valid",          // 状态：valid, unknown, invalid, never_tried
  "scope": "#F24",            // 所属函数的声明 marker
  "descr": "ensures\n\\result ≥ 0",
  "predicate": "\\result ≥ 0",
  "source": { "file": "...", "line": 2, "base": "test.c", "dir": "test" },
  "alarm": null,              // EVA 报警信息（非报警时为 null）
  "alarm_descr": null,
  "from_libc": false,
  "names": [],                // 行为名（如 ["default!"]，多数为空）
  "kinstr": null              // 关联的语句 marker（如 "#k13"）
}
```
注意：过滤函数时使用 `scope` 字段（声明 marker），非函数名。

### 7.10 工具方法：`get_eva_value`

```rust
#[tool(description = "Query EVA value range at a program point. \
    The marker can be obtained from get_eva_alarms results.")]
async fn get_eva_value(
    &self,
    Parameters(params): Parameters<GetEvaValueParams>,
) -> Result<CallToolResult, McpError>
```

**算法**：
```
1. let values = self.client.get(
       "plugins.eva.values.getValues",
       json!({"target": params.marker}),
   ).await?;
2. 返回 values JSON
```

**注意**：`callstack` 是 `param_opt`（可选参数），必须完全省略（不传），不能传 `null`。省略时返回所有调用栈的合并值。如需按调用栈查询，传入整数索引（通过 `getCallstacks` 获取）。

### 7.11 工具方法：`run_wp`

```rust
#[tool(description = "Run WP deductive verification on a specific function. \
    Requires function_name to identify the target. \
    Returns proof task statistics. This may take several minutes.")]
async fn run_wp(
    &self,
    Parameters(params): Parameters<RunWpParams>,
) -> Result<CallToolResult, McpError>
```

**参数**：
- `function_name: String` — 必需，要验证的函数名
- `prover: Option<String>` — 可选，如 "Alt-Ergo:2.6.2"
- `timeout: Option<u32>` — 可选，秒数

**算法**：
```
1. if let Some(prover) = params.prover {
       // SET 是排队执行的（非即时），需要 poll_loop 等待响应
       self.client.set("plugins.wp.setProvers", json!([prover])).await?;
   }
2. if let Some(timeout) = params.timeout {
       self.client.set("plugins.wp.setTimeout", json!(timeout)).await?;
   }
3. // 获取函数声明 marker（如 "#F24"），缓存未命中时自动刷新
   let info = self.resolve_function_or_refresh(&params.function_name).await?;
   let decl_marker = info.declaration;
4. // printDeclaration 必须先调用，以在服务器内注册 PVDecl 等标记
   self.client.get("kernel.ast.printDeclaration", json!(decl_marker)).await?;
5. // startProofs 接受 AST.Marker (PVDecl 类型)，非 AST.Decl (#F)
   // 将 #F<vid> 转换为 #v<vid>（两者共用相同的 Cil varinfo.vid）
   let pvdecl_marker = decl_marker.replace("#F", "#v");
   self.client.exec(
       "plugins.wp.startProofs", json!(pvdecl_marker), Duration::from_secs(600),
   ).await?;
6. let tasks = self.client.get(
       "plugins.wp.getScheduledTasks", json!(null),
   ).await?;
7. {
       let mut state = self.state.write().await;
       state.set_wp_completed();
   }
8. 组装返回 JSON（tasks）
```

**关键协议发现**（集成测试验证）：
- `setProvers`/`setTimeout`：SET 命令被服务器排队（非即时执行），需 POLL 触发处理
- `startProofs`：只接受 `AST.Marker` 类型（`#v`, `#s`, `#k` 等），不接受 `AST.Decl`（`#F`）
- 调用 `printDeclaration` 前，PVDecl 标记未在服务器的 marker 表中注册，会被拒绝为 "invalid marker"

### 7.12 工具方法：`get_verification_status`

```rust
#[tool(description = "Get comprehensive verification status: \
    property counts by category, EVA/WP analysis state.")]
async fn get_verification_status(&self) -> Result<CallToolResult, McpError>
```

**算法**：
```
1. // 重置增量游标
   self.client.get("kernel.properties.reloadStatus", json!(null)).await?;
   let properties = self.client.fetch_all("kernel.properties.fetchStatus").await?;
2. let (project_loaded, eva_state, wp_state) = {
       let state = self.state.read().await;
       (state.project_loaded, state.eva_completed, state.wp_completed)
   };
3. // 按状态分类汇总属性
   let mut summary: HashMap<String, u64> = HashMap::new();  // status → count
   let mut by_kind: HashMap<String, u64> = HashMap::new();   // kind → count
   for prop in &properties {
       let status = prop["status"].as_str().unwrap_or("unknown");
       *summary.entry(status.to_string()).or_default() += 1;
       let kind = prop["kind"].as_str().unwrap_or("unknown");
       *by_kind.entry(kind.to_string()).or_default() += 1;
   }
4. let mut result = json!({
       "total_properties": properties.len(),
       "by_status": summary,
       "by_kind": by_kind,
   });
5. if eva_state {
       let comp = self.client.get(
           "plugins.eva.general.getComputationState", json!(null),
       ).await.unwrap_or(json!(null));
       result["eva"] = comp;
   }
6. if wp_state {
       let tasks = self.client.get(
           "plugins.wp.getScheduledTasks", json!(null),
       ).await.unwrap_or(json!(null));
       result["wp"] = tasks;
   }
7. result["session"] = json!({
       "project_loaded": project_loaded,
       "eva_completed": eva_state,
       "wp_completed": wp_state,
   });
8. // 附带原始 properties 供 Agent 深入查看
   result["properties"] = json!(properties);
9. 返回 result JSON
```

**注意**：`status` 和 `kind` 的字段名需实现阶段实测确认（`fetchStatus` 返回的 Array 条目结构取决于 Frama-C 注册时定义的列）。汇总逻辑在确认字段名后可能需要调整。

---

## 8. 模块 `main`

**文件**：`src/main.rs`

### 8.1 CLI 参数

```rust
#[derive(clap::Parser)]
#[command(name = "frama-c-mcp-server")]
#[command(about = "MCP server for Frama-C formal verification")]
struct Cli {
    /// Unix socket path of a running Frama-C server
    #[arg(long)]
    socket: String,
}
```

**注意**：Phase 1 仅支持连接已有的 Frama-C Server。`--socket` 是必须参数。

### 8.2 `main`

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()>
```

**算法**：
```
1. tracing_subscriber::fmt()
       .with_writer(std::io::stderr)   // 必须写 stderr，stdout 留给 MCP JSON-RPC
       .with_env_filter(EnvFilter::from_default_env())
       .init();
2. let cli = Cli::parse();
3. let state = Arc::new(RwLock::new(SessionState::default()));
4. tracing::info!("connecting to Frama-C server at {}", cli.socket);
5. let client = FramaCClient::connect(&cli.socket, state.clone()).await?;
6. tracing::info!("connected, project loaded");
7. let server = FramaCMcpServer::new(client, state);
8. // 需要 `use rmcp::ServiceExt` 引入 serve 方法
   let service = server.serve(rmcp::transport::io::stdio()).await?;
9. tracing::info!("MCP server running on stdio");
10. service.waiting().await?;
11. Ok(())
```

**启动命令**：
```bash
# 先启动 Frama-C
frama-c test.c -server-socket /tmp/frama-c.sock &

# 再启动 MCP Server
frama-c-mcp-server --socket /tmp/frama-c.sock
```

---

## 9. 错误处理策略

### 9.1 分层原则

```
frama_c 层                        mcp 层
┌─────────────────┐              ┌──────────────────┐
│ codec / transport│              │ server (tools)   │
│ → FramaCError   │──────────────│ → McpError       │──→ MCP 客户端
│                 │  From<> 转换  │ (via .map_err)   │
└─────────────────┘              └──────────────────┘
```

- `frama_c` 模块内部全部使用 `Result<T, FramaCError>`
- `mcp::server` 的工具方法在返回时通过 `?` 自动转换（依赖 `impl From<FramaCError> for McpError`）
- `main` 使用 `anyhow::Result` 处理启动错误

### 9.2 可恢复 vs 不可恢复

| 错误 | 类型 | 处理 |
|------|------|------|
| Frama-C 返回 ERROR | 可恢复 | 转为 MCP error 返回给 Agent，Agent 可修正参数重试 |
| Frama-C 返回 REJECTED | 可恢复 | 转为 MCP invalid_request，Agent 需等待当前操作完成 |
| EXEC 超时 | 可恢复 | 发送 KILL，转为 MCP error，Agent 可调整参数重试 |
| 函数名未找到 | 可恢复 | 转为 MCP invalid_params，Agent 可检查函数名 |
| 连接断开 | 不可恢复 | I/O error 传播到 MCP 层，Agent 需重启 MCP Server |
| JSON 解析失败 | 不可恢复 | 转为 MCP internal_error，记日志 |
| 帧格式错误 | 不可恢复 | 转为 MCP internal_error，记日志 |

### 9.3 工具方法中的错误转换模式

每个工具方法统一使用以下模式：

```rust
async fn tool_method(&self, ...) -> Result<CallToolResult, McpError> {
    // FramaCError 通过 ? 自动转为 McpError
    let data = self.client.get("request.name", json!(params)).await?;

    // 组装成功结果
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&data).unwrap_or_default()
    )]))
}
```

### 9.4 日志策略

| 级别 | 用途 |
|------|------|
| `error!` | 不可恢复错误（连接断开、帧格式错误） |
| `warn!` | 非预期但可继续的情况（POLL 中收到无关响应） |
| `info!` | 连接/断开、工具调用 |
| `debug!` | 协议级消息（发送/接收的 JSON） |
| `trace!` | 帧级数据（raw bytes、分帧解码细节） |

通过 `RUST_LOG` 环境变量控制（`tracing-subscriber` 的 `EnvFilter`）。
