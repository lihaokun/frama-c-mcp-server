# 代码审核与修复报告

> **审核日期**：2026-02-20
> **审核范围**：全部 Rust 源码（1,922 行，13 个文件）
> **审核维度**：一致性、风格、正确性、性能、可维护性（`workflow.md` §4.2）

---

## 1. 问题清单

| # | 严重度 | 文件 | 问题 |
|---|--------|------|------|
| F1 | 中 | `server.rs:53-57` | `reload_project` 的 `fetchFunctions` 未先 `reloadFunctions`（防御性修复） |
| F2 | 高 | `server.rs:94-102` | `get_function_info` 缓存未命中时 `fetchFunctions` 返回空，导致已有缓存被清空（级联失效） |
| F3 | 中 | `server.rs:301-311` | `run_wp` 缓存未命中时直接报错，未尝试刷新函数缓存 |
| F4 | 中 | `server.rs:402` | `get_verification_status` 硬编码 `project_loaded: true`，未从 state 读取 |
| F5 | 低 | `codec.rs:34` | Clippy 警告：`payload.as_bytes().len()` → `payload.len()` |
| F6 | 低 | `types.rs:28` | `GetEvaValueParams.marker` 文档注释中 `#S42` 不是有效 marker 格式 |
| F7 | 中 | `server.rs:220-225` | `get_eva_alarms` 的函数名解析缓存未命中时静默跳过过滤，返回全部属性 |

---

## 2. 分类处理

### 2.1 显而易见的错误 — 直接修复

| # | 问题 | 修复方式 | 需补测试 |
|---|------|---------|---------|
| F5 | `payload.as_bytes().len()` | 改为 `payload.len()` | 否（已有 roundtrip 测试覆盖） |
| F6 | 文档注释 `#S42` | 改为 `#s2` | 否 |

### 2.2 同类问题（增量 fetch 未 reload）— 统一修复

F1、F2 是同一类问题：调用 `fetch_all("kernel.ast.fetchFunctions")` 前未调 `reloadFunctions`。

**根因分析**：`get_eva_alarms` 和 `get_verification_status` 在后续迭代中已发现并修复了 `fetchStatus` 的同类问题（加了 `reloadStatus`），但 `fetchFunctions` 的调用点未做同步排查。

**影响范围排查**：项目中所有 `fetch_all` 调用点：

| 调用位置 | fetch 请求 | 有 reload？ | 状态 |
|----------|-----------|------------|------|
| `client.rs:221` connect() | `fetchFunctions` | 不需要（首次消费） | OK |
| `server.rs:55` reload_project | `fetchFunctions` | **缺失** | **F1** |
| `server.rs:96` get_function_info | `fetchFunctions` | **缺失** | **F2** |
| `server.rs:209` get_eva_alarms | `fetchStatus` | 有 `reloadStatus` | OK |
| `server.rs:361` get_verification_status | `fetchStatus` | 有 `reloadStatus` | OK |

**F1 严重度说明**：`reload_project` 在 `fetchFunctions` 前先调用了 `kernel.ast.compute`（EXEC），`compute` 重建 AST 时触发 Frama-C Server 内部信号机制，Array 状态会被标记为变更，下次 `fetchFunctions` 应返回 `reload: true` + 全部新数据。因此在正常流程下 F1 可能不会触发。但在边缘场景下（如 `compute` 未实际改变 AST、信号未传播），仍有风险。将修复定位为**防御性措施**，严重度定为中。

**F2 影响分析（级联失效）**：F2 的后果比单纯"返回空"更严重。`update_functions` 的实现是先 `self.functions.clear()` 再逐条插入。当 `fetchFunctions` 返回空（已被 connect 消费）时，`update_functions(&[])` 会**清空已有缓存**：

1. `connect()` 消费 `fetchFunctions` → 缓存 3 个函数
2. 用户查询一个不存在的函数 → 缓存未命中
3. `fetchFunctions` 返回空（已被 connect 消费）
4. `update_functions(&[])` → `clear()` → **原来的 3 个函数也丢了**
5. 后续所有函数查询全部失败

这是级联失效，严重度为高。

**修复方式**：在 F1、F2 的 `fetch_all` 前加 `reloadFunctions`。

**属于**：F1 为防御性修复。F2 为必须修复的正确性问题（与已修复的 `reloadStatus` 完全对称）。

**需补测试**：是。在集成测试中增加场景：查询一个不存在的函数后，再查询已存在的函数，验证缓存未被清空。

### 2.3 函数名解析不一致 — 统一修复（F3、F7）

当前函数名→marker 解析存在三种不一致的行为：

| 工具 | 缓存未命中行为 | 问题 |
|------|--------------|------|
| `get_function_info` | 刷新 → 重试 → 仍未命中报错 | 逻辑正确（修复 F2 后） |
| `run_wp` | **直接报错** | F3：缺少刷新逻辑 |
| `get_eva_alarms` scope 解析 | **静默跳过过滤** | F7：返回全部属性，无提示 |

**F3 修复方式**：在 `run_wp` 中，缓存未命中时先 `reloadFunctions` + `fetchFunctions` 刷新缓存，再重试。逻辑与修复后的 `get_function_info` 一致。

**F7 修复方式**：`get_eva_alarms` 的 scope 解析（`server.rs:220-225`）缓存未命中时，应与 `get_function_info` 同样尝试刷新缓存再重试，而非静默返回 `None` 导致过滤条件失效。

**F7 影响分析**：用户传入 `function: "abs_val"` 但该函数不在缓存中时，`resolve_function` 返回 `None`，`scope_marker` 为 `None`，过滤条件被跳过 — 返回**所有函数的属性**而非仅 `abs_val` 的。不报错也不提示，用户无法察觉。

### 2.4 设计层面的遗漏（F4）— 需更新设计文档

**F4**：`get_verification_status` 硬编码 `project_loaded: true`，与 `invalidate_all()` 语义矛盾。

**修复方式**：将 `project_loaded: true` 改为从 `state.project_loaded` 读取。`eva_state` 和 `wp_state` 已在上方（`server.rs:365-368`）通过 state 读锁获取，可一并读取 `project_loaded`。

**需更新设计文档**：是。`detailed-design.md` 中 `run_wp`、`get_eva_alarms`、`get_verification_status` 的算法部分。

**需补测试**：
- F3：在集成测试中不先调 `get_function_info`，直接调 `run_wp`，验证能找到函数
- F4：单元测试中 `invalidate_all` 后 `project_loaded == false`（已有此断言，OK）
- F7：在集成测试中验证 `get_eva_alarms(function="abs_val")` 只返回 abs_val 的属性

---

## 3. 举一反三

### 3.1 模式：「所有 fetch 调用前必须 reload」

提炼规则：**任何工具方法中调用 `fetch_all` 都应先调对应的 `reload`**（除 `connect()` 中的首次消费外）。

后续新增工具时应遵守此规则。

### 3.2 模式：「函数名解析应统一」— 建议抽取公共方法

当前函数名→marker 解析存在四个调用点：

| 调用点 | 缓存未命中行为 |
|--------|--------------|
| `get_function_info` | 刷新 → 重试 |
| `run_wp` | 直接报错（F3） |
| `get_eva_alarms` scope | 静默跳过（F7） |
| `get_eva_value`（间接，marker 由用户传入） | 不涉及 |

其中前三个都需要"缓存未命中 → `reloadFunctions` + `fetchFunctions` 刷新 → 重试 → 仍未命中才报错"的逻辑。三个调用点已达到抽取阈值，建议在 `FramaCMcpServer` 上抽取公共方法：

```rust
async fn resolve_function_or_refresh(&self, name: &str) -> Result<FunctionInfo, McpError>
```

封装：缓存查找 → 未命中则 reload + fetch + 更新缓存 → 再查找 → 仍未命中返回 FunctionNotFound。

### 3.3 模式：「state 字段应从 state 读取」

`get_verification_status` 中 `eva_completed` 和 `wp_completed` 从 state 读取，但 `project_loaded` 硬编码。应确保所有 session 字段均从 state 读取。

---

## 4. 修复执行计划

| 步骤 | 内容 | 涉及文件 |
|------|------|---------|
| 1 | 更新设计文档：增加 `resolve_function_or_refresh` 公共方法、修正 `get_verification_status` session 字段、修正 `get_eva_alarms` scope 解析 | `detailed-design.md` |
| 2 | 修复 F5（clippy）、F6（文档注释） | `codec.rs`, `types.rs` |
| 3 | 抽取 `resolve_function_or_refresh` 公共方法（含 reloadFunctions） | `server.rs` |
| 4 | 修复 F1（reload_project 加 reloadFunctions） | `server.rs` |
| 5 | 修复 F2（get_function_info 改用公共方法） | `server.rs` |
| 6 | 修复 F3（run_wp 改用公共方法） | `server.rs` |
| 7 | 修复 F7（get_eva_alarms scope 解析改用公共方法） | `server.rs` |
| 8 | 修复 F4（project_loaded 从 state 读取） | `server.rs` |
| 9 | 补充测试用例 | `integration_test.rs` |
| 10 | 回归测试（全部 unit + integration） | — |

步骤 3-7 有依赖关系：先抽取公共方法（步骤 3），再逐个调用点替换（步骤 4-7）。

---

## 5. 验证方式

| 修复 | 验证 |
|------|------|
| F1 | 集成测试：`reload_project` 后验证函数列表非空 |
| F2 | 集成测试：查询不存在的函数后，再查询已存在的函数仍成功（缓存未被清空） |
| F3 | 集成测试：不先查函数，直接 `run_wp` 成功 |
| F4 | 单元测试：`invalidate_all` 后 `project_loaded == false`（已有） |
| F5 | `cargo clippy` 无警告 |
| F6 | 目视确认 |
| F7 | 集成测试：`get_eva_alarms(function="abs_val")` 只返回 abs_val 的属性（非全部） |
| 回归 | `cargo test --lib` + `cargo test --test integration_test` 全通过 |

---

## 附录：审核中确认无问题的部分

| 模块 | 行数 | 评价 |
|------|------|------|
| `codec.rs` | 360 | 编解码正确，18 个单元测试覆盖完整 |
| `transport.rs` | 59 | 简洁，帧缓冲逻辑正确 |
| `client.rs` | 322 | `wait_for_id` + `poll_loop` 实现正确，握手流程完整 |
| `state.rs` | 164 | 数据结构清晰，5 个单元测试覆盖 |
| `error.rs` | 78 | 错误分类合理，MCP 转换完整 |
| `main.rs` | 41 | 启动流程正确，tracing 已修正写 stderr |
