# Frama-C Server Protocol — 开发者参考手册

> **适用版本**：Frama-C 31.0 (Gallium)
> **验证日期**：2026-02-20
> **验证方式**：Unix Socket 实测 + Frama-C/Ivette 源码审计
>
> 本文档是从实际开发中提炼的 Frama-C Server 协议参考，涵盖协议规范、API 清单和实现中的关键陷阱。适用于任何需要与 Frama-C Server 通信的客户端项目。
>
> **注意**：本文描述的是 **Frama-C 自身**的 server 协议与启动方式（含手工 `-server-socket`、`-server-zmq` 等选项），供协议层参考。**使用本仓库的 MCP server 时不需要手工启动 Frama-C**——它会自行拉起（见 [README](../../README.md)）。

---

## 目录

1. [传输与分帧](#1-传输与分帧)
2. [命令与响应格式](#2-命令与响应格式)
3. [请求处理模型](#3-请求处理模型) ★ 核心
4. [连接生命周期](#4-连接生命周期)
5. [增量 Fetch 协议](#5-增量-fetch-协议) ★ 重要
6. [Marker 系统](#6-marker-系统) ★ 重要
7. [API 清单](#7-api-清单)
8. [关键陷阱与注意事项](#8-关键陷阱与注意事项) ★ 必读
9. [参考实现模式](#9-参考实现模式)
10. [源码参考](#10-源码参考)

---

## 1. 传输与分帧

### 1.1 传输方式

| 方式 | 启动参数 | 说明 |
|------|---------|------|
| Unix Socket | `-server-socket <path>` | 推荐，支持交互式长连接 |
| Batch 模式 | `-server-batch <file>` | 从文件读取命令，一次性执行 |
| ZMQ | `-server-zmq <endpoint>` | 部分发行版未编译 ZMQ 支持 |

每种传输方式的上层协议完全相同。

### 1.2 分帧格式

每条消息由 **长度前缀 + JSON 负载** 组成：

| 前缀 | 格式 | 最大负载 | 示例 |
|------|------|---------|------|
| `S` | `S` + 3位小写十六进制 | 4095 bytes | `S01a{"cmd":"GET",...}` |
| `L` | `L` + 7位小写十六进制 | 268 MB | `L000001a{"cmd":"GET",...}` |
| `W` | `W` + 15位小写十六进制 | 理论无限 | 极少使用 |

**编码规则**（参考 `server_socket.ml:135-139`）：
- 负载 ≤ 4095 字节：使用 `S` 前缀，`sprintf "S%03x"`
- 负载 ≤ 268435455 字节：使用 `L` 前缀，`sprintf "L%07x"`
- 否则：使用 `W` 前缀，`sprintf "W%015x"`
- 十六进制为**小写**（OCaml `%x` 格式）

**解码**：先读 1 字节判断前缀类型，再读对应位数的十六进制长度，最后读取指定长度的 JSON 字符串。

---

## 2. 命令与响应格式

### 2.1 命令（Client → Server）

```json
// GET — 同步查询
{"cmd":"GET", "id":"RQ.0", "request":"kernel.ast.getFiles", "data":null}

// SET — 修改配置/状态
{"cmd":"SET", "id":"RQ.1", "request":"plugins.wp.setTimeout", "data":10}

// EXEC — 异步执行（长时间操作）
{"cmd":"EXEC", "id":"RQ.2", "request":"plugins.eva.general.compute", "data":null}

// POLL — 轮询排队结果（无需 id）
"POLL"

// KILL — 取消异步操作
{"cmd":"KILL", "id":"RQ.2"}

// SHUTDOWN — 关闭服务端进程
"SHUTDOWN"

// SIGON/SIGOFF — 信号订阅（高级用法，一般不需要）
{"cmd":"SIGON", "id":"SIG.0", "request":"kernel.ast.signalFunctions"}
{"cmd":"SIGOFF", "id":"SIG.0", "request":"kernel.ast.signalFunctions"}
```

**注意**：
- `id` 由客户端分配，任意字符串，用于匹配响应
- `POLL` 和 `SHUTDOWN` 是纯 JSON 字符串（不是对象）
- KILL 的 `id` 必须与要取消的 EXEC 的 `id` 相同

### 2.2 响应（Server → Client）

```json
// DATA — 成功返回
{"res":"DATA", "id":"RQ.0", "data":["/tmp/test.c"]}

// ERROR — 请求执行出错
{"res":"ERROR", "id":"RQ.1", "msg":"Expected int, got null: null"}

// REJECTED — 请求名不存在
{"res":"REJECTED", "id":"RQ.2"}

// SIGNAL — EXEC 中间信号（表示仍在执行）
{"res":"SIGNAL", "id":"RQ.2"}

// KILLED — EXEC 被取消
{"res":"KILLED", "id":"RQ.2"}

// CMDLINEON — 服务端正在处理命令行参数
"CMDLINEON"

// CMDLINEOFF — 命令行处理完毕，服务端就绪
"CMDLINEOFF"
```

**注意**：`CMDLINEON`/`CMDLINEOFF` 是纯 JSON 字符串。

---

## 3. 请求处理模型 ★

这是最容易出错的部分。Frama-C Server 有两种请求处理模式：

### 3.1 立即执行（GET）

```
Client → GET → Server 立即处理 → DATA/ERROR 响应
```

- GET 请求**不进入队列**，立即处理并返回
- 即使有 EXEC 正在运行，GET 也能被处理
- 客户端发送 GET 后，下一条消息一定是对应的 DATA/ERROR/REJECTED

### 3.2 排队执行（SET 和 EXEC）

```
Client → SET/EXEC → Server 放入命令队列
Client → POLL → Server 处理队列 → 响应
Client → POLL → Server 处理队列 → 响应
...
```

- **SET 和 EXEC 都进入命令队列**，不会立即执行
- 服务端只在收到 `POLL` 时才处理队列中的命令
- 必须持续发送 POLL 直到收到该请求的 DATA/ERROR/REJECTED/KILLED 响应

> **⚠ 关键陷阱**：很多开发者误以为 SET 是同步的（发送后等 DATA）。实际上 SET 和 EXEC 完全一样，都需要 POLL 驱动。如果不发 POLL，SET 请求永远不会被处理，客户端会一直等待直到超时。

### 3.3 POLL 机制详解

```
推荐实现（伪代码）：

function poll_loop(request_id, timeout):
    deadline = now() + timeout

    // 先检查是否有即时响应（快速操作可能立刻完成）
    resp = recv(500ms)
    if resp.id == request_id and resp is DATA/ERROR/REJECTED:
        return resp

    while now() < deadline:
        sleep(100ms)
        send("POLL")
        resp = recv(500ms)
        match resp:
            DATA/ERROR/KILLED {id == request_id} → return resp
            SIGNAL → continue  // 仍在执行
            None → continue    // 无待发送消息
            其他 → 丢弃（可能是 stale 响应）

    // 超时，发送 KILL 取消
    send(KILL{id: request_id})
    return TimeoutError
```

**POLL 间隔**：建议 100ms（Ivette 默认 50ms）。

### 3.4 响应 ID 匹配 ★

> **⚠ 关键陷阱**：必须校验响应的 `id` 字段是否匹配当前请求。

场景：如果前一个 SET 请求超时后被 KILL，残留的响应可能在后续请求中返回。不校验 ID 会导致：
- 收到前一个请求的 DATA，误认为是当前请求的响应
- 数据类型完全错误，导致解析失败或逻辑错误

**正确做法**：
```
function wait_for_id(request_id, timeout):
    loop:
        resp = recv(timeout)
        if resp.id == request_id:
            return resp          // 匹配，返回
        else:
            log_warn("丢弃 stale 响应 id={}", resp.id)
            continue             // 不匹配，丢弃继续等
```

---

## 4. 连接生命周期

### 4.1 启动与握手

```
1. 启动 Frama-C Server：
   frama-c <c_files> -server-socket /tmp/frama-c.sock

2. 客户端连接 Unix Socket

3. 等待握手完成：
   Server → "CMDLINEON"     // 可能出现（如果有命令行参数要处理）
   Server → "CMDLINEOFF"    // 就绪信号
```

**实现注意事项**：
- Frama-C Server 不会主动推送消息——需要客户端先发送一条 GET 请求，才能触发服务端发送排队的 CMDLINEON/CMDLINEOFF
- 建议在连接后立即发送一条探测 GET（如 `getFiles`），然后在读取循环中等待 CMDLINEOFF
- 超时建议 30 秒（命令行处理可能包含分析操作）

### 4.2 单客户端限制

Frama-C Server **同一时刻只接受一个客户端连接**。第二个客户端的连接会阻塞直到第一个断开。

### 4.3 关闭

发送 `"SHUTDOWN"` 命令会**终止 Frama-C Server 进程**（不只是断开连接）。如果只想断开连接而保留服务端，直接关闭 Socket 即可。

---

## 5. 增量 Fetch 协议 ★

Frama-C Server 的 Array API（如 `fetchFunctions`、`fetchStatus`）使用增量分页模式。

### 5.1 基本流程

```json
// 请求：data 参数是 batch capacity（一次最多返回多少条）
{"cmd":"GET", "id":"RQ.0", "request":"kernel.ast.fetchFunctions", "data":20000}

// 响应
{"res":"DATA", "id":"RQ.0", "data":{
  "reload": false,           // true = 服务端数据已重置，清空客户端缓存
  "updated": [{...}, ...],   // 本批次新增/更新的条目
  "removed": [],             // 本批次删除的条目（增量更新时使用）
  "pending": 5               // 剩余未返回条目数
}}
```

### 5.2 完整获取算法

```
function fetch_all(request_name):
    all_entries = []
    loop:
        resp = GET(request_name, 20000)  // batch capacity = 20000
        if resp.data.reload == true:
            all_entries.clear()  // 服务端数据重置，丢弃之前累积的
        all_entries.extend(resp.data.updated)
        if resp.data.pending == 0:
            break
    return all_entries
```

### 5.3 增量消费语义 ★

> **⚠ 关键陷阱**：`fetchX` 的数据**只能消费一次**。第一次调用返回所有条目，第二次调用返回空（`updated: []`, `pending: 0`）。

如果需要重新获取全部数据，必须先调用 `reloadX`：

```
GET("kernel.ast.reloadFunctions", null)   // 重置增量游标
GET("kernel.ast.fetchFunctions", 20000)   // 现在返回全部数据
```

**对应的 reload 请求**：

| Fetch 请求 | Reload 请求 |
|-----------|------------|
| `kernel.ast.fetchFunctions` | `kernel.ast.reloadFunctions` |
| `kernel.ast.fetchGlobals` | `kernel.ast.reloadGlobals` |
| `kernel.properties.fetchStatus` | `kernel.properties.reloadStatus` |
| `plugins.wp.fetchGoals` | `plugins.wp.reloadGoals` |
| `plugins.eva.general.fetchFunctions` | `plugins.eva.general.reloadFunctions` |

### 5.4 数据参数

`fetchX` 的 `data` 参数是 **batch capacity**（整数），不是页码或偏移量。表示本次调用最多返回多少条。建议使用 20000（与 Ivette 客户端一致）。

---

## 6. Marker 系统 ★

Frama-C Server 使用 **marker** 作为 AST 节点的全局标识符。大多数 API 接受 marker 作为参数，而非函数名或文件路径。

### 6.1 Marker 类型

| 前缀 | 类型 | 含义 | 示例 |
|------|------|------|------|
| `#F` | AST.Decl（函数声明） | 函数的声明节点 | `#F24` |
| `#v` | AST.Marker（PVDecl，变量声明） | 函数作为变量 | `#v24` |
| `#s` | AST.Marker（语句） | 语句节点 | `#s2` |
| `#k` | AST.Marker（kinstr） | 指令节点 | `#k13` |
| `#p` | AST.Marker（属性） | ACSL 属性 | `#p3` |
| `kf#` | 函数 key（内部标识） | fetchFunctions 返回 | `kf#24` |

### 6.2 Marker 注册机制 ★

> **⚠ 关键陷阱**：Marker 必须先**注册**到服务端的 marker table 中才能被接受。未注册的 marker 会返回 "invalid marker" 错误。

注册方式：调用 `printDeclaration` 会触发服务端解析函数声明，同时将函数体内所有语句、表达式的 marker 注册到 marker table。

```
// 先注册 marker（通过打印函数声明）
GET("kernel.ast.printDeclaration", "#F24")

// 注册后才能使用函数体内的 marker
GET("plugins.eva.values.getValues", {"target": "#s2"})
```

### 6.3 Marker 转换

`#F`（AST.Decl）和 `#v`（AST.Marker/PVDecl）使用相同的数字后缀（都是 CIL `varinfo.vid`），但语义不同：

- `#F24`：函数声明，用于 `printDeclaration`、`scope` 过滤等
- `#v24`：变量声明 marker，用于 `startProofs` 等需要 AST.Marker 的 API

**转换**：`#F<vid>` ↔ `#v<vid>`，只需替换前缀。

### 6.4 `fetchFunctions` 返回的数据结构

```json
{
  "name": "abs_val",              // 函数名
  "key": "kf#24",                 // 函数内部 key
  "decl": "#F24",                 // 声明 marker (AST.Decl)
  "signature": "int abs_val(int x);",  // 函数签名
  "sloc": {                       // 源码位置
    "file": "/path/to/file.c",
    "dir": "/path/to",
    "base": "file.c",
    "line": 6
  }
}
```

**注意**：文件和行号在嵌套的 `sloc` 对象中（不是顶层字段）。

---

## 7. API 清单

### 7.1 Request 命名规则

| 来源 | 格式 | 示例 |
|------|------|------|
| Kernel（无 name） | `kernel.<name>` | — |
| Kernel（name="X"）| `kernel.X.<name>` | `kernel.ast.getFiles` |
| Plugin（无 name） | `plugins.<plugin>.<name>` | `plugins.callgraph.compute` |
| Plugin（name="X"）| `plugins.<plugin>.X.<name>` | `plugins.eva.values.getValues` |

### 7.2 自动生成的 Request

Frama-C 框架为注册的 State/Array 自动生成 request：

| 注册方式 | 生成的 Request |
|----------|---------------|
| `register_state ~name:"X"` | `getX` (GET), `setX` (SET) |
| `register_value ~name:"X"` | `getX` (GET) |
| `register_array ~name:"X"` | `fetchX` (GET 分页), `reloadX` (GET) |

### 7.3 Kernel AST

| Request | Kind | 参数 | 返回 |
|---------|------|------|------|
| `kernel.ast.compute` | EXEC | `null` | `null`（触发 AST 重建）|
| `kernel.ast.getFiles` | GET | `null` | `["/path/to/file.c", ...]` |
| `kernel.ast.setFiles` | SET | `["/path/to/file.c"]` | — |
| `kernel.ast.getFunctions` | GET | `null` | `["#F24", "#F31", ...]`（marker 列表）|
| `kernel.ast.getMainFunction` | GET | `null` | `"#F36"` |
| `kernel.ast.fetchFunctions` | GET | `20000`（capacity） | 分页结果（见§5）|
| `kernel.ast.reloadFunctions` | GET | `null` | `null` |
| `kernel.ast.fetchGlobals` | GET | `20000` | 分页结果 |
| `kernel.ast.reloadGlobals` | GET | `null` | `null` |
| `kernel.ast.printDeclaration` | GET | `"#F24"`（marker 字符串） | 带 ACSL 注解的声明 AST |
| `kernel.ast.getMarkerAt` | GET | `{file, line, column}` | marker |
| `kernel.ast.getInformation` | GET | `null` | 信息类型列表 |

### 7.4 Kernel Properties

| Request | Kind | 参数 | 返回 |
|---------|------|------|------|
| `kernel.properties.fetchStatus` | GET | `20000` | 分页属性列表 |
| `kernel.properties.reloadStatus` | GET | `null` | `null` |
| `kernel.properties.propKindTags` | GET | `null` | 属性类型枚举 |
| `kernel.properties.propStatusTags` | GET | `null` | 验证状态枚举 |
| `kernel.properties.alarmsTags` | GET | `null` | Alarm 类型枚举 |

**`fetchStatus` 返回的属性结构**：
```json
{
  "key": "ip#5",
  "kind": "ensures",             // "requires", "ensures", "behavior", "instance", "exits", "terminates"
  "status": "valid",             // "valid", "unknown", "invalid", "never_tried"
  "scope": "#F24",               // 所属函数的声明 marker
  "descr": "ensures abs_val ≥ 0",
  "predicate": "\\result ≥ 0",
  "source": {"file": "...", "line": 10, "dir": "...", "base": "..."},
  "alarm": false,
  "alarm_descr": "",
  "from_libc": false,
  "names": [],
  "kinstr": null
}
```

### 7.5 Kernel Services

| Request | Kind | 参数 | 返回 |
|---------|------|------|------|
| `kernel.services.getConfig` | GET | `null` | `{version, codename, datadir, ...}` |
| `kernel.services.load` | SET | 文件路径 | — |
| `kernel.services.save` | SET | 文件路径 | — |
| `kernel.services.getLogs` | GET | `null` | 最近日志（最多 100 条）|

### 7.6 EVA General

| Request | Kind | 参数 | 返回 |
|---------|------|------|------|
| `plugins.eva.general.compute` | EXEC | `null` | `null`（运行 EVA 分析）|
| `plugins.eva.general.abort` | GET | `null` | — |
| `plugins.eva.general.getComputationState` | GET | `null` | `"computed"` / `"not_computed"` / ... |
| `plugins.eva.general.getProgramStats` | GET | `null` | 分析统计对象 |
| `plugins.eva.general.getCallers` | GET | 声明 marker | 调用者列表 |
| `plugins.eva.general.getCallees` | GET | marker | 被调用者列表 |
| `plugins.eva.general.getDeadCode` | GET | 声明 marker | 死代码信息 |
| `plugins.eva.general.fetchFunctions` | GET | `20000` | 函数 + EVA 分析状态 |
| `plugins.eva.general.fetchProperties` | GET | `20000` | 属性 + 优先级 + 污点 |

### 7.7 EVA Values

| Request | Kind | 参数 | 返回 |
|---------|------|------|------|
| `plugins.eva.values.getValues` | GET | `{"target": "#s2"}` | 值域信息 |
| `plugins.eva.values.getCallstacks` | GET | marker | 调用栈列表 |
| `plugins.eva.values.getCallstackInfo` | GET | 调用栈索引 | 调用栈详情 |

**`getValues` 返回示例**：
```json
{
  "vBefore": {"alarms": [], "pointedVars": [], "value": "{1}"},
  "vThen":   {"alarms": [], "pointedVars": [], "value": "{1}"},
  "vElse":   {"alarms": [], "pointedVars": [], "value": "Unreachable"}
}
```

### 7.8 WP

| Request | Kind | 参数 | 返回 |
|---------|------|------|------|
| `plugins.wp.startProofs` | EXEC | `"#v24"`（PVDecl marker） | `null` |
| `plugins.wp.getScheduledTasks` | GET | `null` | `{active, done, procs, todo}` |
| `plugins.wp.getProvers` | GET | `null` | `["Alt-Ergo:2.6.2"]` |
| `plugins.wp.setProvers` | SET | `["Alt-Ergo"]` | — |
| `plugins.wp.getTimeout` | GET | `null` | `10` |
| `plugins.wp.setTimeout` | SET | `10` | — |
| `plugins.wp.fetchGoals` | GET | `20000` | 分页证明目标 |
| `plugins.wp.reloadGoals` | GET | `null` | `null` |
| `plugins.wp.generateRTEGuards` | EXEC | — | 生成 RTE 断言 |
| `plugins.wp.cancelProofTasks` | SET | — | 取消证明任务 |

### 7.9 Callgraph

| Request | Kind | 参数 | 返回 |
|---------|------|------|------|
| `plugins.callgraph.compute` | EXEC | `null` | `null` |
| `plugins.callgraph.getCallgraph` | GET | `null` | `{edges: [...], vertices: [...]}` |
| `plugins.callgraph.getIsComputed` | GET | `null` | `true/false` |

**`getCallgraph` 返回示例**：
```json
{
  "edges": [
    {"src": "#F36", "dst": "#F24", "kind": "both"},
    {"src": "#F36", "dst": "#F31", "kind": "both"}
  ],
  "vertices": [
    {"name": "main", "decl": "#F36", "root": "#F36", "is_root": true},
    {"name": "abs_val", "decl": "#F24", "root": "#F24", "is_root": true}
  ]
}
```

---

## 8. 关键陷阱与注意事项 ★

### 8.1 SET 不是同步的

**问题**：发送 SET 后等待 DATA 响应，永远等不到（超时）。

**原因**：SET 和 EXEC 一样进入命令队列，只有发送 POLL 才会触发服务端处理队列。

**正确做法**：SET 请求也必须使用 POLL 循环等待结果。

```
// ✗ 错误
send(SET("plugins.wp.setTimeout", 10))
resp = recv()  // 永远超时

// ✓ 正确
send(SET("plugins.wp.setTimeout", 10))
resp = poll_loop("RQ.1", timeout=30s)
```

### 8.2 必须校验响应 ID

**问题**：前一个请求超时后，残留响应会在后续请求中返回，导致数据错乱。

**场景**：
1. 发送 SET(id=RQ.1) → 超时
2. 发送 GET(id=RQ.2) → 收到 DATA(id=RQ.1)（前一个请求的响应）
3. 误认为是 GET 的结果 → 数据类型完全错误

**正确做法**：始终校验 `resp.id == request_id`，不匹配的响应直接丢弃。

### 8.3 增量 Fetch 只能消费一次

**问题**：第二次调用 `fetchX` 返回空数据。

**原因**：增量 fetch 是有状态的，每条记录只返回一次。

**正确做法**：需要重新获取时，先调用 `reloadX` 重置游标。

### 8.4 `param_opt` 参数必须省略，不能传 null

**问题**：`getValues` 传 `{"target": "#s2", "callstack": null}` → "Expected int, got null"。

**原因**：Frama-C 的可选参数（`param_opt`）要求**完全不出现在 JSON 对象中**。传 `null` 会被当作值来解析。

**正确做法**：
```json
// ✗ 错误
{"target": "#s2", "callstack": null}

// ✓ 正确（省略 callstack 字段）
{"target": "#s2"}
```

### 8.5 `printDeclaration` 参数是纯字符串

**问题**：传 `{"marker": "#F24"}` → 类型错误。

**正确做法**：直接传 marker 字符串作为 `data`。

```json
// ✗ 错误
{"cmd":"GET", "request":"kernel.ast.printDeclaration", "data":{"marker":"#F24"}}

// ✓ 正确
{"cmd":"GET", "request":"kernel.ast.printDeclaration", "data":"#F24"}
```

### 8.6 WP `startProofs` 需要 PVDecl marker

**问题**：传 `#F24`（AST.Decl）→ "invalid marker"。

**原因**：`startProofs` 只接受 AST.Marker 类型。`#F` 是 AST.Decl，不是 AST.Marker。

**正确做法**：
1. 先调用 `printDeclaration("#F24")` 注册函数的 marker
2. 将 `#F24` 转换为 `#v24`（替换前缀，数字相同）
3. 传 `#v24` 给 `startProofs`

```
GET("kernel.ast.printDeclaration", "#F24")   // 步骤 1：注册 marker
EXEC("plugins.wp.startProofs", "#v24")       // 步骤 2-3：使用 PVDecl marker
```

### 8.7 Frama-C Server 不主动推送

Frama-C Server 在客户端首次发送命令前，不会主动推送任何消息（包括 CMDLINEON/CMDLINEOFF）。客户端连接后必须主动发送一条请求才能触发服务端的响应流。

---

## 9. 参考实现模式

### 9.1 客户端架构

```
FramaCClient
├── transport: Unix Socket 连接（read/write）
├── codec: S/L 分帧编解码
├── counter: u64（生成递增 request ID）
├── get(request, data) → 发送 GET + wait_for_id()
├── set(request, data) → 发送 SET + poll_loop()
├── exec(request, data, timeout) → 发送 EXEC + poll_loop()
├── fetch_all(request) → 循环 GET 直到 pending==0
└── shutdown() → 发送 SHUTDOWN
```

### 9.2 建议的 POLL 参数

| 参数 | 建议值 | 说明 |
|------|-------|------|
| POLL 间隔 | 100ms | Ivette 用 50ms，MCP 场景 100ms 足够 |
| 单次 recv 超时 | 500ms | 无响应视为"队列为空" |
| GET 总超时 | 10s | GET 是同步的，通常很快 |
| SET 总超时 | 30s | SET 排队处理，略慢 |
| EXEC 总超时 | 600s | EVA/WP 可能运行数分钟 |
| Fetch batch | 20000 | 与 Ivette 一致 |

### 9.3 Marker 缓存模式

建议在客户端维护函数名 → marker 的映射缓存：

```
连接时：
  entries = fetch_all("kernel.ast.fetchFunctions")
  for entry in entries:
      cache[entry.name] = {
          marker: entry.key,
          declaration: entry.decl,
          signature: entry.signature,
          file: entry.sloc.file,
          line: entry.sloc.line,
      }

使用时：
  info = cache["abs_val"]
  GET("kernel.ast.printDeclaration", info.declaration)  // 使用 #F marker
  EXEC("plugins.wp.startProofs", info.declaration.replace("#F", "#v"))  // 使用 #v marker
```

---

## 10. 源码参考

| 文件 | 内容 |
|------|------|
| `src/plugins/server/server_socket.ml:135-139` | S/L/W 分帧编码实现 |
| `src/plugins/server/main.ml:296-297` | GET 立即执行 vs SET/EXEC 排队 |
| `src/plugins/server/main.ml:352-369` | POLL 命令处理逻辑 |
| `src/plugins/server/states.ml:302` | Fetch capacity 解析 |
| `src/plugins/server/request.ml` | Request 注册和分发 |
| `ivette/src/frama-c/states.ts:429-442` | Ivette 分页循环（batch=20000）|
| `ivette/src/frama-c/server.ts:826-831` | Ivette 请求/响应关联模型 |
| `src/plugins/wp/wpApi.ml` | WP API 注册（startProofs 参数类型）|
