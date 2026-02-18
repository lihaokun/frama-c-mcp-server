# CLAUDE.md

## 项目概述

Frama-C MCP Server：一个纯 OCaml 的 Frama-C 插件，通过 MCP 协议（JSON-RPC over stdio）让 AI 验证 agent 与 Frama-C 的静态分析和形式验证能力交互。

当前处于 **早期开发阶段（POC）**。

## 开发环境

- OCaml 4.14.2, opam switch `frama`
- Frama-C 31.0 (Gallium)
- dune 3.20.2
- Rust nightly 1.95.0（仅用于 FFI 实验）

## 项目结构

```
src/                         # MCP 插件主代码（纯 OCaml，待实现）
experiments/
├── standalone-ffi/          # Exp 1C: OCaml↔Rust ctypes.foreign
├── frama-c-ffi/             # Exp 1A+1B: Frama-C 插件调 Rust + 回调
└── rust-calls-ocaml/        # Approach 3 实验
    ├── exp3a-simple/        # Rust 嵌入 OCaml（已验证）
    ├── exp3b-json/          # Rust↔OCaml JSON 交换（已验证）
    └── exp3c-frama-c/       # Rust 嵌入 Frama-C（阻塞中）
test/                        # 测试 C 文件
docs/
├── architecture.md          # 架构决策
├── architecture-discussion-full.md
├── ocaml-rust-ffi-experiments.md
└── workflow.md              # 开发流程规范
```

## 开发流程

严格遵守 @docs/workflow.md 中的全部规则。

## 架构决策

已选方案：**纯 OCaml 插件**（Approach 1）。通过 `frama-c -load-module` 加载，直接调用 Frama-C API。

如 OCaml 的 async/networking 不够用，fallback 到 Approach 4（Frama-C 插件调 Rust 库）。FFI 可行性已通过 `experiments/` 验证。

详见 [docs/architecture.md](docs/architecture.md)。
