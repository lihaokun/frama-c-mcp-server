# 调研报告：Frama-C Server API 实测

> **日期**: 2025-02-19
> **目标**: 搞清 Frama-C 31.0 Server 的真实 API，验证 v2.2 设计文档的假设，建立 request → tool 映射

---

## 1. 协议实测结果

### 1.1 协议格式（v2.2 设计有误）

| 维度 | v2.2 假设 | 实际 |
|------|----------|------|
| 命令字段 | `"kind"` | `"cmd"` |
| 响应字段 | `"kind"` | `"res"` |
| 错误响应 | `{"kind":"ERROR","data":{...}}` | `{"res":"ERROR","msg":"..."}` |
| POLL 命令 | 未提及 | `"POLL"` (JSON 字符串) |
| SHUTDOWN | 未提及 | `"SHUTDOWN"` (JSON 字符串) |
| 额外命令 | 无 | `SIGON`, `SIGOFF`, `KILL` |
| 额外响应 | 无 | `CMDLINEON`, `CMDLINEOFF`, `KILLED` |
| 分帧格式 | `S`+3hex / `L`+7hex | 正确，另有 `W`+15hex |

**正确的命令格式：**
```json
{"cmd":"GET","id":"RQ.0","request":"kernel.ast.getFunctions","data":null}
```

**正确的响应格式：**
```json
{"res":"DATA","id":"RQ.0","data":["#F990","#F998"]}
{"res":"ERROR","id":"RQ.1","msg":"Expected object, got null: null"}
{"res":"REJECTED","id":"RQ.2"}
```

**关键协议行为：**
- 连接后先等 `CMDLINEOFF` 再发请求（Frama-C 先执行命令行解析）
- EXEC 是异步的，发送后需反复 `POLL` 获取结果
- GET 可在 EXEC 运行期间执行
- SET/EXEC 在命令行执行期间排队

### 1.2 Request 命名规则

| 来源 | 前缀规则 | 示例 |
|------|----------|------|
| Kernel（无 `~name:`）| `kernel.<name>` | — |
| Kernel（`~name:"X"`）| `kernel.X.<name>` | `kernel.ast.getFunctions` |
| Plugin（无 `~name:`）| `plugins.<plugin>.<name>` | `plugins.callgraph.compute` |
| Plugin（`~name:"X"`）| `plugins.<plugin>.X.<name>` | `plugins.eva.values.getValues` |

### 1.3 自动生成的 Request

Frama-C Server 框架自动为注册的 State/Array/Dictionary 生成 request：

| 注册方式 | 生成的 Request |
|----------|---------------|
| `register_state ~name:"X"` | `getX` (GET), `setX` (SET), `signalX` (signal) |
| `register_value ~name:"X"` | `getX` (GET), `signalX` (signal) |
| `register_array ~name:"X"` | `fetchX` (GET, 分页), `reloadX` (GET), `signalX` (signal) |
| `Request.dictionary ~name:"X"` | `XTags` (GET) |

---

## 2. 已验证的 Request 清单（实测通过）

### 2.1 Kernel AST（`kernel.ast.*`）

| Request | Kind | 测试结果 | 返回示例 |
|---------|------|---------|---------|
| `kernel.ast.compute` | EXEC | DATA | `null` |
| `kernel.ast.getFunctions` | GET | DATA | `["#F990","#F998"]`（marker ID 列表）|
| `kernel.ast.getFiles` | GET | DATA | `["/tmp/test_sort.c"]` |
| `kernel.ast.getMainFunction` | GET | DATA | `"#F998"` |
| `kernel.ast.getInformation` | GET | DATA | 信息类型列表 |
| `kernel.ast.setFiles` | SET | — | 设置源文件 |
| `kernel.ast.getMarkerAt` | GET | ERROR(需参数) | `{file,line,column}` |
| `kernel.ast.printDeclaration` | GET | ERROR(需参数) | 需 decl marker |
| `kernel.ast.parseExpr` | GET | — | 解析 C 表达式 |
| `kernel.ast.parseLval` | GET | — | 解析 C 左值 |
| `kernel.ast.fetchFunctions` | GET | ERROR(需 int) | 分页查询函数列表 |
| `kernel.ast.reloadFunctions` | GET | DATA | `null` |
| `kernel.ast.fetchGlobals` | GET | ERROR(需 int) | 分页查询全局变量 |
| `kernel.ast.reloadGlobals` | GET | DATA | — |

**注意**：`getFunctions` 返回的是 marker ID（如 `#F990`），不是函数名。需要 `fetchFunctions` 获取函数详情（名称、签名等），或 `printDeclaration` 打印函数声明。

### 2.2 Kernel Services（`kernel.services.*`）

| Request | Kind | 测试结果 |
|---------|------|---------|
| `kernel.services.getConfig` | GET | DATA: `{version, codename, version_codename, datadir, pluginpath}` |
| `kernel.services.load` | SET | 加载存档文件 |
| `kernel.services.save` | SET | 保存会话 |
| `kernel.services.setLogs` | SET | 开关日志监控 |
| `kernel.services.getLogs` | GET | 获取最近日志（最多 100 条）|
| `kernel.services.fetchMessage` | GET | 分页查询日志消息 |

**⚠ v2.2 错误**：使用了 `kernel.getConfig` 和 `kernel.getLogs`，正确名称需要 `services` 子包。

### 2.3 Kernel Project（`kernel.project.*`）

| Request | Kind | 测试结果 |
|---------|------|---------|
| `kernel.project.getList` | GET | DATA: `[{"id":"default","name":"default","current":true}]` |
| `kernel.project.create` | SET | 创建新项目 |

**⚠ 注意**：`getCurrent`、`setCurrent`、`getOn`、`setOn`、`execOn` 在源码中已被**注释掉**。

### 2.4 Kernel Properties（`kernel.properties.*`）

| Request | Kind | 说明 |
|---------|------|------|
| `kernel.properties.fetchStatus` | GET | 分页查询属性状态（需 int 参数：起始行号）|
| `kernel.properties.reloadStatus` | GET | 强制重载属性状态 |
| `kernel.properties.propKindTags` | GET | 属性类型枚举 |
| `kernel.properties.propStatusTags` | GET | 验证状态枚举 |
| `kernel.properties.alarmsTags` | GET | Alarm 类型枚举 |

### 2.5 EVA General（`plugins.eva.general.*`）

| Request | Kind | 测试结果 |
|---------|------|---------|
| `plugins.eva.general.compute` | EXEC | 运行 EVA 分析 |
| `plugins.eva.general.abort` | GET | 终止 EVA 分析 |
| `plugins.eva.general.getCallers` | GET | ERROR(需 decl marker) |
| `plugins.eva.general.getCallees` | GET | ERROR(需 marker) |
| `plugins.eva.general.getDeadCode` | GET | ERROR(需 decl marker) |
| `plugins.eva.general.taintedLvalues` | GET | ERROR(需 decl marker) |
| `plugins.eva.general.getStates` | GET | 获取域状态 |
| `plugins.eva.general.getComputationState` | GET | EVA 计算状态 |
| `plugins.eva.general.getProgramStats` | GET | 分析统计 |
| `plugins.eva.general.fetchFunctions` | GET | 分页：函数 + EVA 分析状态 |
| `plugins.eva.general.fetchProperties` | GET | 分页：属性 + 优先级 + 污点 |
| `plugins.eva.general.fetchFunctionStats` | GET | 分页：函数统计（覆盖率、alarm 数）|

### 2.6 EVA Values（`plugins.eva.values.*`）

| Request | Kind | 测试结果 |
|---------|------|---------|
| `plugins.eva.values.getValues` | GET | ERROR(需 `{target, callstack}`) |
| `plugins.eva.values.getCallstacks` | GET | 获取调用栈 |
| `plugins.eva.values.getCallstackInfo` | GET | 调用栈详情 |
| `plugins.eva.values.getStmtInfo` | GET | ERROR(需 stmt marker) |
| `plugins.eva.values.getProbeInfo` | GET | ERROR(需 marker) |

### 2.7 WP Main（`plugins.wp.*`）

| Request | Kind | 测试结果 |
|---------|------|---------|
| `plugins.wp.startProofs` | EXEC | 生成目标并运行证明器 |
| `plugins.wp.generateRTEGuards` | EXEC | 生成 RTE 断言 |
| `plugins.wp.getScheduledTasks` | GET | DATA: `{todo:0, procs:24, done:0, active:0}` |
| `plugins.wp.getProvers` | GET | DATA: `["Alt-Ergo:2.6.2"]` |
| `plugins.wp.setProvers` | SET | 设置证明器 |
| `plugins.wp.setTimeout` | SET | 设置超时 |
| `plugins.wp.fetchGoals` | GET | 分页查询证明目标 |
| `plugins.wp.reloadGoals` | GET | 强制重载目标 |
| `plugins.wp.cancelProofTasks` | SET | 取消证明任务 |

### 2.8 Callgraph（`plugins.callgraph.*`）

| Request | Kind | 测试结果 |
|---------|------|---------|
| `plugins.callgraph.compute` | EXEC | DATA: `null` |
| `plugins.callgraph.getCallgraph` | GET | 获取调用图数据 |
| `plugins.callgraph.getIsComputed` | GET | 是否已计算 |

### 2.9 其他插件

| Request | Kind | 说明 |
|---------|------|------|
| `plugins.studia.studia.getReadsLval` | GET | 读取某左值的语句列表 |
| `plugins.studia.studia.getWritesLval` | GET | 写入某左值的语句列表 |
| `plugins.impact.impact.impactStatement` | GET | 语句影响分析 |
| `plugins.region.compute` | EXEC | 计算 region |
| `plugins.region.regions` | GET | 获取 region 数据 |
| `plugins.dive.*` | 多种 | 数据流探索 |

---

## 3. v2.2 Tool → 实际 Request 映射

### 3.1 可直接实现的 Tool（Phase 1）

| v2.2 Tool | v2.2 假设的 Request | 实际可用 Request | 差异 |
|-----------|-------------------|-----------------|------|
| `load_project` | `kernel.ast.getFunctions` + `metrics.getMetrics` | `kernel.ast.setFiles` → `kernel.ast.compute` → `kernel.ast.fetchFunctions` | `metrics.getMetrics` 不存在 |
| `get_callgraph` | `callgraph.getGraph` | `plugins.callgraph.compute` → `plugins.callgraph.getCallgraph` | 名称不同 |
| `get_function_info` | `kernel.ast.getFunctionInfo` | `kernel.ast.fetchFunctions` + `kernel.ast.printDeclaration` | 需组合多个请求 |
| `run_eva` | `eva.setParams` + `eva.compute` | `kernel.parameters.set*` + `plugins.eva.general.compute` | EVA 参数通过全局参数系统设置 |
| `get_eva_alarms` | `eva.getAlarms` | `kernel.properties.fetchStatus`（过滤 alarm 类型） + `plugins.eva.general.fetchProperties` | 无专用 alarm 查询，用 properties |
| `get_eva_value` | `eva.getValues` | `plugins.eva.values.getValues` | 名称和参数格式不同 |
| `run_wp` | `wp.setParams` + `wp.compute` | `plugins.wp.setProvers` + `plugins.wp.setTimeout` + `plugins.wp.startProofs` | 参数分开设置 |
| `get_wp_goals` | `wp.getGoals` | `plugins.wp.fetchGoals`（分页） | 分页 API |
| `get_current_annotations` | `kernel.properties.getStatus` | `kernel.properties.fetchStatus`（分页） | 分页 API |
| `find_callers` | `callgraph.getCallers` | `plugins.eva.general.getCallers`（需 EVA）或从 callgraph 数据解析 | 需 EVA 分析结果 |
| `lookup_symbol` | `kernel.ast.getDecl` | `kernel.ast.fetchFunctions` / `kernel.ast.fetchGlobals` / `kernel.ast.getMarkerAt` | 无直接 getDecl |
| `get_verification_status` | 多个请求 | `kernel.properties.fetchStatus` + `plugins.eva.general.getComputationState` + `plugins.wp.getScheduledTasks` | 可组合 |

### 3.2 需要 OCaml 插件的 Tool（Phase 3）

| v2.2 Tool | 原因 |
|-----------|------|
| `inject_acsl` | 无任何 ACSL 注入 request |
| `remove_acsl` | 无任何 ACSL 删除 request |
| `find_memory_ops` | 无 CIL 遍历 request（需 Visitor API）|
| `get_cfg` | 无 CFG 导出 request |
| `get_data_deps` | `from` 插件未注册 server request |

### 3.3 纯 Rust 端实现的 Tool

| v2.2 Tool | 实现方式 |
|-----------|---------|
| `suggest_verification_plan` | Rust 端策略逻辑，基于其他 tool 结果 |
| `trace_call_chain` | Rust BFS 遍历 callgraph 数据 |
| `investigate_alarm` | Rust 组合多个 GET 查询 |

---

## 4. 关键发现

### 4.1 分页 API（Fetch 模式）

Frama-C Server 的 Array API 使用分页模式：
- `fetchX` 需要 `int` 参数（**batch capacity**：最大返回条目数，非起始行号）
  - 验证依据：`server/states.ml:302` `capacity = n`，每条目消耗 1 capacity
  - Ivette 使用 batch=20000（`states.ts:433`）
- 返回 `{ reload, updated, removed, pending }`
  - `updated`：本批次条目数组
  - `pending`：剩余未返回条目数（0 = 全部取完）
- 需要循环调用直到 `pending == 0` 才能获取完整数据
- 每次调用发送相同的 batch capacity，不需要递增偏移量
- `reloadX` 强制服务端重新计算数据

这与 v2.2 假设的"一次返回全部"不同，Rust 端需要实现分页循环。

### 4.2 Marker 系统

Frama-C Server 大量使用 **marker**（如 `#F990`）作为 AST 节点的引用：
- 函数、语句、表达式都用 marker 标识
- 大多数查询的输入是 marker，不是函数名
- 需要先通过 `fetchFunctions` 获取函数 marker，再用 marker 查询其他信息

v2.2 的 Tool 参数大多用 `function: String`（函数名），实际需要先做 name → marker 解析。

### 4.3 EVA 参数设置

EVA 分析参数不通过 `eva.setParams` 设置（此 request 不存在），而是通过全局参数系统 `kernel.parameters.set<ParamName>` 逐个设置。需要调研：
- EVA precision 对应的参数名
- EVA main function 对应的参数名
- EVA slevel 对应的参数名

### 4.4 不存在的 Request（v2.2 假设有误）

| v2.2 假设 | 实际情况 |
|-----------|---------|
| `kernel.ast.getFunctionInfo` | 不存在，需组合 `fetchFunctions` + `printDeclaration` |
| `metrics.getMetrics` / `metrics.getFunctionMetrics` | 不存在，metrics 插件未注册 server request |
| `eva.getAlarms` | 不存在，用 `kernel.properties.fetchStatus` 过滤 |
| `eva.setParams` | 不存在，用 `kernel.parameters.set*` |
| `eva.getSummary` | 不存在，用 `plugins.eva.general.getProgramStats` |
| `callgraph.getCallers` / `callgraph.getCallees` | callgraph 插件无此 request，`getCallers`/`getCallees` 在 EVA 插件下 |
| `from.getFunctionDeps` | `from` 插件未注册 server request |
| `vp.injectAnnotation` / `vp.removeAnnotation` | 不存在，需 OCaml 插件 |
| `vp.getAnnotations` | 不存在 |
| `vp.suggestPlan` | 不存在 |
| `vp.getMemoryOps` | 不存在 |
| `kernel.ast.getCFG` | 不存在 |
| `kernel.ast.getDecl` | 不存在 |

---

## 5. 推荐实施策略

### Phase 1（核心通信 + 基础 Tool）

**基础设施：**
1. 实现 Frama-C Server Unix Socket 客户端（正确的协议格式）
2. 实现分页 fetch 循环（`fetchX` → 循环直到无更多数据）
3. 实现 marker 解析（函数名 → marker 映射缓存）
4. 实现 EXEC + POLL 异步等待

**可交付的 Tool（8 个）：**
- `load_project` — `setFiles` + `compute` + `fetchFunctions`
- `get_callgraph` — `callgraph.compute` + `callgraph.getCallgraph`
- `get_function_info` — `fetchFunctions` + `printDeclaration`
- `run_eva` — `kernel.parameters.set*` + `eva.general.compute`
- `get_eva_alarms` — `kernel.properties.fetchStatus` 过滤
- `get_eva_value` — `plugins.eva.values.getValues`
- `find_callers` — `plugins.eva.general.getCallers`
- `get_verification_status` — 组合多个 GET

### Phase 2（完整 Tool 集）

- `run_wp` + `get_wp_goals` — WP API 完善
- `get_current_annotations` — properties 过滤
- `lookup_symbol` — fetchFunctions/fetchGlobals/getMarkerAt
- `trace_call_chain` — Rust BFS
- `investigate_alarm` — Rust 组合查询
- `suggest_verification_plan` — Rust 策略逻辑

### Phase 3（OCaml 插件）

需要开发 OCaml 插件注册新的 server request：
- `inject_acsl` / `remove_acsl` — ACSL 注解操作
- `find_memory_ops` — CIL Visitor 遍历
- `get_cfg` — CFG 导出
- `get_data_deps` — From 插件封装

---

## 6. 参考

- Frama-C Server 源码：`~/.opam/frama/.opam-switch/sources/frama-c.31.0/src/plugins/server/`
- Ivette 客户端实现：同目录下 `ivette/src/frama-c/client_socket.ts`
- 协议文档：`src/plugins/server/doc/server_socket.md`
