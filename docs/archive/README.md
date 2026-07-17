# 历史文档归档

> **这里的文档记录设计沿革，不反映现状。** 现状请看 [README.md](../../README.md)（使用）、[docs/architecture.md](../architecture.md)（架构）、[CLAUDE.md](../../CLAUDE.md)（开发）。
>
> 归档文档内的相对链接可能已失效（移动所致），未做修补——它们按原样保存。

这些文档写于 2025-02 ~ 2026-02 的选型与首版实现期。之后项目发生了三处会让它们**直接误导**的变化：

| 变化 | 归档文档里的旧说法 | 现状 |
|---|---|---|
| **传输层** | Rust + **ZMQ**（v2.2 设计） | Unix Socket（ZMQ 环境不可用，已废弃） |
| **启动方式** | 手工 `frama-c … -server-socket /tmp/x.sock`，再 `--socket /tmp/x.sock` 连接 | MCP server **惰性自行 spawn** frama-c（`--frama-c`）；`--socket` 已废弃且被忽略 |
| **能力范围** | 分 Phase 推进，8 → 15 / 20 个工具，OCaml 插件是「Phase 3 可选项」 | **43 个工具**；OCaml 插件 `ast-utils` **已落地且必需**（实际演进到当初评估里的「方案 C」）；另有沙箱与自下而上全程序编排 |

## 内容

| 文件 | 是什么 |
|---|---|
| `frama-c-mcp-rust-zmq-设计-v2.2.md` | ZMQ 时代的完整设计：Tool 定义、参数与类型系统、实施路线图。传输层已废弃；工具定义与类型系统是当前实现的祖本 |
| `architecture-discussion-full.md` | 五种候选方案的完整分析（纯 OCaml 插件 / CIL 导出 / Rust 嵌 OCaml / Frama-C 调 Rust / Rust + Server） |
| `frama-c-mcp-设计讨论记录.md` | 设计演进过程记录 |
| `ocaml-rust-ffi-experiments.md` | OCaml↔Rust FFI 实验报告（对应仓库根的 `experiments/`，同为历史产物）。结论：1A/1B/1C/3A/3B 通过，3C（Rust 嵌入 Frama-C）因 dune 虚拟模块阻塞 → 促成改用 Server 协议方案 |
| `design/rust-mcp-server/architecture.md` | 首版架构设计（2026-02-19） |
| `design/rust-mcp-server/detailed-design.md` | 首版细化设计 |
| `test-reports/` | 首版的测试报告与 code review 修复计划 |

## 仍然有效、未归档的文档

- [`../reference/frama-c-server-protocol-guide.md`](../reference/frama-c-server-protocol-guide.md) — Frama-C server 协议参考（协议未变，仍准确）
- [`../research/frama-c-server-api-research.md`](../research/frama-c-server-api-research.md) — Frama-C server API 调研
- [`../workflow.md`](../workflow.md) — 开发流程规范
