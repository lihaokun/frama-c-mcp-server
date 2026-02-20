# Frama-C MCP Server 设计讨论记录

> **日期**: 2025-02-17  
> **参与者**: 用户 + Claude  
> **产出**: frama-c-mcp-rust-zmq-设计-v2.2.md

---

## 讨论线索总览

```
话题 1: MCP 基础概念 → 为什么需要 MCP？MCP vs Skill？
    │
话题 2: Rust MCP 生态 → rmcp SDK、Rust vs Python
    │
话题 3: 设计文档 v1 回顾 → 15 个 tool、ZMQ 协议、架构决策
    │
话题 4: 更新设计文档 → v2（rmcp 0.15、MCP 2025-11-25、Task 支持）
    │
话题 5: CIL 替代 LSP → Frama-C AST 遍历能否支持代码搜索？
    │                  → 验证场景下 CIL 比 clangd 更优的论证
    │                  → v2.1（第六组：5 个代码导航 tool）
    │
话题 6: Agentic Search → MCP 能否支持 agentic search skill？
                       → 聚合查询减少 Agent 调用轮次
                       → v2.2（第七组：2 个聚合搜索 tool）
```

---

## 话题 1: MCP 基础概念

### 为什么需要 MCP

MCP (Model Context Protocol) 解决 LLM 与外部工具交互的标准化问题。核心价值：

- **结构化工具接口**：明确的输入输出 schema，模型调用更可靠
- **可组合性**：不同 MCP server 独立开发、自由组合
- **协议标准化**：不同 agent 框架可接入同一套 server
- **状态管理**：长驻进程维护会话状态（如验证上下文）

**对形式化验证场景的意义**：将 Frama-C 能力（WP、EVA、AST 查询）封装成标准工具，agent 可靠调用，避免脆弱的 shell 命令拼接。

### MCP vs Skill

| 维度 | MCP | Skill |
|------|-----|-------|
| 本质 | 通信协议/接口标准 | 能力包/知识模块 |
| 层次 | 底层传输层 | 上层应用层 |
| 内容 | 工具 schema + 调用约定 | 知识 + 策略 + 工具 + 模板 |
| 类比 | USB 协议 | 带说明书的完整外设 |
| 领域知识 | 不含（纯接口） | 核心价值就是领域知识 |

**结论**：Skill 可以包含 MCP 工具。验证规划器需要两层：MCP 层标准化暴露 Frama-C 能力，Skill 层编码验证策略。

---

## 话题 2: Rust MCP 生态

### rmcp SDK 现状

- 官方 SDK，仓库 `modelcontextprotocol/rust-sdk`
- 基于 tokio 异步运行时
- 使用 proc macro 定义工具
- 版本演进非常快（0.1 → 0.15，10 个月内 30+ 版本）

### Rust vs Python 选择

核心交互是调 Frama-C CLI/ZMQ，Rust 的 `Command` + 输出解析完全够用。Rust 优势：编译后单一 binary 部署极简、类型系统在编译期捕获 schema 不一致、所有权系统更易管理验证会话状态。

---

## 话题 3: 设计文档 v1 回顾

初始设计基于 rmcp 0.8，15 个 tool 分五组：

1. 项目初始化（3 tools）
2. EVA 分析（3 tools）
3. WP 验证（2 tools）
4. ACSL 注解（3 tools）
5. 验证规划器（2 tools + 2 专用 tool）

架构：三层（MCP Layer → Tool Router → Frama-C Client via ZMQ）。

---

## 话题 4: 设计文档 v1 → v2

### 触发原因

rmcp 从 0.8 升级到 0.15，API 发生重大变化。同时 MCP 规范从 2024-11-05 升级到 2025-11-25。

### 关键变更

| 维度 | v1 | v2 |
|------|----|----|
| rmcp 版本 | 0.8+ | 0.15+ |
| MCP 协议版本 | 2024-11-05 | 2025-11-25 |
| Rust edition | 2021 | 2024（需要 nightly） |
| Tool 宏 | `#[tool(tool_box)]` | `#[tool_router]` + `#[tool_handler]` |
| 参数传递 | `#[tool(param)]` / `#[tool(aggr)]` | `Parameters<T>` wrapper |
| 新增能力 | — | MCP Task 生命周期（长时间运行操作） |

### 研究发现

- rmcp 0.15 最新稳定（2026-02-10 发布）
- MCP 2025-11-25 引入 Task primitive（call-now, fetch-later），非常适合 EVA/WP 长时间分析
- schemars 1.0+ 是 rmcp 0.15 的硬要求
- `transport-io` 替代了 `transport-stdio`
- `#[tool_router]` + `#[tool_handler]` 分离了工具注册和 ServerHandler 实现

---

## 话题 5: CIL 替代 LSP（v2.1）

### 用户问题

> 看一下 frama-c server 的 api 他对 cil 结构的遍历, 可以让支撑 claude code 的代码 search 使用吗? 比如 lsp 一类的?

### 分析过程

1. **研究 Frama-C Server API**：通过搜索和阅读文档，确认 Frama-C Server 通过 `kernel.ast.*`、`callgraph.*`、`from.*` 等 request 暴露了丰富的 CIL AST 查询能力。这正是 Ivette GUI 内部使用的同一套接口。

2. **初始判断**：CIL 不太适合作为通用 LSP 替代（启动成本高、CIL 是 normalized AST、不支持编辑场景），更适合作为 clangd 之上的语义增强层。

3. **用户追问**：

   > 验证过程中, 代码的修改是非常谨慎的, 而且 frama-c 已经在启动情况下, 用 cil 代替是不是就比较合理了?

4. **重新评估**：用户指出了关键的场景约束——验证场景下代码修改极少且受控（仅通过 inject/remove_acsl），Frama-C 已启动 CIL AST 已在内存中。这让 CIL 替代 LSP 的论点成立。

### 决策

在验证工作流中，CIL 替代 LSP 是合理的，原因：

- **零额外开销**：Frama-C 已启动，AST 已在内存
- **信息更丰富**：带 ACSL 注解 + 验证状态 + EVA 值域 + 函数间依赖
- **修改受控**：仅通过白名单 tool 修改注解
- **天然一致**：AST 和分析结果自动同步

但保留了一个前提：如果未来需要支持源码编辑（Agent 自动修复 C 代码），则需要补充 clangd。

### 产出

新增第六组 CIL 代码导航 tool（5 个）：
- `find_callers` — 调用点查找（Phase 1，内置 API）
- `get_data_deps` — 数据依赖分析（Phase 2，From 插件）
- `find_memory_ops` — 内存操作定位（Phase 3，需 OCaml 插件）
- `lookup_symbol` — 符号查询（Phase 1，内置 API）
- `get_cfg` — 控制流图（Phase 2）

---

## 话题 6: Agentic Search（v2.2）

### 用户问题

> 另外一个关键 skill 是 agentic search，我们开发的 mcp 可以支持这个功能吗？

### 分析过程

1. **厘清概念**：Agentic search 不是一个单独的 tool，而是 agent 利用多个结构化查询 tool 进行多步推理的能力。

2. **现有能力评估**：发现已有的 20 个 tool 天然支持 agentic search 的三层搜索：
   - 结构导航（第六组 CIL tool）
   - 语义查询（EVA/WP tool）
   - 全局视图（项目/规划 tool）

3. **瓶颈识别**：Agent 做 agentic search 时最常见的模式是"从一个线索出发多步追踪"，如果每步都是独立 tool call 会产生 5-10 轮往返，增加延迟和 token 消耗。

### 决策

新增第七组 Agentic Search 聚合查询 tool（2 个）：

1. **`trace_call_chain`** — 多层调用链追踪
   - 动机：`find_callers` 只返回直接调用者，要追踪完整调用链需多次调用
   - 实现：Rust BFS 遍历 callgraph，支持方向（上/下）、深度限制、终止点
   - 阶段：Phase 2（纯 Rust，不需要 OCaml 插件）

2. **`investigate_alarm`** — alarm 深度调查
   - 动机：拿到一个 alarm 后需要查值域、调用链、数据依赖、已有注解、CFG——通常需要 5-6 次独立 tool call
   - 实现：MCP server 端组合多个 FramaCClient GET 查询，三级深度（quick/normal/deep）
   - 阶段：Phase 2（纯 Rust 组合查询）

### 典型 Agentic Search 工作流

```
任务："找出程序中所有可能的缓冲区溢出风险"

1. get_callgraph()                        # 全局结构
2. run_eva(precision: 3)                  # 粗粒度分析
3. get_eva_alarms(type: "mem_access")     # 定位可疑 alarm
4. investigate_alarm(id: "alarm-87",      # ← 聚合查询，一次获取完整上下文
     depth: "deep")
   → alarm 详情 + 值域 + 调用链 + 数据依赖 + CFG + 已有注解
5. inject_acsl(...)                       # 修复
6. run_wp(...)                            # 验证修复
```

没有 `investigate_alarm`，步骤 4 需要 5-6 次独立 tool call。

---

## 设计演进总结

```
v1 (2025-02-04)
  15 tools, 5 组, rmcp 0.8, MCP 2024-11-05
  │
v2 (2025-02-17)
  15 tools, 5 组, rmcp 0.15, MCP 2025-11-25, Task 支持
  │  宏 API 重构: tool_box → tool_router + tool_handler
  │  schemars 1.0+, edition 2024
  │
v2.1 (2025-02-17)
  20 tools, 6 组
  │  新增第六组: CIL 代码导航（替代 LSP）
  │  设计决策: 验证场景下 Frama-C CIL 比 clangd 更优
  │
v2.2 (2025-02-17)
  20 tools, 7 组
     新增第七组: Agentic Search 聚合查询
     设计决策: 减少 Agent 调用轮次，server 端组合多步查询
```

### 最终 Tool 清单（20 个）

| 组 | 名称 | 工具数 | 说明 |
|---|------|-------|------|
| ① | 项目初始化 | 3 | load_project, get_callgraph, get_function_info |
| ② | EVA 分析 | 3 | run_eva, get_eva_alarms, get_eva_value |
| ③ | WP 验证 | 2 | run_wp, get_wp_goals |
| ④ | ACSL 注解 | 3 | inject_acsl, remove_acsl, get_current_annotations |
| ⑤ | 验证规划 | 2 | get_verification_status, suggest_verification_plan |
| ⑥ | CIL 代码导航 | 5 | find_callers, get_data_deps, find_memory_ops, lookup_symbol, get_cfg |
| ⑦ | Agentic Search | 2 | trace_call_chain, investigate_alarm |

### 实施路线图

| Phase | 周期 | 核心交付 |
|-------|------|---------|
| Phase 1 | 2 周 | ZMQ 通信 + 5 个基础 tool + MCP Inspector 测试 |
| Phase 2 | 2-3 周 | 全部 22 tool + SessionState + 集成测试 |
| Phase 3 | 3-4 周 | OCaml 插件 + Task 支持 + ACSL 迭代循环 |
| Phase 4 | 2 周 | Agent 集成 + 千行级测试 + 效果评估 |
