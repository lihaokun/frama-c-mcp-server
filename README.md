# frama-c-mcp-server

An [MCP](https://modelcontextprotocol.io/) server for [Frama-C](https://frama-c.com/), enabling AI agents to perform C program formal verification — abstract interpretation (EVA), deductive proof (WP), ACSL annotation reasoning, and sandboxed CEGIS experimentation.

面向 [Frama-C](https://frama-c.com/) 的 [MCP](https://modelcontextprotocol.io/) 服务器，让 AI 智能体能够执行 C 程序形式验证——抽象解释 (EVA)、演绎证明 (WP)、ACSL 注解推理，以及沙箱化的 CEGIS 迭代实验。

## Architecture / 架构

```
AI Agent <── MCP (stdio) ──> Rust Server ──┬── Unix Socket ──> Frama-C (main)   ── EVA / WP / CIL
                              rmcp 0.16     │                   + ast-utils plugin
                              43 tools      │
                              session state └── Unix Socket ──> Frama-C (sandbox) ── isolated CEGIS
                                                                + ast-utils plugin
```

Two halves, both required / 两个必需的部分：

- **Rust MCP server** — bridges MCP (JSON-RPC over stdio, via [rmcp](https://github.com/modelcontextprotocol/rust-sdk)) to the Frama-C server protocol (custom binary protocol over Unix socket, the same one [Ivette](https://frama-c.com/html/ivette.html) uses). It **lazily spawns** Frama-C itself — you no longer start a Frama-C server by hand.
- **`ast-utils` Frama-C plugin** (OCaml, in `ast-utils/`) — registers the server requests the MCP relies on for AST access, dependency extraction, ACSL injection and WP config. **Without this plugin most tools will fail**, so it must be built and installed into the same opam switch as Frama-C.

- **Rust MCP server** — 桥接 MCP（stdio 上的 JSON-RPC，用 [rmcp](https://github.com/modelcontextprotocol/rust-sdk)）与 Frama-C server 协议（Unix socket 上的自定义二进制协议，与 Ivette 相同）。它会**惰性拉起** Frama-C——**不再需要你手工启动 Frama-C server**。
- **`ast-utils` Frama-C 插件**（OCaml，见 `ast-utils/`）——注册 MCP 依赖的 server request（AST 访问、依赖提取、ACSL 注入、WP 配置）。**没有它，大部分工具会失败**，因此必须构建并安装到与 Frama-C 相同的 opam switch。

### Sandbox model / 沙箱模型

`create_sandbox` extracts a function **with all its type/callee/global dependencies** into a temporary self-contained C file and launches a **separate** Frama-C instance on it. The agent can then iterate ACSL annotations and run WP there without touching the main project; verified results are merged back explicitly.

`create_sandbox` 把一个函数**连同它全部的类型/被调用者/全局依赖**提取成一个独立的临时 C 文件，并在其上启动**独立的** Frama-C 实例。智能体可在沙箱里反复迭代 ACSL 注解并跑 WP，不污染主工程；验证通过的结果再显式合并回去。

## MCP Tools / MCP 工具（43）

### Project & Source / 工程与源码

| Tool | Description / 说明 |
|------|---------------------|
| `reload_project` | Load/reload C source files; reparses AST, refreshes cached state / 加载或重载 C 源文件，重解析 AST 并刷新缓存 |
| `list_files` / `list_functions` / `list_globals` / `list_declarations` | Enumerate project contents / 枚举工程内容 |
| `lookup_symbol` | Look up a function or global by name / 按名称查找函数或全局变量 |
| `get_function_info` | Location, signature, annotated declaration / 位置、签名、带注解的声明 |
| `get_function_ast` | Structured AST of a function (statements + sids) / 函数的结构化 AST（语句 + sid）|
| `print_source_main` / `print_source_sandbox` | Pretty-print current source with annotations / 打印当前带注解的源码 |

### Analysis / 分析

| Tool | Description / 说明 |
|------|---------------------|
| `run_eva` | Abstract interpretation; finds potential runtime errors / 抽象解释，发现潜在运行时错误 |
| `get_eva_alarms` / `get_eva_value` | List alarms; query value ranges at a program point / 列出报警；查询程序点值域 |
| `investigate_alarm` | Deep dive: value ranges + callers + annotations in one call / 一次返回值域、调用者、注解 |
| `suggest_verification_plan` | Analyze state, suggest next actions / 分析状态并建议下一步 |
| `run_linear_invariant` | Linear loop-invariant synthesis helper / 线性循环不变式合成辅助 |

### WP / 演绎证明

| Tool | Description / 说明 |
|------|---------------------|
| `run_wp_main` / `run_wp_sandbox` | Run WP on the main project / on a sandbox / 在主工程或沙箱上跑 WP |
| `get_wp_goals` | Proof goals with status (VALID / UNKNOWN / TIMEOUT / FAILED) / 证明目标及状态 |
| `get_vc_details` | Full verification condition of a goal / 某目标的完整验证条件 |
| `get_verification_status` | Aggregate status: property counts, EVA/WP state / 综合状态：属性统计、EVA/WP 状态 |

### Annotations / 注解

| Tool | Description / 说明 |
|------|---------------------|
| `add_annotation_main` / `add_annotation_sandbox` | Add one ACSL clause (spec or statement annotation) / 添加单条 ACSL（契约或语句注解）|
| `inject_all_annotations_main` / `inject_all_annotations_sandbox` | Inject a full annotation set atomically / 原子注入整套注解 |
| `get_current_annotations` | List a function's ACSL with verification status / 列出函数 ACSL 及验证状态 |
| `extract_annotations` | Extract annotations added inside a sandbox / 提取沙箱内新增的注解 |
| `validate_acsl` | Syntax/typing check an ACSL string before injecting / 注入前校验 ACSL 语法与类型 |

### Sandbox / 沙箱

| Tool | Description / 说明 |
|------|---------------------|
| `create_sandbox` | Extract function + deps into a temp file, launch a separate Frama-C / 提取函数及依赖到临时文件并启动独立 Frama-C |
| `reset_sandbox` | Recreate from the original, preserving the experiment id / 从原函数重建，保留 experiment id |
| `delete_sandbox` | Tear down the sandbox instance / 销毁沙箱实例 |

### Navigation & Scheduling / 导航与调度

| Tool | Description / 说明 |
|------|---------------------|
| `get_callgraph` | Function call graph / 函数调用图 |
| `find_callers` | All callers of a function / 函数的所有调用者 |
| `trace_call_chain` | Multi-level call chain (up or down) / 多层调用链（向上/向下）|
| `compute_topological_order` | Tarjan + Kahn: bottom-up order + SCC groups with levels / 拓扑序（自下而上）+ 带层级的 SCC 分组 |
| `get_ready_functions` | Which functions are ready to verify next / 下一批可验证的函数 |

### Verification State / 验证状态

| Tool | Description / 说明 |
|------|---------------------|
| `store_function_conclusion` / `get_function_conclusion` / `list_conclusions` | Persist and query per-function verification conclusions / 存取每个函数的验证结论 |
| `store_project_state` / `get_project_state` | Persist and query project-level orchestration state / 存取工程级编排状态 |
| `lock_project` / `unlock_project` | Guard the main project against reload/WP during batch work / 批量作业期间保护主工程不被重载/跑 WP |

## Quick Start / 快速上手

### Prerequisites / 前置条件

- [Frama-C](https://frama-c.com/) 31.0 (Gallium)
- OCaml >= 4.14.2 + opam + dune >= 3.0 — **required to build the `ast-utils` plugin**
- Rust (edition 2021)
- WP provers: alt-ergo (pulled in by Frama-C), and optionally z3 / cvc5

### Build / 构建

```bash
# 1. Build + install the ast-utils Frama-C plugin (same opam switch as Frama-C)
#    构建并安装 ast-utils 插件（与 Frama-C 同一个 opam switch）
cd ast-utils
dune build && dune install
cd ..

# 2. Build the MCP server / 构建 MCP 服务器
cargo build --release
```

> After changing the plugin, run `dune clean && dune build && dune install` — an incremental
> build may not relink the `.cmxs`, leaving the old plugin installed.
> 改动插件后请 `dune clean && dune build && dune install`——增量构建可能不重新链接 `.cmxs`，导致装的还是旧版。

### Usage / 使用

The server **spawns Frama-C itself** on the first `reload_project` call. You only point it at the `frama-c` binary:

服务器在第一次 `reload_project` 时**自行拉起 Frama-C**。你只需告诉它 `frama-c` 二进制在哪：

```bash
./target/release/frama-c-mcp-server --frama-c /path/to/frama-c
```

| Flag | Default | Description / 说明 |
|------|---------|---------------------|
| `--frama-c` | `frama-c` | Path to the Frama-C binary / Frama-C 二进制路径 |
| `--max-sandboxes` | `32` | Safety ceiling on concurrent sandbox Frama-C processes / 并发沙箱进程的安全上限 |
| `--socket` | — | **Deprecated and ignored** — sockets are auto-generated per process / **已废弃且被忽略**，socket 现按进程自动生成 |

### MCP Configuration / MCP 配置

`.mcp.json` (project) or `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "frama-c": {
      "command": "/path/to/frama-c-mcp-server",
      "args": ["--frama-c", "/path/to/frama-c"]
    }
  }
}
```

## Verification Workflows / 验证工作流

**Direct loop / 直接循环** — EVA finds problems, agent writes ACSL, WP proves:

```
reload_project → run_eva → get_eva_alarms → investigate_alarm
              → add_annotation_main / inject_all_annotations_main
              → run_wp_main → get_wp_goals   (repeat until all VALID)
```

**Sandboxed CEGIS / 沙箱化 CEGIS** — iterate without touching the main project:

```
create_sandbox → validate_acsl → inject_all_annotations_sandbox → run_wp_sandbox
              → get_wp_goals / get_vc_details   (counter-example guided retry)
              → extract_annotations → inject into main → run_wp_main
              → store_function_conclusion → delete_sandbox
```

**Whole-program, bottom-up / 全程序自下而上** — verify callees before callers:

```
reload_project → get_callgraph → compute_topological_order
              → get_ready_functions → (verify each) → store_function_conclusion
              → repeat until every function has a conclusion
```

## Testing / 测试

```bash
# Unit tests — no Frama-C needed / 单元测试，无需 Frama-C
cargo test --lib

# Integration + MCP stdio E2E — need frama-c + ast-utils installed on PATH
# 集成与 MCP stdio 端到端测试——需要 PATH 上有 frama-c 且已装 ast-utils
export PATH="$(opam var bin):$PATH"
cargo test --test integration_test -- --test-threads=1
cargo test --test mcp_stdio_test --release -- --test-threads=1
```

| Suite | Needs Frama-C? | Description / 说明 |
|-------|----------------|---------------------|
| `cargo test --lib` | no / 否 | Codec, state, callgraph, topological order / 编解码、状态、调用图、拓扑序 |
| `integration_test` | yes / 是 | Live Frama-C server: EVA, WP, annotations, sandbox / 真实 Frama-C：EVA、WP、注解、沙箱 |
| `mcp_stdio_test` | yes / 是 | Full MCP stdio surface / 完整 MCP stdio 工具面 |
| `store_conclusion_test` | no / 否 | Conclusion persistence / 结论持久化 |
| `lazy_spawn_regression_test`, `reload_project_regression_test`, `sigterm_cleanup_test`, `zombie_reap_test` | yes / 是 | Process lifecycle: lazy spawn, reload, signal cleanup, child reaping / 进程生命周期 |

CI runs both lanes (`.github/workflows/ci.yml`): a fast Rust-only job, and a full job that installs Frama-C + provers + the `ast-utils` plugin.

CI 跑两条线（`.github/workflows/ci.yml`）：纯 Rust 快跑；以及安装 Frama-C + 证明器 + `ast-utils` 插件的完整集成。

## Project Structure / 项目结构

```
src/
├── main.rs                 # CLI entry point (lazy spawn) / 入口（惰性拉起）
├── lib.rs                  # Library root / 库根
├── state.rs                # Session + conclusion + project state / 会话、结论、工程状态
├── topo.rs                 # Tarjan + Kahn topological order / 拓扑排序
├── linear_invariant.rs     # Linear invariant CLI bridge / 线性不变式 CLI 桥接
├── error.rs                # Error types / 错误类型
├── frama_c/
│   ├── client.rs           # Frama-C client (GET/SET/EXEC/POLL)
│   ├── codec.rs            # Wire protocol codec / 协议编解码（S/L 长度前缀分帧）
│   └── transport.rs        # Unix socket transport / Unix Socket 传输层
└── mcp/
    ├── server.rs           # 43 MCP tool implementations / 43 个 MCP 工具实现
    ├── param_compat.rs     # Tool parameter compatibility / 参数兼容层
    └── types.rs            # Tool parameter types / 工具参数类型

ast-utils/                  # Frama-C plugin (OCaml) — REQUIRED / Frama-C 插件（OCaml），必需
├── src/                    # AST export, extraction, sandbox, ACSL injection, request registry
└── test/                   # Plugin regression tests / 插件回归测试

test/                       # C fixtures for integration tests / 集成测试用 C 文件
tests/                      # Rust integration tests / Rust 集成测试
docs/                       # Architecture and design notes / 架构与设计文档
```

## Technical Notes / 技术要点

**Frama-C Server Protocol / Frama-C 服务器协议**: Custom binary protocol (not JSON-RPC). Commands: `GET`/`SET`/`EXEC`/`POLL`/`SHUTDOWN`. Framing: `S`+3 hex or `L`+7 hex length prefix. `SET` and `EXEC` are queued — must use a `POLL` loop. Same protocol Ivette (Frama-C's official GUI) uses. / 自定义二进制协议（非 JSON-RPC）。`SET` 和 `EXEC` 为队列式——需 `POLL` 轮询。与 Ivette（官方 GUI）使用相同协议。

**AST Reload / AST 重载**: File reparse requires `setFiles([])` → `setFiles(files)` → `compute`. Direct `setFiles(same_value)` is a no-op due to Frama-C's state dependency system. This matches Ivette's `reparseFiles()` implementation. / 文件重新解析需要三步。直接 `setFiles(同值)` 是空操作。与 Ivette 的 `reparseFiles()` 一致。

**Fetch APIs are incremental / fetch API 是增量的**: `fetchFunctions` returns everything only on the first call, deltas afterwards. Call `reloadFunctions` first whenever you need a full snapshot. / `fetchFunctions` 只在首次返回全量，之后只返回变更；需要全量前先 `reloadFunctions`。

**WP memory model / WP 内存模型**: This server configures WP with `Typed+nocast`; casts make the corresponding VCs fail safely rather than being silently assumed away. / 本服务器把 WP 配为 `Typed+nocast`；存在 cast 时相关 VC 会安全失败，而非被静默放过。

**Callee contracts / 被调用者契约**: A bare declaration with no contract defaults to `assigns \nothing`, which is **unsound** (WP manual §2.1). Sandbox extraction therefore emits empty-body stubs for callees that lack an explicit `assigns`. / 无契约的裸声明默认 `assigns \nothing`，这是**不可靠的**；因此沙箱提取会为缺少显式 `assigns` 的被调用者生成空体 stub。

## License

MIT — including the bundled `ast-utils` Frama-C plugin.
