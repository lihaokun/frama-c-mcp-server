# CLAUDE.md

## 项目概述

**frama-c-mcp-server**：让 AI agent 通过 [MCP](https://modelcontextprotocol.io/) 使用 [Frama-C](https://frama-c.com/) 做 C 程序形式验证——EVA 抽象解释、WP 演绎证明、ACSL 注解推理、沙箱化 CEGIS 迭代。

仓库有**两个必需的部分**：

| 部分 | 语言 | 作用 |
|---|---|---|
| `src/` — MCP server | Rust (rmcp 1.x) | 上游讲 MCP（stdio JSON-RPC），下游讲 Frama-C server 协议（Unix socket）。**43 个工具**。**惰性拉起** Frama-C 进程。 |
| `ast-utils/` — Frama-C 插件 | OCaml (dune) | 注册 MCP 依赖的 8 个自定义 server request（AST 访问、依赖提取、ACSL 注入、WP 配置、VC 详情等）。**没它大部分工具会失败。** |

> 这正是早期架构文档里预留的「Phase 3：按需加 OCaml 插件扩展 Frama-C Server 能力（方案 C）」——现已落地为 `ast-utils`。

## 开发环境

- Frama-C **31.0 (Gallium)**，OCaml >= 4.14.2，opam，dune >= 3.0
- Rust edition 2021，rmcp 1.x（官方 MCP SDK）
- WP 证明器：alt-ergo（随 Frama-C 装），可选 z3 / cvc5

## 构建

```bash
# 插件必须装到与 frama-c 同一个 opam switch
cd ast-utils && dune build && dune install && cd ..
cargo build --release
```

**改插件后必须 `dune clean` 再 build + install**——增量构建可能不重新链接 `.cmxs`，导致装的还是旧版（典型症状：改了代码但行为没变）。

## 运行

```bash
./target/release/frama-c-mcp-server --frama-c /path/to/frama-c
```

**不需要手工启动 Frama-C server**：第一次 `reload_project` 时 MCP server 自己 spawn frama-c 并连上（lazy spawn）。`--socket` 已废弃并被忽略（socket 按进程自动生成）。`--max-sandboxes`（默认 32）限制并发沙箱进程数。

## 架构

```
AI Agent <── MCP (stdio) ──> Rust Server ──┬── Unix Socket ──> Frama-C (main)    ── EVA / WP / CIL
                              43 tools      │                   + ast-utils
                              session state └── Unix Socket ──> Frama-C (sandbox) ── 隔离的 CEGIS
                                                                + ast-utils
```

**沙箱模型**：`create_sandbox` 把目标函数**连同全部类型/被调用者/全局依赖**提取成独立的临时 C 文件，在其上起**另一个** Frama-C 实例。agent 在沙箱里反复试 ACSL + 跑 WP，不污染主工程；验证通过的注解再显式提取、合并回主工程。

工具面与工作流详见 [README.md](README.md)，架构详见 [docs/architecture.md](docs/architecture.md)。

## 关键技术约束

- **Frama-C server 协议**：自定义二进制协议（非 JSON-RPC）。命令 `GET`/`SET`/`EXEC`/`POLL`/`SHUTDOWN`；分帧为 `S`+3 hex 或 `L`+7 hex 长度前缀。`SET`/`EXEC` 是队列式**异步**——必须用 `POLL` 循环拿中间 SIGNAL 和最终结果。与 Ivette（官方 GUI）同协议。
- **AST 重载**：必须 `setFiles([])` → `setFiles(files)` → `compute` 三步。直接 `setFiles(同值)` 是空操作（Frama-C 状态依赖系统所致）。与 Ivette 的 `reparseFiles()` 一致。
- **fetch API 是增量的**：`fetchFunctions` 只在首次返回全量，之后只返回变更。每次需要全量前先 `reloadFunctions` 重置 cursor。
- **WP 内存模型**：本 server 把 WP 配为 `Typed+nocast`——有 cast 时相关 VC **安全失败**，而不是被静默放过。
- **无契约 callee 默认 `assigns \nothing`，这是 unsound 的**（WP 手册 §2.1 明说 "best effort, unsafe"）。因此沙箱提取会给缺显式 `assigns` 的 callee 生成**空体 stub**，而非裸声明。
- **`serde_json` 开了 `preserve_order`**：Frama-C / 插件发来的 JSON object key 序（= 源码序）必须在解析后保住；否则按字母序遍历会把 `then_body`/`else_body` 之类的顺序弄反。
- **WP 标记**：`startProofs` 要的是 PVDecl 标记（`#v<vid>`）而非 AST.Decl（`#F<vid>`），且须先 `printDeclaration` 把标记注册进 server 表。

## 测试

```bash
cargo test --lib                                              # 纯单测，不需要 frama-c
export PATH="$(opam var bin):$PATH"                           # 下面两个需要 frama-c + 已装 ast-utils
cargo test --test integration_test -- --test-threads=1
cargo test --test mcp_stdio_test --release -- --test-threads=1
```

CI（`.github/workflows/ci.yml`）两条线：纯 Rust 快跑；完整线装 Frama-C 31.0 + 证明器 + ast-utils 插件后跑集成与 MCP stdio E2E。

## 开发流程

严格遵守 [@docs/workflow.md](docs/workflow.md) 中的全部规则（先规划后动手、单步推进、最小变更、设计先于代码）。

## 文档

- [README.md](README.md) — 使用说明、43 个工具、验证工作流
- [docs/architecture.md](docs/architecture.md) — 当前架构
- [docs/reference/frama-c-server-protocol-guide.md](docs/reference/frama-c-server-protocol-guide.md) — Frama-C server 协议参考
- [docs/archive/](docs/archive/) — **历史设计记录**（早期方案选型、ZMQ 时代设计、FFI 实验、旧测试报告）。**已被现状取代，勿作现状参考。**
