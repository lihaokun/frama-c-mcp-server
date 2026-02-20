# frama-c-mcp-server

An [MCP](https://modelcontextprotocol.io/) server for [Frama-C](https://frama-c.com/), enabling AI agents to perform C program formal verification — abstract interpretation (EVA), deductive proof (WP), and ACSL annotation reasoning.

面向 [Frama-C](https://frama-c.com/) 的 [MCP](https://modelcontextprotocol.io/) 服务器，让 AI 智能体能够执行 C 程序形式验证——抽象解释 (EVA)、演绎证明 (WP) 和 ACSL 注解推理。

## Architecture / 架构

```
AI Agent <── MCP (stdio) ──> Rust Server <── Unix Socket ──> Frama-C Server
                              rmcp 0.16                      -server-socket
                              15 tools                       EVA / WP / CIL
                              session state                  200+ server requests
```

The Rust process bridges two protocols:
- **Upstream (MCP)**: JSON-RPC over stdio, handled by [rmcp](https://github.com/anthropics/rust-sdk) — the official Rust MCP SDK
- **Downstream (Frama-C)**: Custom binary protocol over Unix Socket (same protocol [Ivette](https://frama-c.com/html/ivette.html) uses)

Rust 进程桥接两个协议：
- **上游 (MCP)**：JSON-RPC over stdio，由 [rmcp](https://github.com/anthropics/rust-sdk)（官方 Rust MCP SDK）处理
- **下游 (Frama-C)**：自定义二进制协议 over Unix Socket（与 [Ivette](https://frama-c.com/html/ivette.html) 使用相同协议）

## MCP Tools / MCP 工具

### Core Analysis / 核心分析

| Tool | Description | 说明 |
|------|-------------|------|
| `reload_project` | Load/reload C source files. Reparses AST and refreshes all cached state. | 加载/重载 C 源文件。重新解析 AST 并刷新所有缓存状态。 |
| `run_eva` | Run EVA abstract interpretation. Finds potential runtime errors. | 运行 EVA 抽象解释。发现潜在运行时错误（除零、缓冲区溢出等）。 |
| `run_wp` | Run WP deductive verification on specified functions. | 对指定函数运行 WP 演绎证明，验证 ACSL 契约正确性。 |
| `get_verification_status` | Get comprehensive status: property counts by category, EVA/WP state. | 获取综合验证状态：属性按类别统计、EVA/WP 分析状态。 |

### Querying Results / 查询结果

| Tool | Description | 说明 |
|------|-------------|------|
| `get_eva_alarms` | List EVA alarms, filterable by function/kind/status. | 列出 EVA 报警，可按函数/类型/状态过滤。 |
| `get_eva_value` | Query EVA value range at a program point. | 查询程序点的 EVA 值域。 |
| `get_wp_goals` | List WP proof goals with status (VALID/NORESULT/UNKNOWN). | 列出 WP 证明目标及状态。 |
| `get_current_annotations` | List ACSL annotations on a function with verification status. | 列出函数的 ACSL 注解及其验证状态。 |

### Navigation / 导航

| Tool | Description | 说明 |
|------|-------------|------|
| `get_function_info` | Source location, signature, and annotated declaration. | 函数源码位置、签名和带注解的声明。 |
| `get_callgraph` | Compute and return the function call graph. | 计算并返回函数调用图。 |
| `find_callers` | Find all callers of a function (requires EVA). | 查找函数的所有调用者（需先运行 EVA）。 |
| `trace_call_chain` | Multi-level call chain traversal (callers or callees). | 多层调用链追踪（向上/向下）。 |
| `lookup_symbol` | Look up a function or global variable by name. | 按名称查找函数或全局变量。 |

### Compound / 组合工具

| Tool | Description | 说明 |
|------|-------------|------|
| `investigate_alarm` | Deep investigation: value ranges, callers, annotations in one call. | 深度调查报警：值域、调用者、注解一次返回。 |
| `suggest_verification_plan` | Analyze current state and suggest next actions. | 分析当前状态并建议下一步操作。 |

## Quick Start / 快速上手

### Prerequisites / 前置条件

- [Frama-C](https://frama-c.com/) >= 31.0 (Gallium)
- Rust (edition 2021)
- OCaml >= 4.14, opam

### Build / 构建

```bash
cargo build --release
```

### Usage / 使用

```bash
# 1. Start a Frama-C server / 启动 Frama-C 服务器
frama-c your_program.c -server-socket /tmp/frama-c.sock

# 2. Start the MCP server / 启动 MCP 服务器
./target/release/frama-c-mcp-server --socket /tmp/frama-c.sock
```

### Claude Desktop Configuration / Claude Desktop 配置

Add to / 添加到 `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "frama-c": {
      "command": "/path/to/frama-c-mcp-server",
      "args": ["--socket", "/tmp/frama-c.sock"]
    }
  }
}
```

## Iterative Verification Workflow / 迭代验证工作流

The primary use case this server enables:

本服务器支持的核心使用场景：

```
1. Agent receives a C file              智能体接收 C 文件
2. reload_project                      → 加载到 Frama-C
3. run_eva                             → 发现潜在运行时错误
4. get_eva_alarms                      → 查看除零、越界等报警
5. investigate_alarm                   → 深入分析报警根因
6. Agent writes ACSL annotations         智能体编写 ACSL 注解消除报警
7. reload_project                      → 重新解析修改后的文件
8. run_wp                              → 证明注解正确
9. get_wp_goals                        → 确认所有目标 VALID
```

This loop — EVA finds problems, agent injects ACSL, WP proves correctness — is tested end-to-end in `test_iterative_workflow`.

这个循环——EVA 发现问题、智能体注入 ACSL、WP 证明正确性——已在 `test_iterative_workflow` 中端到端验证。

## Testing / 测试

```bash
# Unit tests (no server needed) / 单元测试（无需服务器）
cargo test --lib

# Offline integration tests / 离线集成测试
cargo test --test integration_test -- test_function_not_found test_state_invalidation \
  test_update_functions_empty_clears_cache test_connect_bad_socket

# Live integration tests (each needs its own Frama-C server)
# 在线集成测试（每个测试需要独立的 Frama-C 服务器实例）
frama-c test/test_abs.c -server-socket /tmp/frama-c-test.sock
cargo test test_full_workflow -- --nocapture

frama-c test/test_comprehensive.c -server-socket /tmp/frama-c-test-comp.sock
cargo test test_comprehensive -- --nocapture

frama-c test/test_iterative_raw.c -server-socket /tmp/frama-c-test-iter.sock
cargo test test_iterative -- --nocapture
```

| Suite | Tests | Description / 说明 |
|-------|-------|---------------------|
| Unit tests | 28 | Codec, state management, callgraph queries / 编解码、状态管理、调用图查询 |
| Offline integration | 4 | State invalidation, error handling / 状态失效、错误处理 |
| `test_full_workflow` | 14 steps | Phase 1: basic 8-tool workflow / 基础 8 工具工作流 |
| `test_phase2_workflow` | 13 steps | Phase 2: globals, callgraph, multi-function WP / 全局变量、调用图、多函数 WP |
| `test_comprehensive` | 19 steps | All 15 tools against Safe Buffer Module / 全部 15 工具综合测试 |
| `test_iterative_workflow` | 5 phases | Raw C → EVA → inject ACSL → reload → WP / 裸 C → EVA → 注入 ACSL → 重载 → WP |

## Project Structure / 项目结构

```
src/
├── main.rs                 # CLI entry point / 入口
├── lib.rs                  # Library root / 库根
├── state.rs                # Session state / 会话状态（函数、全局变量、调用图缓存）
├── error.rs                # Error types / 错误类型
├── frama_c/
│   ├── client.rs           # Frama-C client (GET/SET/EXEC/POLL)
│   ├── codec.rs            # Wire protocol codec / 协议编解码（S/L 长度前缀分帧）
│   └── transport.rs        # Unix socket transport / Unix Socket 传输层
└── mcp/
    ├── server.rs           # 15 MCP tool implementations / 15 个 MCP 工具实现
    └── types.rs            # Tool parameter types / 工具参数类型

test/                       # C files for integration tests / 集成测试用 C 文件
tests/integration_test.rs   # Integration tests / 集成测试
docs/                       # Architecture, design, test reports / 架构、设计、测试报告
experiments/                # OCaml↔Rust FFI experiments (completed) / FFI 实验（已完成）
```

## Technical Notes / 技术要点

**Frama-C Server Protocol / Frama-C 服务器协议**: Custom binary protocol (not JSON-RPC). Commands: `GET`/`SET`/`EXEC`/`POLL`/`SHUTDOWN`. Framing: `S`+3 hex or `L`+7 hex length prefix. `SET` and `EXEC` are queued — must use `POLL` loop. Same protocol Ivette (Frama-C's official GUI) uses. / 自定义二进制协议（非 JSON-RPC）。`SET` 和 `EXEC` 为队列式——需 `POLL` 轮询。与 Ivette（官方 GUI）使用相同协议。

**AST Reload / AST 重载**: File reparse requires `setFiles([])` → `setFiles(files)` → `compute`. Direct `setFiles(same_value)` is a no-op due to Frama-C's state dependency system. This matches Ivette's `reparseFiles()` implementation. / 文件重新解析需要 `setFiles([])` → `setFiles(files)` → `compute` 三步。直接 `setFiles(同值)` 是空操作。这与 Ivette 的 `reparseFiles()` 实现一致。

**WP Markers / WP 标记**: `startProofs` requires PVDecl markers (`#v<vid>`), not AST.Decl (`#F<vid>`). Must call `printDeclaration` first to register markers in the server's table. / `startProofs` 需要 PVDecl 标记（`#v<vid>`），非 AST.Decl（`#F<vid>`）。必须先调用 `printDeclaration` 注册标记。

## License

MIT
