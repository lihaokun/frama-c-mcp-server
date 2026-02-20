# CLAUDE.md

## 项目概述

Frama-C MCP Server：Rust MCP Server 通过 Unix Socket 与 Frama-C Server 进程通信，让 AI 验证 agent 与 Frama-C 的静态分析和形式验证能力交互。

当前处于 **Phase 1 完成**——8 个 MCP 工具已实现并通过测试。

## 开发环境

- OCaml 4.14.2, opam switch `frama`
- Frama-C 31.0 (Gallium)
- dune 3.20.2
- Rust nightly 1.95.0, rmcp 0.16（官方 MCP SDK）

## 项目结构

```
src/                         # Rust MCP server（Phase 1 已实现）
experiments/                 # OCaml↔Rust FFI 实验（已完成）
├── standalone-ffi/          # Exp 1C: OCaml↔Rust ctypes.foreign
├── frama-c-ffi/             # Exp 1A+1B: Frama-C 插件调 Rust + 回调
└── rust-calls-ocaml/        # Approach 3 实验（Rust 嵌入 OCaml）
test/                        # 测试 C 文件
docs/
├── architecture.md          # 架构决策（选定方案 A）
├── frama-c-mcp-rust-zmq-设计-v2.2.md  # Tool 定义、类型系统（复用）
├── frama-c-mcp-设计讨论记录.md         # 设计演进记录
├── architecture-discussion-full.md
└── workflow.md              # 开发流程规范
```

## 开发流程

严格遵守 @docs/workflow.md 中的全部规则。

## 架构决策

已选方案：**Rust MCP Server + Frama-C Server (Unix Socket)**（方案 A）。

```
Agent <-- MCP (stdio) --> Rust (rmcp) <-- Unix Socket --> Frama-C Server
```

- Rust (rmcp 0.16) 处理 MCP 协议（JSON-RPC over stdio）
- Frama-C Server 处理验证工作（CIL AST、EVA、WP），通过 `-server-socket` 启动
- Frama-C Server 使用自定义协议（非 JSON-RPC）：GET/SET/EXEC/POLL/SHUTDOWN 命令，自定义分帧
- Phase 3 可按需添加 OCaml 插件扩展 Frama-C Server 能力（演进到方案 C）

详见 [docs/architecture.md](docs/architecture.md)，Tool 定义详见 [docs/frama-c-mcp-rust-zmq-设计-v2.2.md](docs/frama-c-mcp-rust-zmq-设计-v2.2.md)。

## 关键技术约束

- Frama-C Server 协议：`S`+3 hex / `L`+7 hex 长度前缀分帧，非 JSON-RPC
- EXEC 是异步的，需通过 POLL 获取中间 SIGNAL 和最终结果
- 当前环境：`-server-socket` 和 `-server-batch` 可用，ZMQ 不可用
- OCaml MCP 生态不可用：`ocaml-mcp` 需 OCaml 5.0+，`jsonrpc` 与 yojson 3.0.0 冲突
