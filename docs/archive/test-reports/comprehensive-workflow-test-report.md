# Comprehensive AI Agent Verification Workflow — Test Report

> 测试日期：2026-02-21
> 测试目标：模拟一个 AI 智能体使用全部 15 个 MCP 工具，对 C 程序进行完整的形式验证工作流

---

## 1. 测试概览

| 项目 | 值 |
|------|---|
| 测试用 C 文件 | `test/test_comprehensive.c` — "Safe Buffer Module" |
| Frama-C Server | `frama-c test/test_comprehensive.c -server-socket /tmp/frama-c-test-comp.sock` |
| 测试入口 | `tests/integration_test.rs::test_comprehensive` |
| 测试结果 | **PASS** — 全部 4 阶段、19 个步骤通过 |
| 耗时 | ~5 秒 |
| 工具覆盖 | 15/15 个 MCP 工具的底层 API 全部被调用 |

### 1.1 测试 C 文件设计

`test_comprehensive.c` 是专门设计的验证测试靶标，包含：

```
函数 (9):
  buf_push   — behaviors (ok/full), complete/disjoint, assigns  → WP 全部可证
  buf_get    — 简单契约 (requires/ensures/assigns)              → WP 全部可证
  buf_sum    — 循环不变式, 调用 buf_get                         → EVA 分析
  buf_avg    — 带前置条件的除法                                  → EVA 分析
  echo       — 命名 ensures (correct 可证, wrong 不可证)        → WP 产生 NORESULT
  unsafe_read — 无前置条件的数组访问                             → EVA 数组越界报警
  unsafe_avg  — 无前置条件的除法                                → EVA 除零报警
  run        — 编排器, 调用 buf_push/buf_sum/buf_avg           → 调用链中间层
  main       — 入口, 使用 volatile int nondet 非确定性输入      → EVA 入口

全局变量 (4):
  data[16]   — 缓冲区数组
  count      — 当前元素数
  error_code — 错误码
  nondet     — volatile, 用于 EVA 非确定性输入

调用链 (4 层):
  main → run → buf_sum → buf_get
  main → run → buf_avg → buf_sum → buf_get
  main → unsafe_read
  main → unsafe_avg
  main → echo
```

### 1.2 设计来源

- **behaviors + complete/disjoint**: 参考 Frama-C WP 测试套件 (`tests/wp/wp_acsl/`)
- **named ensures (correct/wrong)**: 参考 `tests/wp/wp_acsl/unit_bit_test.c` 模式
- **volatile nondet**: 参考 Frama-C EVA 测试套件，用于生成不可判定的输入
- **unsafe 函数**: 故意省略前置条件，触发 EVA 报警

---

## 2. 工作流阶段与步骤

### Phase A: 侦察 (Reconnaissance)

模拟 AI 智能体首次连接到 Frama-C，发现项目结构。

#### A1. 连接并加载函数 (`reload_project`)

**MCP 工具**: `reload_project`

**操作**: 连接 Frama-C Server，自动执行 `reloadFunctions` + `fetchFunctions` 初始化状态缓存。

**实际结果**:
```
9 functions loaded:
  buf_push  → marker=kf#27, decl=#F27, line=51
  buf_get   → marker=kf#34, decl=#F34, line=69
  buf_sum   → marker=kf#39, decl=#F39, line=78
  buf_avg   → marker=kf#50, decl=#F50, line=95
  echo      → marker=kf#56, decl=#F56, line=106
  unsafe_read → marker=kf#60, decl=#F60, line=112
  unsafe_avg  → marker=kf#64, decl=#F64, line=118
  run       → marker=kf#68, decl=#F68, line=124
  main      → marker=kf#78, decl=#F78, line=137
```

**断言**: 9 个函数全部加载，文件名以 `test_comprehensive.c` 结尾，行号 > 0。

---

#### A2. 查询全局变量 (`lookup_symbol`)

**MCP 工具**: `lookup_symbol`（全局变量分支）

**操作**: `reloadGlobals` + `fetchGlobals` → `update_globals` 填充缓存 → `resolve_global` 查询。

**实际结果**:
```
globals loaded (≥ 3):
  count      → decl=#G25, type=int
  error_code → decl=#G26, type=int
  data       → decl=#G23, type=int [16]   (数组)
  nondet     → decl=#G77, type=volatile int
```

**断言**: `count` 的 `declaration` 以 `#G` 开头；`error_code` 的 `typ` 为 `"int"`。

---

#### A3. 计算调用图 (`get_callgraph`)

**MCP 工具**: `get_callgraph`

**操作**: `plugins.callgraph.compute` (EXEC) → `plugins.callgraph.getCallgraph` (GET) → `update_callgraph` 缓存边和顶点。

**实际结果**:
```
10 edges, 9 vertices

run callees: ["#F50" (buf_avg), "#F43" (buf_sum), "#F27" (buf_push)]
main callees: ["#F68" (run), "#F64" (unsafe_avg), "#F60" (unsafe_read), "#F56" (echo)]
```

**断言**: 边数 > 0，顶点数 > 0，`run` 至少有 3 个 callee。

---

#### A4. 调用链追踪 (`trace_call_chain`)

**MCP 工具**: `trace_call_chain`（callees 方向）

**操作**: 从 `main` 出发做 BFS，最大深度 5，遍历 `get_callees`。

**实际结果**:
```
BFS call chain from main:
  main→run (depth 0)
  main→unsafe_avg (depth 0)
  main→unsafe_read (depth 0)
  main→echo (depth 0)
  run→buf_avg (depth 1)
  run→buf_sum (depth 1)
  run→buf_push (depth 1)
  unsafe_avg→buf_sum (depth 1)
  buf_avg→buf_sum (depth 2)
  buf_sum→buf_get (depth 2)
```

**关键路径**: `main → run → buf_sum → buf_get`（4 层调用链）

**断言**: BFS 至少访问 5 个函数。实际访问 8 个（main, run, unsafe_avg, unsafe_read, echo, buf_avg, buf_sum, buf_push, buf_get — 去重后 9 个）。

---

#### A5. 函数详情 (`get_function_info`)

**MCP 工具**: `get_function_info`

**操作**: `printDeclaration(#F27)` 获取 `buf_push` 带 ACSL 注解的声明文本。

**实际结果**: 返回 JSON 数组，包含标记化 AST（含 `marker` 字段），可用于后续 EVA 值查询。

**断言**: `printDeclaration` 返回类型为 array。

---

### Phase B: EVA 分析 (Abstract Interpretation)

模拟 AI 智能体运行 EVA，发现运行时风险，深入调查报警。

#### B1. 运行 EVA (`run_eva`)

**MCP 工具**: `run_eva`

**操作**: `plugins.eva.general.compute` (EXEC, 最长 300 秒) → `getComputationState` 确认完成 → 设置 `eva_completed` 标志。

**实际结果**: `computationState = "computed"`

**断言**: 计算状态为 `"computed"`。

---

#### B2. 获取 EVA 报警 (`get_eva_alarms`)

**MCP 工具**: `get_eva_alarms`

**操作**: `reloadStatus` + `fetchStatus` → 按 status 分组统计。

**实际结果**:
```
49 properties total:
  valid:             24  (EVA 证明安全的)
  unknown:            6  (EVA 无法确定的)
  never_tried:       18  (尚未验证的, 如 WP ensures)
  invalid_under_hyp:  1  (在某些假设下无效)

unsafe_read 的非 valid 属性: 2 条 (index_bound 报警)
```

**关键发现**:
| 函数 | 报警类型 | 状态 | kinstr |
|------|---------|------|--------|
| `unsafe_read` | `index_bound` — 数组越界 | unknown | `#k38` |
| `unsafe_avg` | `division_by_zero` — 除零 | invalid_under_hyp | `#k43` |
| `buf_sum` | `signed_overflow` — 有符号溢出 | unknown | `#k24` |
| `run` | `signed_overflow` — 有符号溢出 | unknown | `#k51` |

**断言**: ≥20 条属性；有 `valid` 类属性；有 `unknown` 类属性；`unsafe_read` 有非 valid 属性。

---

#### B3. 查找调用者 (`find_callers`)

**MCP 工具**: `find_callers`

**操作**: `plugins.eva.general.getCallers(#F34)` 查询 `buf_get` 的调用者。

**实际结果**:
```
buf_get has 1 caller: buf_sum (call=#F39, stmt=#k24)
```

**断言**: 返回类型为 array，不为空。

> 注意：`getCallers` 是 EVA 基于分析结果的调用者查询，只返回在 EVA 分析中实际到达的调用点。`buf_get` 只被 `buf_sum` 调用。

---

#### B4. 查询 EVA 值域 (`get_eva_value`)

**MCP 工具**: `get_eva_value`

**操作**:
1. `printDeclaration(unsafe_read_decl)` — 注册 marker 到服务器
2. 从 B2 的 `unsafe_read` 报警中提取 `kinstr` marker (`#k38` — 数组访问语句)
3. `plugins.eva.values.getValues(target="#k38")` — 查询该程序点的值域

**实际结果**:
```json
{
  "vBefore": {"alarms": [], "pointedVars": [], "value": "UNINITIALIZED"},
  "vAfter":  {"alarms": [], "pointedVars": [], "value": "{0; 10; 20; 30}"}
}
```

**解读**: EVA 在 `unsafe_read` 的 `return data[idx]` 语句处计算出 `data[idx]` 的返回值集合为 `{0, 10, 20, 30}`（对应 `run(10, 20, 30)` 写入的值）。`vBefore` 为 `UNINITIALIZED` 表示 `data[idx]` 在赋值前的初始状态。

**断言**: 返回值为 object，且包含 `vBefore` 或 `vAfter` 字段（非空对象）。

> 注意：循环头等非数据流语句的 `kinstr`（如 `#k17`）可能返回空 `{}`。选择报警关联的 `kinstr` 才能获得有效值域。

---

#### B5. 深入调查报警 (`investigate_alarm`)

**MCP 工具**: `investigate_alarm`

**操作**: 找到 `unsafe_avg` 的非 valid 属性 → 按三个层次调查。

**目标属性**: `#p77` — `reachable` — "reachability of stmt line 119 in unsafe_avg"

**Quick 层** — 属性详情:
```json
{
  "key": "#p77",
  "kind": "reachable",
  "status": "invalid_under_hyp",
  "scope": "#F64",
  "kinstr": "#k43"
}
```

**Normal 层** — 值域 + 调用者:
```
values at #k43: {
  "vAfter": {"alarms": [], "pointedVars": [], "value": "Unreachable"},
  "vBefore": {"alarms": [], "pointedVars": [], "value": "UNINITIALIZED"}
}
callers: [{"call": "#F78", "stmt": "#k58"}]  → 被 main 调用
```

**Deep 层** — 同函数全部注解:
```
unsafe_avg has 3 annotations total
```

**断言**: 属性被找到，调查流程完整执行。

---

#### B6. 函数注解查看 (`get_current_annotations`)

**MCP 工具**: `get_current_annotations`

**操作**: `reloadStatus` + `fetchStatus` → 按 `scope == buf_push_decl` 过滤。

**实际结果**:
```
buf_push has 16 annotations:
  ensures:  7  (behavior ok 的 4 条 + behavior full 的 3 条)
  behavior: 3  (default + ok + full)
  assumes:  2  (ok + full)
  requires: 1
  assigns:  1
  complete: 1
  disjoint: 1
```

**断言**: `buf_push` 至少有 3 条注解。实际有 16 条，涵盖 7 种 kind。

---

### Phase C: WP 验证 (Deductive Verification)

模拟 AI 智能体运行 WP 形式化证明，验证函数契约。

#### C1. 运行 WP (`run_wp`)

**MCP 工具**: `run_wp`（多函数支持）

**操作**: 对 3 个函数运行 WP：

| 函数 | 操作 | 结果 |
|------|------|------|
| `echo` | `printDeclaration(#F56)` → `startProofs(#v56)` | 完成 |
| `buf_push` | `printDeclaration(#F27)` → `startProofs(#v27)` | 完成 |
| `buf_get` | `printDeclaration(#F34)` → `startProofs(#v34)` | 完成 |

**协议关键点**:
- `startProofs` 需要 PVDecl marker (`#v<vid>`)，不是 AST.Decl (`#F<vid>`) → 需要 `#F` → `#v` 转换
- 必须先调 `printDeclaration` 将 marker 注册到服务器的 marker 表中
- `setTimeout(10)` 设置为 10 秒超时

---

#### C2. WP 目标查看 (`get_wp_goals`)

**MCP 工具**: `get_wp_goals`

**操作**: `reloadGoals` + `fetchGoals` → 解析目标结构 → 按 status 分组 → 按 scope 过滤。

**实际结果**:
```
10 total goals:
  VALID:     9
  NORESULT:  1

Goals by function:
  echo (3 goals):
    [VALID]     Assigns nothing
    [VALID]     Post-condition 'correct'     ← ensures correct: \result == x  ✓
    [NORESULT]  Post-condition 'wrong'       ← ensures wrong: \result > 0    ✗
  buf_push (5 goals):
    [VALID]     Assigns data[0 .. CAPACITY-1], count, error_code
    [VALID]     Disjoint behaviors
    [VALID]     Complete behaviors
    [VALID]     Post-condition (behavior ok)
    [VALID]     Post-condition (behavior full)
  buf_get (2 goals):
    [VALID]     Assigns nothing
    [VALID]     Post-condition \result == data[idx]
```

**关键验证**: `echo` 的 `ensures wrong: \result > 0` 目标状态为 `NORESULT`（无法证明），**不是** `VALID`。这证明 MCP 能正确区分可证明和不可证明的契约。

**fetchGoals API 格式确认**:
```json
{
  "wpo": "echo_ensures_wrong",     // 目标 ID
  "scope": "#F56",                 // 函数声明 marker (用于过滤)
  "fct": "echo",                   // 函数名 (用于显示)
  "name": "Post-condition 'wrong'", // 目标名称
  "status": "NORESULT"             // 状态 (大写)
}
```

**断言**:
- 目标数 > 0
- 有 `wpo`、`scope`、`status`、`fct`、`name` 字段
- VALID 目标 ≥ 8
- echo 有 ≥ 2 个目标
- echo 的 `wrong` 目标状态 ≠ `"VALID"`

---

### Phase D: 评估 (Assessment)

模拟 AI 智能体汇总结果，生成验证报告。

#### D1. 符号查找 — 函数 (`lookup_symbol`)

**MCP 工具**: `lookup_symbol`（函数分支）

**操作**: `resolve_function("buf_push")`

**实际结果**:
```
buf_push: marker=kf#27, decl=#F27, line=51
file ends with test_comprehensive.c ✓
marker starts with kf# ✓
declaration starts with #F ✓
```

---

#### D2. 符号查找 — 全局变量 (`lookup_symbol`)

**MCP 工具**: `lookup_symbol`（全局变量分支）

**操作**: `resolve_global("error_code")`

**实际结果**:
```
error_code: type=int, decl=#G26, marker=vi#26
declaration starts with #G ✓
marker starts with vi# ✓
```

---

#### D3. 验证建议 (`suggest_verification_plan`)

**MCP 工具**: `suggest_verification_plan`

**操作**: 检查 `project_loaded`、`eva_completed`、`wp_completed` 标志 → 查询最终属性状态 → 生成建议。

**实际结果**:
```
State: project_loaded=true, eva_completed=true, wp_completed=true

Final properties (after EVA + WP):
  valid:             39  (WP 将部分 never_tried 提升为 valid)
  unknown:            5
  never_tried:       10  (未被 WP 覆盖的函数)
  invalid_under_hyp:  1

Suggestion: EVA+WP complete → review results (priority: low)
```

**属性状态变化** (EVA only → EVA + WP):
| 状态 | EVA only | EVA + WP | 变化 |
|------|----------|----------|------|
| valid | 24 | 39 | +15 (WP 证明了 buf_push/buf_get/echo 的契约) |
| unknown | 6 | 5 | -1 |
| never_tried | 18 | 10 | -8 (被 WP 处理) |
| invalid_under_hyp | 1 | 1 | 不变 |

---

#### D4. 验证状态总览 (`get_verification_status`)

**MCP 工具**: `get_verification_status`

**操作**: `getComputationState` + `getScheduledTasks` → 汇总。

**实际结果**:
```
EVA: computationState = "computed"
WP: scheduledTasks is object (contains active/done/procs/todo)
```

**断言**: EVA 状态为 `"computed"`，WP tasks 为 object。

---

## 3. MCP 工具覆盖矩阵

| # | MCP 工具 | 测试步骤 | 底层 API 调用 | 状态 |
|---|---------|---------|--------------|------|
| 1 | `reload_project` | A1 | `reloadFunctions` + `fetchFunctions` + `getFiles` | ✅ |
| 2 | `get_function_info` | A5 | `printDeclaration` | ✅ |
| 3 | `get_callgraph` | A3 | `callgraph.compute` + `getCallgraph` | ✅ |
| 4 | `run_eva` | B1 | `eva.general.compute` + `getComputationState` + `getProgramStats` | ✅ |
| 5 | `get_eva_alarms` | B2 | `reloadStatus` + `fetchStatus` + scope 过滤 | ✅ |
| 6 | `get_eva_value` | B4 | `eva.values.getValues(target=kinstr)` | ✅ |
| 7 | `run_wp` | C1 | `wp.setTimeout` + `printDeclaration` + `wp.startProofs` (×3) | ✅ |
| 8 | `get_verification_status` | D4 | `reloadStatus` + `fetchStatus` + `getComputationState` + `getScheduledTasks` | ✅ |
| 9 | `get_wp_goals` | C2 | `reloadGoals` + `fetchGoals` + scope/status 过滤 | ✅ |
| 10 | `get_current_annotations` | B6 | `reloadStatus` + `fetchStatus` + scope 过滤 | ✅ |
| 11 | `find_callers` | B3 | `eva.general.getCallers` | ✅ |
| 12 | `lookup_symbol` | A2, D1, D2 | `resolve_function` / `resolve_global` + refresh | ✅ |
| 13 | `trace_call_chain` | A4 | BFS on `get_callees` (callgraph 缓存) | ✅ |
| 14 | `investigate_alarm` | B5 | property lookup + `getValues` + `getCallers` + scope 过滤 | ✅ |
| 15 | `suggest_verification_plan` | D3 | 状态标志检查 + `reloadStatus` + `fetchStatus` | ✅ |

---

## 4. API 格式发现与修正

通过本次测试确认的 Frama-C Server API 实际格式（与设计文档差异已修正）:

### 4.1 fetchGoals 字段

| 设计文档假设 | 实际 API | 说明 |
|-------------|---------|------|
| `function` | `scope` | 函数声明 marker (`#F<vid>`) |
| `property` | `name` | 目标名称（如 `"Post-condition 'wrong'"`) |
| `status: "valid"` | `status: "VALID"` | **大写**，已用 `eq_ignore_ascii_case` 兼容 |
| — | `fct` | 函数名字符串（额外字段，用于显示） |

### 4.2 fetchGlobals 字段

| 设计文档假设 | 实际 API | 说明 |
|-------------|---------|------|
| `key: "kv#5"` | `key: "vi#25"` | marker 前缀为 `vi#` 不是 `kv#` |
| `decl: "#V5"` | `decl: "#G25"` | 声明前缀为 `#G` 不是 `#V` |

### 4.3 callgraph edge kind

| 设计文档假设 | 实际 API | 说明 |
|-------------|---------|------|
| `kind: "calls" / "called_by"` | `kind: "both" / "inter_functions"` | 方向由 `src→dst` 编码，不需 kind 过滤 |

### 4.4 EVA values getValues

| 场景 | kinstr | 实际返回 |
|------|--------|---------|
| 数组访问语句 (unsafe_read) | `#k38` | `{"vBefore": {"value": "UNINITIALIZED"}, "vAfter": {"value": "{0; 10; 20; 30}"}}` |
| 不可达语句 (unsafe_avg) | `#k43` | `{"vAfter": {"value": "Unreachable"}, "vBefore": {"value": "UNINITIALIZED"}}` |
| 循环头 (buf_sum) | `#k17` | `{}` (空对象 — 循环头无数据流) |
| `callstack` 参数 | — | 是 `param_opt`，省略时查合并值，传入时查特定调用栈 |

> 结论：查询有意义的值域需选择报警关联的 `kinstr`（从 `get_eva_alarms` 的 `kinstr` 字段获取），而非任意语句。

---

## 5. 已发现的 Bug 与修正

| # | 问题 | 发现步骤 | 修正 |
|---|------|---------|------|
| 1 | callgraph `get_callees` 返回空 | A3 (Phase 2 测试) | 移除 kind 过滤 — 方向由 src→dst 编码 |
| 2 | `get_wp_goals` 按 `function` 字段过滤不到目标 | C2 (Phase 2 测试) | 改为 `scope` 字段 |
| 3 | `get_wp_goals` 按 `"valid"` 过滤不到目标 | C2 (Phase 2 测试) | 状态为大写 `"VALID"`，改用 `eq_ignore_ascii_case` |
| 4 | `identity` 函数不产生 WP ensures 目标 | 早期设计 | 改为 `echo` 函数 + 命名 ensures (`correct:` / `wrong:`) 模式 |

---

## 6. 验证工作流总结

本测试证明：通过 15 个 MCP 工具，AI 智能体可以执行以下完整的形式验证工作流：

```
┌─────────────────────────────────────────────────────────────────┐
│  Phase A: 侦察                                                  │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────────────┐│
│  │ reload_project│ → │lookup_symbol │ → │ get_callgraph        ││
│  │ (9 functions) │   │ (4 globals)  │   │ (10 edges)           ││
│  └──────────────┘   └──────────────┘   └──────────────────────┘│
│         │                                        │              │
│         ▼                                        ▼              │
│  ┌──────────────┐                       ┌──────────────────────┐│
│  │get_function_ │                       │ trace_call_chain     ││
│  │info (AST)    │                       │ (4-level BFS)        ││
│  └──────────────┘                       └──────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Phase B: EVA 分析                                              │
│  ┌──────────┐   ┌──────────────┐   ┌──────────────────────────┐│
│  │ run_eva  │ → │get_eva_alarms│ → │ find_callers             ││
│  │ (49 props)│   │ (6 unknown)  │   │ (buf_get ← buf_sum)     ││
│  └──────────┘   └──────────────┘   └──────────────────────────┘│
│        │                │                                       │
│        ▼                ▼                                       │
│  ┌──────────────┐  ┌─────────────────┐  ┌─────────────────────┐│
│  │get_eva_value │  │investigate_alarm│  │get_current_          ││
│  │(kinstr→values)│  │(quick/normal/   │  │annotations           ││
│  │              │  │ deep)           │  │(buf_push: 16 annots) ││
│  └──────────────┘  └─────────────────┘  └─────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Phase C: WP 验证                                               │
│  ┌──────────────────────────┐   ┌──────────────────────────────┐│
│  │ run_wp                   │ → │ get_wp_goals                 ││
│  │ (echo + buf_push +       │   │ (10 goals: 9 VALID +        ││
│  │  buf_get)                │   │  1 NORESULT on echo 'wrong') ││
│  └──────────────────────────┘   └──────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Phase D: 评估                                                  │
│  ┌──────────────┐   ┌──────────────────────────────────────────┐│
│  │lookup_symbol │ → │ suggest_verification_plan                ││
│  │(fn + global) │   │ (EVA+WP done → review results)          ││
│  └──────────────┘   └──────────────────────────────────────────┘│
│                              │                                  │
│                              ▼                                  │
│                     ┌──────────────────────────────────────────┐│
│                     │ get_verification_status                  ││
│                     │ (39 valid, 5 unknown, 10 never_tried,    ││
│                     │  1 invalid_under_hyp)                    ││
│                     └──────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

---

## 7. 完整测试清单

| 类别 | 测试数 | 通过 | 失败 |
|------|--------|------|------|
| 单元测试 (`cargo test --lib`) | 28 | 28 | 0 |
| 离线集成测试 | 4 | 4 | 0 |
| Phase 1 集成测试 (`test_full_workflow`, 14 步) | 1 | 1 | 0 |
| Phase 2 集成测试 (`test_phase2_workflow`, 13 步) | 1 | 1 | 0 |
| 综合工作流测试 (`test_comprehensive`, 19 步) | 1 | 1 | 0 |
| 迭代工作流测试 (`test_iterative_workflow`, 5 阶段) | 1 | 1 | 0 |
| **合计** | **36** | **36** | **0** |

> 注：4 个 live-server 测试各需独立的 Frama-C Server 实例，需分别启动后运行。

---

## 8. 迭代验证工作流测试

> 测试日期：2026-02-21
> 测试文件：`test/test_iterative_raw.c`
> 测试入口：`tests/integration_test.rs::test_iterative_workflow`

### 8.1 测试概述

模拟 AI 智能体的核心使用场景：拿到裸 C 文件 → EVA 发现报警 → 注入 ACSL 注解 → 重新加载 → WP 证明注解正确。

| 项目 | 值 |
|------|---|
| 测试用 C 文件 | `test/test_iterative_raw.c` — 裸 C (无 ACSL) + 运行时写入注解版 |
| Frama-C Server | `frama-c test/test_iterative_raw.c -server-socket /tmp/frama-c-test-iter.sock` |
| 测试结果 | **PASS** — 5 个阶段全部通过 |
| 耗时 | ~6 秒 |
| 关键验证 | 文件修改 + AST 重载 + EVA/WP 状态正确刷新 |

### 8.2 测试 C 文件设计

```
裸 C 版 (初始):
  safe_div(a, b)   — return a / b (无 ACSL → EVA 报 division_by_zero)
  array_read(idx)  — return arr[idx] (无 ACSL → EVA 报 index_bound)
  main             — volatile int nondet 驱动非确定性输入

注解版 (运行时覆写):
  safe_div   — requires b != 0; assigns \nothing; ensures \result == a / b;
  array_read — requires 0 <= idx < SIZE; assigns \nothing; ensures \result == arr[idx];
  main       — 不变
```

`volatile int nondet` 关键作用：阻止 EVA 用常量传播消除报警。若 main 用 `safe_div(100, 5)` 等常量调用，EVA 能静态证明安全，不会产生属性。

### 8.3 阶段详情

#### Phase 1: 裸 C → EVA 报警

| 步骤 | 操作 | 结果 |
|------|------|------|
| 1.1 | 连接 + 加载函数 | 3 functions: safe_div, array_read, main |
| 1.2 | 运行 EVA | computed |
| 1.3 | 获取报警 | 5 properties, 5 non-valid |

```
safe_div:    2 non-valid (division_by_zero + signed_overflow)
array_read:  2 non-valid (index_bound)
main:        1 non-valid
```

#### Phase 2: 注入 ACSL → 重新加载

| 步骤 | 操作 | 结果 |
|------|------|------|
| 2.1 | `std::fs::write` 覆写注解版 | 文件含 "requires" |
| 2.2 | AST 重载 (setFiles→compute) | 成功 |
| 2.3 | 刷新函数列表 | 3 functions, EVA/WP invalidated |
| 2.4 | printDeclaration 检查 | ACSL "requires" 出现在声明中 |

**AST 重载关键发现**: `kernel.ast.setFiles` 同值不触发依赖失效。必须先 `setFiles([])` 再 `setFiles(原始列表)` 才能迫使 AST 重新解析。这是因为 Frama-C 的状态依赖系统基于值变化检测。

#### Phase 3: 重新 EVA → 报警变化

| 步骤 | 操作 | 结果 |
|------|------|------|
| 3.1 | 运行 EVA | computed |
| 3.2 | 获取报警 | 13 properties, 13 non-valid |

属性数增多是因为注解版引入了用户定义的 requires/ensures/assigns 属性（status=never_tried），加上 EVA 自动生成的 RTE 检查。

#### Phase 4: WP 证明 → 注解正确

| 步骤 | 操作 | 结果 |
|------|------|------|
| 4.1 | WP on safe_div | startProofs 成功 |
| 4.1 | WP on array_read | startProofs 成功 |
| 4.2 | 获取 WP goals | 6 goals: 5 VALID + 1 NORESULT |

```
Goals by status:
  VALID:     5 (ensures + assigns 证明)
  NORESULT:  1 (可能的 signed_overflow 相关目标)
```

#### Phase 5: 最终验证 + 清理

| 步骤 | 操作 | 结果 |
|------|------|------|
| 5.1 | 验证状态 | EVA=computed, WP tasks available, 3 functions |
| 5.2 | 服务器关闭 | shutdown 成功 |
| — | FileRestoreGuard::drop | 文件恢复为裸 C 版 |

### 8.4 技术发现

1. **AST 重载方法**: `setFiles([])` + `setFiles(files)` + `compute` — 必须通过值变化触发 Frama-C 状态依赖系统的 AST 失效。直接 `setFiles(同值)` + `compute` 是 no-op。

2. **volatile 非确定性**: `volatile int nondet` 是让 EVA 在简单测试文件上产生报警的关键。无 volatile 时，EVA 常量传播会消除所有警告。

3. **文件恢复安全**: `FileRestoreGuard` (RAII) 确保测试 panic 时也能恢复原始文件内容，防止 Git 工作区污染。

4. **Frama-C Server 无 `Ast.mark_as_changed()` 暴露**: 服务器协议不直接暴露 `Ast.mark_as_changed()` 或 `Project.clear()` 端点。`kernel.project.create` 是创建全新项目的替代方案，但 `setFiles` 值变化法更简洁。
