# Frama-C MCP Server 架构

## 当前架构：Rust MCP Server + Frama-C Server (Unix Socket) + ast-utils 插件

```
┌──────────────────────────────────────────────────────────────┐
│                       LLM Agent (Claude)                      │
└───────────────────────────┬──────────────────────────────────┘
                            │ MCP (JSON-RPC 2.0 / stdio)
                            ▼
┌──────────────────────────────────────────────────────────────┐
│                 frama-c-mcp-server  (Rust)                    │
│                                                               │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────────────┐  │
│  │ MCP Layer   │  │ Tool Router  │  │ State Manager       │  │
│  │ (rmcp 1.x)  │  │ 43 tools     │  │ 会话状态 / 结论 /   │  │
│  │ stdio       │  │              │  │ 工程状态 / 锁       │  │
│  └──────┬──────┘  └──────┬───────┘  └──────────┬──────────┘  │
│         │                │                      │             │
│  ┌──────▼────────────────▼──────────────────────▼──────────┐  │
│  │        Frama-C Client（自定义协议 GET/SET/EXEC/POLL）    │  │
│  │        + 进程管理：惰性 spawn / 生命周期 / 回收          │  │
│  └──────┬──────────────────────────────┬────────────────────┘ │
└─────────┼──────────────────────────────┼──────────────────────┘
          │ Unix Socket                  │ Unix Socket（每沙箱一条）
          ▼                              ▼
┌───────────────────────────┐  ┌───────────────────────────────┐
│  Frama-C 进程（main）      │  │  Frama-C 进程（sandbox ×N）    │
│  ┌────────┐ ┌────┐ ┌────┐ │  │  独立临时 C 文件               │
│  │ Kernel │ │EVA │ │ WP │ │  │  = 目标函数 + 全部依赖         │
│  │AST/Prop│ │    │ │    │ │  │  用于隔离的 CEGIS 迭代         │
│  └────────┘ └────┘ └────┘ │  │                               │
│  ┌──────────────────────┐ │  │  ┌──────────────────────┐     │
│  │  ast-utils 插件       │ │  │  │  ast-utils 插件       │     │
│  └──────────────────────┘ │  │  └──────────────────────┘     │
└───────────────────────────┘  └───────────────────────────────┘

启动：frama-c-mcp-server --frama-c /path/to/frama-c
      （Frama-C 进程由 MCP server 在首次 reload_project 时自行拉起）
```

## 组成部分

### 1. Rust MCP Server（`src/`）

| 模块 | 职责 |
|---|---|
| `mcp/server.rs` | 43 个工具实现（工程/分析/WP/注解/沙箱/导航/验证状态）|
| `mcp/types.rs`, `mcp/param_compat.rs` | 工具参数类型 + 兼容层 |
| `frama_c/client.rs` | Frama-C 客户端：GET/SET/EXEC/POLL 语义、进程惰性拉起与回收 |
| `frama_c/codec.rs`, `frama_c/transport.rs` | 协议编解码（`S`+3 hex / `L`+7 hex 分帧）+ Unix socket 传输 |
| `state.rs` | 会话状态、每函数验证结论、工程级编排状态、工程锁 |
| `topo.rs` | Tarjan SCC + Kahn 分层，产出自下而上验证序 |
| `linear_invariant.rs` | 线性循环不变式合成 CLI 桥接 |

### 2. ast-utils Frama-C 插件（`ast-utils/`，**必需**）

Frama-C 内置 server 注册了 200+ request，但不足以支撑注解驱动的验证循环。`ast-utils` 补上 8 个：

| Request | 用途 |
|---|---|
| `getFunctionAst` | 函数的结构化 AST（语句 + sid），供 agent 定位注解插入点 |
| `extractFunctionWithDeps` | 递归收集类型/被调用者/全局依赖，提取成独立可编译 C 文件（沙箱基础）|
| `execAddAnnotation` | 通过 Frama-C API 注入 ACSL（不改源文件文本）|
| `execExtractAnnotations` | 取出沙箱内新增的注解，供合并回主工程 |
| `getAcslValidation` | 注入前做 ACSL 语法/类型检查 |
| `execSetWpConfig` | 配置 WP（内存模型、证明器、超时）|
| `getVcDetails` | 取某个证明目标的完整验证条件 |
| `printSource` | 打印当前带注解的源码 |

**插件未安装 ⇒ 上述工具全部失败。** 必须装到与 `frama-c` 相同的 opam switch。

### 3. 沙箱模型

`create_sandbox` 把目标函数**连同全部依赖**提取成独立临时 C 文件，并在其上起**另一个** Frama-C 进程（而非在主工程内复制 AST）：

- agent 在沙箱里反复试 ACSL、跑 WP、读 VC 反例，主工程完全不受影响
- 沙箱失败/污染时 `reset_sandbox` 从原函数重建，保留 experiment id
- 验证通过后 `extract_annotations` → 注入主工程 → 主工程 `run_wp_main` 复核
- 命名空间 `experiment_id:function_name`，支持多沙箱并发（`--max-sandboxes` 默认 32）

**为什么用独立进程而非进程内复制**：在主工程内复制函数 AST 会撞上 Frama-C 状态依赖系统（AbortFatal），且复制体与原体的 WP VC 质量存在差异；独立进程给出干净、可丢弃、可并发的隔离边界。

### 4. 自下而上的全程序编排

`compute_topological_order`（Tarjan SCC + Kahn 分层）给出验证序：先 callee 后 caller，递归环打成 SCC 组。`get_ready_functions` 返回当前可验证的一批，配合 `store_function_conclusion` 记录进度，`lock_project` 在批量作业期间保护主工程不被重载。

## 设计决策

### 为什么是 Rust + Frama-C Server

评估过四个方案：

| 方案 | MCP 协议 | Frama-C 能力 | 工程难度 | 性能 |
|------|---------|-------------|---------|------|
| A: Rust + Frama-C Server | ★★★★★ | ★★★☆☆→★★★★★ | 中 | ms 级 |
| B: 纯 OCaml 插件 | ★★☆☆☆ | ★★★★★ | 中高 | ns 级 |
| C: 混合（A 的超集）| ★★★★★ | ★★★★★ | 高 | ms 级 |
| D: Rust + CLI 子进程 | ★★★★★ | ★☆☆☆☆ | 低 | 秒级 |

选 A 的核心原因：

1. **MCP 生态成熟度**：rmcp 是官方 Rust MCP SDK；OCaml 侧无可用 MCP SDK（`ocaml-mcp` 需 OCaml 5.0+，本项目环境是 4.14.2）
2. **Frama-C Server 已存在**：内置 Server 插件（Ivette GUI 的后端）支持 Unix Socket，已注册 200+ request，无需从零构建交互层
3. **异步能力**：EVA/WP 可能跑数分钟，Rust (tokio) 天然支持；OCaml 4.14 缺异步手段
4. **渐进增强**：先用内置 request 覆盖基础工具，需要时再写 OCaml 插件扩展

**第 4 点已经发生**：注解驱动的验证循环需要内置 request 给不了的能力，于是有了 `ast-utils`——即当初预留的「Phase 3 演进到方案 C」。**当前实际形态是方案 C（Rust server + 自研 OCaml 插件）**，而非纯 A。

### 决策沿革

| 日期 | 决策 | 状态 |
|------|------|------|
| 2025-02-17 | v2.2 设计：Rust + ZMQ | [已废弃传输层] ZMQ 不可用，改 Unix Socket；Tool 定义与类型系统保留 |
| 2025-02-18 | 纯 OCaml 插件（Approach 5）| [已废弃] MCP 生态不足、异步受限 |
| 2025-02-19 | Rust + Frama-C Server (Unix Socket) | 选定（方案 A）|
| 2025-02~ | 手工起 server + `--socket` 连接 | [已废弃] 改为 MCP server 惰性 spawn（`--frama-c`）|
| 2026 | 加入 `ast-utils` 插件 + 沙箱 + 自下而上编排 | **当前**（实际落到方案 C）|

## 关键技术细节

**Frama-C Server 协议**（非 JSON-RPC）：
- 命令：`GET(id,request,data)`, `SET(id,request,data)`, `EXEC(id,request,data)`, `POLL`, `SHUTDOWN`
- 回复：`DATA(id,data)`, `ERROR(id,msg)`, `SIGNAL(id)`, `REJECTED(id)`
- 传输：Unix Socket，自定义分帧（`S`+3 hex / `L`+7 hex 长度前缀）
- `SET`/`EXEC` 队列式异步——须 POLL 拿中间 SIGNAL 与最终结果

详见 [reference/frama-c-server-protocol-guide.md](reference/frama-c-server-protocol-guide.md)。

**AST 重载**：须 `setFiles([])` → `setFiles(files)` → `compute` 三步；直接 `setFiles(同值)` 是空操作（状态依赖系统所致）。同 Ivette 的 `reparseFiles()`。

**fetch API 是增量的**：`fetchFunctions` 只首次返回全量，之后只返回变更；需要全量前先 `reloadFunctions` 重置 cursor。

**WP 配置**：内存模型 `Typed+nocast`——有 cast 时 VC 安全失败而非静默放过。无契约 callee 默认 `assigns \nothing` 是 unsound 的（WP 手册 §2.1），故沙箱提取对缺显式 `assigns` 的 callee 生成空体 stub。

**JSON key 序**：`serde_json` 开 `preserve_order`，保住插件 emit 的源码序（否则字母序遍历会弄反 `then_body`/`else_body` 这类结构）。

## 历史文档

早期方案选型全文、ZMQ 时代设计（v2.2）、OCaml↔Rust FFI 实验、旧测试报告与旧细化设计均已移入 [archive/](archive/)——**记录设计沿革用，不反映现状**。
