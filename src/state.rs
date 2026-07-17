use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use serde::{Serialize, Deserialize};
use tokio::sync::Mutex as AsyncMutex;
use tokio::process::Child;

/// State of a sandbox Frama-C instance.
///
/// `sandbox_child` 持 tokio Child 句柄供 cleanup_sandbox 调用 `child.wait().await`
/// 显式 reap，否则 frama-c 子进程退出时会变 zombie（参 docs/fixes/
/// frama-c-mcp-fix-child-reap-broken-pipe.md）。Arc<Mutex<Option<Child>>>
/// 兼容 SandboxState 的 Clone 语义（Arc 共享句柄，cleanup 时 take Option）。
#[derive(Debug, Clone)]
pub struct SandboxState {
    pub experiment_id: String,
    /// Original function name in the main project
    pub original_function: String,
    /// Temp directory for sandbox files
    pub sandbox_dir: PathBuf,
    /// Socket path for sandbox Frama-C
    pub sandbox_socket: PathBuf,
    /// PID of sandbox Frama-C process（仅供日志 / debug；实际生命周期管理走 sandbox_child）
    pub sandbox_pid: u32,
    /// tokio Child 句柄（drop 时 kill_on_drop 兜底；cleanup_sandbox 应主动 wait reap）
    pub sandbox_child: Arc<AsyncMutex<Option<Child>>>,
    /// Statement ID mapping: (orig_sid, sandbox_sid)
    pub sid_map: Vec<(i64, i64)>,
    /// Sandbox function's declaration marker (e.g. "#F48"), cached at creation
    pub declaration_marker: String,
}

#[derive(Debug, Default)]
pub struct SessionState {
    pub project_loaded: bool,
    pub eva_completed: bool,
    pub wp_completed: bool,
    pub functions: HashMap<String, FunctionInfo>,
    // --- Phase 2 ---
    pub globals: HashMap<String, GlobalInfo>,
    pub callgraph_edges: Vec<CallEdge>,
    pub callgraph_vertices: Vec<CallVertex>,
    // --- Skill-based verification ---
    pub conclusions: HashMap<String, FunctionVerificationState>,
    pub project_state: Option<ProjectVerificationState>,
}

// --- Verification state types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionVerificationState {
    pub function: String,
    pub status: VerificationStatus,
    // NOTE: 长文本字段（semantic_proof / semiformal_proof / program_summary）
    // 已从 in-memory struct 删除——它们的真相**仅在磁盘** `.frama-c-mcp/<func>/*.md`。
    // - 写入：caller 用 Write 工具直接写 `.md`（store API 不接受这些字段）
    // - 读取：MCP handler 在 get_function_conclusion 时从磁盘读 + 组装 JSON 响应
    // - 见 docs/fixes/conclusion-per-field-files.md Plan A 设计
    // - 解决了 in-memory vs disk desync 导致的 persist 误删 bug
    // - `analysis_summary` 历史上是第 4 个长文本字段，2026-05-26 因撞 CC subagent guard 删除，
    //   内容并入 semiformal_proof.md 的 ## function_summary section
    //   （见 docs/fixes/rename-analysis-summary-subagent-guard.md）
    /// 已 commit 的规约（含 hash_label / kind / acsl / stmt_id / wp_status / derived_from / merged_at 等）
    pub specs: Vec<AnnotationEntry>,
    /// 元信息：曾经设计但 degrade 到 reference 的规约（不进 verified.c，不影响 status）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_specs: Vec<AnnotationEntry>,
    /// status="unsound" 时填：被移除的 unsound 规约 + 反例
    /// hard check 用 jq -e '.unsound_specs | type == "array"' 校验，必须存在
    #[serde(default)]
    pub unsound_specs: Vec<UnsoundSpec>,
    /// Per-goal WP results
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wp_results: Vec<WpGoalResult>,
    /// Aggregate WP summary
    pub wp_summary: Option<WpGoalSummary>,
    /// Free-form notes
    pub notes: String,

    // --- S1_info_gather 输出 ---
    // 这三个字段不能用 skip_serializing_if：S1 hard check 要求 .callees / .callee_info /
    // .existing_asserts 字段在持久化 JSON 里必须存在（即使为空），叶函数 callees=[] 也要写。
    /// callee 名字列表
    #[serde(default)]
    pub callees: Vec<String>,
    /// callee 状态 + sources
    #[serde(default)]
    pub callee_info: HashMap<String, CalleeInfo>,
    /// 源码已存在的 assert（不进 specs，但 SP 推导按 stmt fact 处理）
    #[serde(default)]
    pub existing_asserts: Vec<ExistingAssert>,

    // --- S2.5 step 12 structured spec proposals (S3 ground truth) ---
    // proposed_* 字段不能用 skip_serializing_if：S2.5 hard check
    // proof_review_complete.sh 要求 .proposed_behaviors/.proposed_requires/
    // .proposed_ensures/.proposed_assigns/.proposed_loop_annots 字段在
    // 持久化 JSON 里**必须存在**（即使为空数组），简单函数（无循环 / 无显式 requires）
    // 也得明示空数组。
    //
    // schema v2（rmcp 1.7 升级 + PR #91 B 重设计）：
    //   - proposed_behaviors: 顶层 behavior 声明（name + assumes），其余字段按
    //     `behavior: "<name>"` 引用，避免 assumes 重复。
    //   - proposed_assigns: 改 Vec<ProposedAssigns>（之前是 Option<String>，仅
    //     表达单条 top-level assigns），现在支持多条 + behavior 引用。
    //   - proposed_loop_annots[i].{invariants,assigns,variant} 升级到带
    //     behavior 字段的 typed struct。
    #[serde(default)]
    pub proposed_behaviors: Vec<ProposedBehavior>,
    #[serde(default)]
    pub proposed_requires: Vec<ProposedRequires>,
    #[serde(default)]
    pub proposed_ensures: Vec<ProposedEnsures>,
    #[serde(default)]
    pub proposed_assigns: Vec<ProposedAssigns>,
    #[serde(default)]
    pub proposed_loop_annots: Vec<ProposedLoopAnnot>,
    // 函数级 terminates clause（单值、可空，仅 prove_termination=false 函数有）：
    // {acsl: "terminates \\false", derived_from: "termination_waived"}。fsmint-6：S_prepare 入列，
    // inject_all 在 sandbox/main 两处用同一字段注入 → 自然 merge。不是 array，不受上面 jq array 校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_terminates: Option<serde_json::Value>,

    // 以下字段同样被 hard check 的 jq -e '.X | type == "array"' 校验，
    // 不能 skip_serializing_if（即使空也要写入 JSON）。
    // --- callee_gap 路径产出 ---
    #[serde(default)]
    pub callee_requests: Vec<CalleeRequest>,

    // --- Revision counters (替代 sp_revision_history / proposed_revision_history Vec) ---
    // 旧设计存完整 SpRevisionRecord Vec（含 markdown sp_diff），现简化为：
    //   - count: 防 runaway loop（hard check 限 ≤ N 轮）
    //   - last_*_error_analysis: LLM 下轮 prompt context（"上次为啥失败"），每轮覆盖
    // 历史 sp_diff 不再保留：让用户把 .frama-c-mcp/ 纳入 git，git log + git show 任意 SHA
    // 即可看到当时的 semantic_proof.md 内容。
    #[serde(default)]
    pub sp_revision_count: u32,
    #[serde(default)]
    pub last_sp_error_analysis: String,
    #[serde(default)]
    pub proposed_revision_count: u32,
    #[serde(default)]
    pub last_proposed_error_analysis: String,

    // --- failure 路径（status=failed/unsound） ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_evidence: Option<FailureEvidence>,

    // --- S5_output ---
    /// .frama-c-mcp/verified/<func>_verified.c 路径（status=verified/failed/unsound 时非空）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_source: Option<String>,

    // --- verify-program-fsm v1 接入 (detailed-design §6.4) ---
    //
    // unsound_reason_type: status=unsound 时的子分类，让上层主 FSM 协调
    // (architecture §5.5 + detailed-design §6.4).
    //   "internal_logic_error" — caller 自身 SP 推导错 (caller bug)
    //   "callee_requires_too_strict" — caller 加强 requires 后某 ensures 不可证，
    //                                  根因 callee requires 太严 (主 FSM 启动 weak_requires 流程)
    //   "other" — 其他
    /// status="unsound" 时填；其他 status 应为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsound_reason_type: Option<String>,

    /// 仅 unsound_reason_type="callee_requires_too_strict" 时填，
    /// 描述 caller 发现的 blocking_requires + caller_state_at_site + evidence。
    /// 主 FSM 读此字段启动 weak_requires 流程，把内容转为 callee 的
    /// weak_requires_request const_var。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_callee_requires: Option<BlockingCalleeRequires>,

    /// status="caller_request_infeasible" 时填 (callee 拒绝主 FSM 的 weak_requires_request):
    /// 描述 callee 拒绝弱化 requires 的 UB 反例。
    /// 主 FSM 读此字段标 caller 为 permanent_blocked + 记 program 级
    /// failure_evidence.caller_request_infeasible。
    #[serde(default)]
    pub infeasible_requests: Vec<InfeasibleRequest>,

    // --- 跨 FSM 注入 ---
    // 注：program_summary 长文本字段已从 in-memory struct 删除（Plan A），
    // 真相在 `.frama-c-mcp/<func>/program_summary.md`，MCP handler 直接读写文件。

    // --- sandbox 状态字段（create_sandbox / add_annotation / reset / delete 副作用维护） ---
    /// sandbox 中 sallstmts 的数量（从 create_sandbox 提取的 AST 信息）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ast_stmt_count: Option<u32>,
    /// sandbox 是否处于"reset 后未 add 任何注解"的干净状态
    #[serde(default = "default_true")]
    pub sandbox_clean: bool,
    /// 当前 sandbox 上累计 add 的注解数（reset_sandbox 时清零）
    #[serde(default)]
    pub annotation_count: u32,
    /// sandbox 是否已 delete（S5_output 末尾置 true）
    #[serde(default)]
    pub sandbox_deleted: bool,

    // --- audit ---
    /// 关键节点快照（revision 前 / clear_specs 前）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conclusion_history: Vec<FunctionVerificationState>,
}

/// serde default helper：bool 字段默认 true
fn default_true() -> bool { true }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    InProgress,
    Verified,
    /// WP 工具能力不足（数学规约正确，工具证不了，如 \\freeable 不支持）
    Failed,
    /// 真 UB 风险（数学上规约站不住）
    Unsound,
    /// callee.ensures 不够强；F 自身无法补
    BlockedOnCallee,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationEntry {
    /// MCP auto-generated unique hash (e.g. "li_a3f2"), always present
    pub hash_label: String,
    /// Agent-provided semantic label (e.g. "bounds"), optional
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_label: Option<String>,
    /// 顶层二分类（design + hard check 一致）：
    /// - "spec"  = function-level（requires / ensures / assigns），stmt_id 必须为 null
    /// - "annot" = stmt-level（loop_invariant / loop_assigns / loop_variant / assert），stmt_id 必填
    /// 细粒度 ACSL 类型（requires/ensures/loop_invariant/...）从 acsl 文本起始关键字推断，
    /// 或从 derived_from 反推（"proposed_requires[i]" → requires）。
    pub kind: String,
    /// ACSL text (expression only, no label)
    pub acsl: String,
    /// Associated stmt ID (kind="annot" 必填；kind="spec" 必为 null)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stmt_id: Option<i64>,
    /// 必填：本 spec 来源标识，匹配 hard check acsl_validated.sh 的正则：
    ///   ^proposed_(requires|ensures|assigns|loop_annots\[\d+\]\.(invariants\[\d+\]|assigns|variant))(\[\d+\])?$
    /// 或 "remediation:..." 起始（S4 bridge / degrade 路径）。
    pub derived_from: String,
    /// Who created this annotation
    pub source: AnnotationSource,
    /// Why this annotation exists
    pub purpose: String,
    /// Hash label of the main spec this auxiliary spec supports (for commit gating)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_target: Option<String>,
    /// WP status: "valid" | "unknown" | "timeout" | "noresult" | null
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wp_status: Option<String>,
    /// Proof time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wp_time_ms: Option<u32>,
    /// Prover used: "Qed" | "Alt-Ergo" | "z3" | etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wp_prover: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationSource {
    /// Already in source code
    Original,
    /// Generated by skill and committed
    Generated,
    /// Candidate not yet proven
    Reference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WpGoalResult {
    /// WP goal name
    pub name: String,
    /// "VALID" | "UNKNOWN" | "TIMEOUT" | "NORESULT"
    pub status: String,
    /// Prover that produced this result
    pub prover: Option<String>,
    /// Proof time in milliseconds
    pub time_ms: Option<u32>,
    /// Hash label of the related AnnotationEntry
    pub related_spec: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WpGoalSummary {
    pub total: u32,
    pub valid: u32,
    pub unknown: u32,
    pub timeout: u32,
    pub failed: u32,
    /// "Typed+nocast" | "Bytes"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 实际用的 timeout 秒数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_used: Option<u32>,
    /// 写入时 cegis_attempts_count 的快照（首次为 0）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_retry: Option<u32>,
    /// 失败的 spec goals 的 hash_label（来自 conclusion.specs）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_goal_labels: Vec<String>,
    /// 失败的源码 assert / RTE goals（不在 specs 里）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_source_asserts: Vec<FailedSourceAssert>,
}

/// 失败的源码 assert / RTE goal（区别于 spec 的 hash_label）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedSourceAssert {
    pub stmt_id: u32,
    pub acsl: String,
    /// "user_assert" | "rte_overflow" | "rte_bound" | "rte_division" | "rte_pointer" | "rte_shift"
    pub kind: String,
}

// --- New structs for 11-round design refinements ---

/// status="unsound" 时被移除的 unsound 规约（含反例信息）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsoundSpec {
    pub hash_label: String,
    pub kind: String,
    pub acsl: String,
    pub stmt_id: Option<u32>,
    /// 反例 / WP counterexample 描述
    pub counterexample: String,
    /// 移除时机（cegis_attempts_count 快照）
    pub removed_at_retry: Option<u32>,
}

/// status ∈ {failed, unsound} 时填，描述死路根因
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEvidence {
    /// "source_assert" | "callee_requires" | "proposed_requires" | "rte_*" | etc.
    #[serde(rename = "type")]
    pub failure_type: String,
    /// "<file:line>" 或 "<stmt_id>" 或 "<hash_label>"
    pub location: String,
    /// 失败的 ACSL 表达式
    pub acsl: String,
    /// WP 不支持的谓词名（如 "\\freeable"）；可选
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_predicate: Option<String>,
    /// LLM 试过的等价表达 reformulate（最终都失败）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempted_reformulations: Vec<String>,
    /// counterexample / 反例（unsound 类必填）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterexample: Option<String>,
}

/// 跨函数依赖：F 的 callee 列表 + 状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalleeInfo {
    /// "verified" | "failed" | "unsound" | "blocked_on_callee" | "in_progress"
    pub status: String,
    /// 来自 caller 抓取或 verify-program 注入的 callee context
    pub sources: CalleeSources,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalleeSources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_spec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_proof: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semiformal_proof: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ast: Option<String>,
}

/// callee_gap 路径产出：要求 caller 强化 callee.ensures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalleeRequest {
    pub callee: String,
    /// 具体 ACSL 性质，或 NL 描述
    pub required_property: String,
    /// 哪一步 SP 推导 / 哪个 ensures 的证明依赖它
    pub reason: String,
}

/// status="unsound" + unsound_reason_type="callee_requires_too_strict" 时附带。
/// caller 自身 SP 推导发现 callee.requires 不能满足，加强 caller.requires 后某
/// ensures 不可证 → 报告给主 FSM 启动 weak_requires 流程。
/// 详见 architecture.md §5.5 / detailed-design.md §6.4。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockingCalleeRequires {
    /// callee 函数名（caller 调用的对象）
    pub callee: String,
    /// callee 现有的、caller 不能满足的具体 requires (例 "b < 100")
    pub blocking_requires: String,
    /// caller 在调用点的 SP-state 描述 (例 "b 可能 ≥ 100")
    pub caller_state_at_site: String,
    /// caller 加强 requires 后某 ensures 不可证的具体路径 (NL)
    pub strengthening_attempt: String,
    /// 具体证据 (NL) — SP 推导路径 + 不可证 ensures 名
    pub evidence: String,
}

/// status="caller_request_infeasible" 时附带：
/// callee 收到主 FSM weak_requires_request 后重审，发现 blocking_requires
/// 是 UB-必要不能弱化 → 用此字段拒绝。详见 architecture.md §5.8。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfeasibleRequest {
    /// 哪个 caller 提的弱化请求
    pub caller: String,
    /// "weaken_requires" (v1 只有这一种，预留 enum)
    pub rejected_request_type: String,
    /// 被拒弱化的具体 requires
    pub rejected_requires: String,
    /// UB 反例描述 (NL, hard check 要求 ≥ 50 bytes)
    pub infeasibility_reason: String,
}

/// S1 提取的源码已存在的 assert（非 LLM 设计的 spec）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistingAssert {
    pub stmt_id: u32,
    pub acsl: String,
    /// "source" | "user_added" | "rte_injected"（verify-program 阶段注入的 RTE）
    pub origin: String,
}

// --- S2.5 structured spec proposals (ground truth, S3 直接消费) ---
//
// schema v2（PR #91 B 重设计，2026-05-24）：
//   - 顶层加 `proposed_behaviors`：一次声明 behavior name + assumes，其余 clause
//     按 `behavior: "<name>"` 引用，避免每 entry 重复携带 assumes。
//   - requires / ensures / assigns / loop_invariants / loop_assigns / loop_variant
//     都加可选 `behavior` 字段（引用 proposed_behaviors[i].name；
//     未提供 → 默认 / top-level）。
//   - inject_all_annotations_sandbox 拼装 ACSL 时按引用查表生成
//     "behavior X: assumes A; <clause>;"；未声明引用 → 报错（type=ProposedError）。
//
// ACSL 参考：函数级 behavior 内 requires/assigns/ensures 都合法（§2.3.2）；
// loop annotation 用 "for X: loop invariant ..." 语法（§2.4.2）。

/// 命名 behavior 声明 — assumes clauses 提到顶层，避免每 entry 重复携带。
/// 引用方在 ProposedRequires/Ensures/Assigns/ProposedLoop* 的 `behavior` 字段
/// 写 `name` 来挂入此 behavior。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedBehavior {
    /// behavior 名（必须是合法 C 标识符）。
    pub name: String,
    /// assumes clauses（多个 AND）。空 / 缺省 → 等价 ACSL 的 `assumes \true`
    /// （named behavior 但总是适用）。
    #[serde(default)]
    pub assumes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedRequires {
    /// bare ACSL predicate, 不含 `requires` 关键字、不含分号。
    pub acsl: String,
    /// 引用 proposed_behaviors[i].name；None → 默认（top-level）behavior。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
    /// 反例式必要性论证（"该条不成立时函数有 UB 或违反 spec"）。仅 metadata。
    pub necessity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedEnsures {
    /// bare ACSL predicate。
    pub acsl: String,
    /// 引用 markdown 节，如 "step 8 path-1"。仅 metadata。
    pub from: String,
    /// 引用 proposed_behaviors[i].name；None → 默认 behavior。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
}

/// 函数级 assigns clause。schema v2 起为 Vec — 之前是 Option<String>（单条），
/// 现在支持多条 + behavior 引用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedAssigns {
    /// bare assigns 内容（如 "*p, a[0..n-1]"），不含 `assigns` 关键字、不含分号。
    pub acsl: String,
    /// 引用 proposed_behaviors[i].name；None → 默认 behavior。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
}

/// loop invariant — schema v2 起带可选 behavior 引用（生成 `for X: loop invariant ...`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedLoopInvariant {
    pub acsl: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedLoopAssigns {
    pub acsl: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedLoopVariant {
    pub acsl: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedLoopAnnot {
    /// sandbox 里 loop stmt 的 sid（S2.5 调 get_function_ast 已查得）。
    pub stmt_id: u32,
    /// 人类可读注释，S3 不消费。
    pub loop_label: String,
    /// loop invariants — schema v2 起从 Vec<String> 升级到 typed struct。
    pub invariants: Vec<ProposedLoopInvariant>,
    /// loop assigns — schema v2 起从单 String 升级到 Vec<typed>，支持多条 + behavior。
    pub assigns: Vec<ProposedLoopAssigns>,
    /// loop variant — schema v2 起从单 String 升级到 Option<typed>（loop variant 至多 1 条）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<ProposedLoopVariant>,
}

// --- Project-level state additions (verify-program 用，向后兼容旧 ProjectVerificationState) ---
//
// 旧 4 字段 (source_files / verification_order / current_index / global_notes) 保留兼容
// thin variant; verify-program-fsm v1 用新字段 (levels / scc_groups / completion_map 等).
// 详见 docs/design/verify-program-fsm/{architecture.md §3.1, detailed-design.md §5.2.1}.

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectVerificationState {
    // 旧字段 (thin variant 兼容)
    pub source_files: Vec<String>,
    // VO-completeness fix（2026-06）：verification_order 升为 **server-owned**——
    // compute_topological_order 从 callgraph **defined** 函数 seed in-memory + full-replace 保留，
    // agent 不再 build → 完整性按构造成立（无 mis-build / 无漏函数静默跳过）。
    #[serde(default)]
    pub verification_order: Vec<String>,
    pub current_index: usize,
    pub global_notes: String,

    // S1 输出
    // P2 方案 A：活字段（source_files/completion_map.status…）去 #[serde(default)] → 必填，
    // full-replace 下漏写 loud-fail，不静默抹历史。
    // server-owned 例外（调用图派生，compute_topological_order seed in-memory + full-replace 保留，
    // 仿 locked；LLM 不再填）：verification_order(上) / current_level / scc_groups。
    // #112 的 `levels` 字段已**彻底删除**（VO-completeness fix）——分层信息由 scc_groups.level 携带；
    // `Level` 类型保留（topo.rs 算法返回值，非持久化字段）。
    #[serde(default)]
    pub current_level: usize,
    #[serde(default)]
    pub scc_groups: Vec<SccGroup>,

    // S2 维护
    pub completion_map: HashMap<String, FunctionCompletion>,
    pub feedback_pending: HashMap<String, FeedbackPayload>,

    // SCC 振荡检测
    pub scc_iteration_counters: HashMap<u32, u32>,
    pub scc_spec_hashes: HashMap<u32, Vec<String>>,

    // 全局进展检测
    pub last_spec_hash: HashMap<String, String>,
    pub last_progress_snapshot: ProgressSnapshot,

    // 失败追踪
    #[serde(default)]
    pub failure_evidence: Option<ProgramFailureEvidence>,
    #[serde(default)]
    pub final_gate_result: Option<FinalGateResult>,
    #[serde(default)]
    pub merged_source: Option<String>,

    // 辅助 hard check (lock_project/unlock_project 同步维护)
    #[serde(default)]
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Level {
    #[serde(default)]
    pub level: usize,
    pub groups: Vec<SccGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SccGroup {
    pub id: u32,
    pub members: Vec<String>,
    #[serde(default)]
    pub level: usize,
    pub is_cycle: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCompletion {
    /// 8 种枚举: pending / pending_redispatch / completed / failed_merged /
    ///   unsound / extraction_issue / permanent_blocked / not_reached
    pub status: String,
    pub last_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    pub verified_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeedbackPayload {
    #[serde(default)]
    pub caller_requests: Vec<ProgramCallerRequest>,
    #[serde(default)]
    pub weak_requires_request: Option<WeakRequiresRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramCallerRequest {
    pub caller: String,
    pub required_ensures: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeakRequiresRequest {
    pub caller: String,
    pub blocking_requires: String,
    pub caller_state_at_site: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProgressSnapshot {
    #[serde(default)]
    pub completion_counts: HashMap<String, usize>,
    #[serde(default)]
    pub pending_redispatch_specs: HashMap<String, String>,
    #[serde(default)]
    pub feedback_queue_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProgramFailureEvidence {
    /// 6 种枚举: load_failed / dispatch_failed / level_unrecoverable /
    ///   global_stuck / unlock_export_failed / final_gate_failed
    pub failure_type: String,
    pub details: String,
    #[serde(default)]
    pub attempted_operations: Vec<String>,
    #[serde(default)]
    pub unsound_specs: Vec<UnsoundRecord>,
    #[serde(default)]
    pub caller_request_infeasible: Vec<InfeasibleRecord>,
    #[serde(default)]
    pub scc_oscillating: Vec<SccOscillatingRecord>,
    #[serde(default)]
    pub stuck_triples: Vec<StuckTriple>,
    #[serde(default)]
    pub failed_funcs: Vec<FinalGateFailedFunc>,
    #[serde(default)]
    pub wp_invocation_errors: Vec<String>,
    #[serde(default)]
    pub last_completed_level: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnsoundRecord {
    pub function: String,
    pub unsound_specs: Vec<serde_json::Value>,
    pub counterexample: String,
    pub unsound_reason_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfeasibleRecord {
    pub caller: String,
    pub callee: String,
    pub rejected_request_type: String,
    pub rejected_requires: String,
    pub infeasibility_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SccOscillatingRecord {
    pub scc_id: u32,
    pub members: Vec<String>,
    pub hash_history: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StuckTriple {
    pub caller: String,
    pub callee: String,
    pub request_type: String,
    pub blocking_requires_or_required_ensures: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalGateFailedFunc {
    pub function: String,
    #[serde(default)]
    pub goals_failed: Vec<serde_json::Value>,
    #[serde(default)]
    pub goals_timeout: Vec<serde_json::Value>,
    #[serde(default)]
    pub goals_unknown: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalGateResult {
    pub total: usize,
    pub valid: usize,
    pub unknown: usize,
    pub timeout: usize,
    pub failed: usize,
    pub passed: bool,
}

/// Upsert input for store_conclusion: all fields optional except function.
/// 任何 None 字段保持原值（merge 语义）。
///
/// **Plan A 收尾**（见 docs/fixes/remove-store-long-text-fields.md）：
/// 长文本字段（semantic_proof / semiformal_proof / program_summary）
/// **已从本 struct 删除**。这些字段的真相**只**在 `.frama-c-mcp/<func>/*.md` 文件，
/// caller 用 Write 工具直接写。store_function_conclusion 仅处理短/结构化字段。
/// 注：`analysis_summary` 历史上是第 4 个长文本字段，已删除（见
/// docs/fixes/rename-analysis-summary-subagent-guard.md）。
#[derive(Default)]
pub struct FunctionConclusionUpdate {
    pub function: String,
    pub status: Option<VerificationStatus>,
    pub specs: Option<Vec<AnnotationEntry>>,
    pub reference_specs: Option<Vec<AnnotationEntry>>,
    pub unsound_specs: Option<Vec<UnsoundSpec>>,
    pub wp_results: Option<Vec<WpGoalResult>>,
    pub wp_summary: Option<WpGoalSummary>,
    pub notes: Option<String>,

    // S1
    pub callees: Option<Vec<String>>,
    pub callee_info: Option<HashMap<String, CalleeInfo>>,
    pub existing_asserts: Option<Vec<ExistingAssert>>,

    // S2.5 step 12 — schema v2: behaviors 顶层 + assigns Vec
    pub proposed_behaviors: Option<Vec<ProposedBehavior>>,
    pub proposed_requires: Option<Vec<ProposedRequires>>,
    pub proposed_ensures: Option<Vec<ProposedEnsures>>,
    pub proposed_assigns: Option<Vec<ProposedAssigns>>,
    pub proposed_loop_annots: Option<Vec<ProposedLoopAnnot>>,
    /// fsmint-6: 函数级 terminates waiver（{acsl, derived_from}）。S_prepare 经
    /// store_function_conclusion 写入；缺省 None（true 模式 / 未设）不改动既有值。
    pub proposed_terminates: Option<serde_json::Value>,

    // callee_gap
    pub callee_requests: Option<Vec<CalleeRequest>>,

    // Revision counters（替代 sp_revision_history / proposed_revision_history Vec）
    pub sp_revision_count: Option<u32>,
    pub last_sp_error_analysis: Option<String>,
    pub proposed_revision_count: Option<u32>,
    pub last_proposed_error_analysis: Option<String>,

    // failure
    pub failure_evidence: Option<FailureEvidence>,

    // S5
    pub verified_source: Option<String>,

    // verify-program-fsm v1 接入 (detailed-design §6.4)
    pub unsound_reason_type: Option<String>,
    pub blocking_callee_requires: Option<BlockingCalleeRequires>,
    pub infeasible_requests: Option<Vec<InfeasibleRequest>>,

    // cross-FSM: program_summary 不在 update struct（Plan A），handler 直接写 .md 文件


    /// 特殊：true 时把当前 conclusion 快照 push 到 conclusion_history（不 set 其他字段）
    pub push_history: bool,
}

/// Summary returned by list_conclusions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConclusionSummary {
    pub function: String,
    pub status: VerificationStatus,
    pub wp_summary: Option<WpGoalSummary>,
}

/// Upsert input for store_project_state: all fields optional.
#[derive(Default)]
pub struct ProjectStateUpdate {
    // 旧字段 (thin variant 兼容)
    pub source_files: Option<Vec<String>>,
    pub verification_order: Option<Vec<String>>,
    pub current_index: Option<usize>,
    pub global_notes: Option<String>,

    // verify-program-fsm v1 新字段 (详见 ProjectVerificationState)
    // `levels` 字段已删（VO-completeness fix）——server-owned，agent 不提交。
    pub current_level: Option<usize>,
    pub scc_groups: Option<Vec<SccGroup>>,
    pub completion_map: Option<HashMap<String, FunctionCompletion>>,
    pub feedback_pending: Option<HashMap<String, FeedbackPayload>>,
    pub scc_iteration_counters: Option<HashMap<u32, u32>>,
    pub scc_spec_hashes: Option<HashMap<u32, Vec<String>>>,
    pub last_spec_hash: Option<HashMap<String, String>>,
    pub last_progress_snapshot: Option<ProgressSnapshot>,
    pub failure_evidence: Option<ProgramFailureEvidence>,
    pub final_gate_result: Option<FinalGateResult>,
    pub merged_source: Option<String>,
    pub locked: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub name: String,
    pub marker: String,
    pub declaration: String,
    pub signature: String,
    pub file: String,
    pub line: u32,
    /// fetchFunctions `"defined"`：有定义（可验证）vs library declared-only（不验证）。
    /// 供 compute_topological_order 过滤 verification_order 只含 defined 函数（VO-completeness fix）。
    pub defined: bool,
}

#[derive(Debug, Clone)]
pub struct GlobalInfo {
    pub name: String,
    pub marker: String,       // e.g. "vi#25"
    pub declaration: String,  // e.g. "#G25"
    pub typ: String,          // e.g. "int"
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub struct CallEdge {
    pub src: String,   // declaration marker, e.g. "#F36"
    pub dst: String,   // declaration marker, e.g. "#F24"
    pub kind: String,  // "both", "calls", "called_by"
}

#[derive(Debug, Clone)]
pub struct CallVertex {
    pub name: String,        // function name
    pub declaration: String, // declaration marker, e.g. "#F36"
}

impl SessionState {
    /// Populate the functions cache from `fetchFunctions` response entries.
    ///
    /// Actual Frama-C Server JSON format (verified by integration test):
    /// ```json
    /// {
    ///   "name": "abs_val",
    ///   "key": "kf#24",          // function marker
    ///   "decl": "#F24",          // declaration marker (for printDeclaration)
    ///   "signature": "int abs_val(int x);",
    ///   "defined": true,
    ///   "sloc": {                // source location is a nested object
    ///     "file": "/path/to/file.c",
    ///     "line": 6,
    ///     "base": "file.c",
    ///     "dir": "test"
    ///   }
    /// }
    /// ```
    pub fn update_functions(&mut self, entries: &[serde_json::Value]) {
        self.functions.clear();
        for entry in entries {
            let name = entry["name"].as_str().unwrap_or_default().to_string();
            let marker = entry["key"].as_str().unwrap_or_default().to_string();
            let declaration = entry["decl"].as_str().unwrap_or_default().to_string();
            let signature = entry["signature"].as_str().unwrap_or_default().to_string();
            let file = entry["sloc"]["file"].as_str().unwrap_or_default().to_string();
            let line = entry["sloc"]["line"].as_u64().unwrap_or(0) as u32;
            let defined = entry["defined"].as_bool().unwrap_or(false);
            if !name.is_empty() {
                self.functions.insert(
                    name.clone(),
                    FunctionInfo {
                        name,
                        marker,
                        declaration,
                        signature,
                        file,
                        line,
                        defined,
                    },
                );
            }
        }
    }

    pub fn resolve_function(&self, name: &str) -> Option<&FunctionInfo> {
        self.functions.get(name)
    }

    /// Populate the globals cache from `fetchGlobals` response entries.
    ///
    /// Verified Frama-C Server JSON format:
    /// ```json
    /// {
    ///   "name": "max_val",
    ///   "key": "vi#25",           // global variable marker
    ///   "decl": "#G25",           // declaration marker
    ///   "type": "int",
    ///   "const": false,
    ///   "volatile": false,
    ///   "sloc": { "file": "/path/to/file.c", "line": 2 }
    /// }
    /// ```
    pub fn update_globals(&mut self, entries: &[serde_json::Value]) {
        self.globals.clear();
        for entry in entries {
            let name = entry["name"].as_str().unwrap_or_default().to_string();
            let marker = entry["key"].as_str().unwrap_or_default().to_string();
            let declaration = entry["decl"].as_str().unwrap_or_default().to_string();
            let typ = entry["type"].as_str().unwrap_or_default().to_string();
            let file = entry["sloc"]["file"].as_str().unwrap_or_default().to_string();
            let line = entry["sloc"]["line"].as_u64().unwrap_or(0) as u32;
            if !name.is_empty() {
                self.globals.insert(
                    name.clone(),
                    GlobalInfo {
                        name,
                        marker,
                        declaration,
                        typ,
                        file,
                        line,
                    },
                );
            }
        }
    }

    pub fn resolve_global(&self, name: &str) -> Option<&GlobalInfo> {
        self.globals.get(name)
    }

    /// Populate callgraph cache from `getCallgraph` response.
    ///
    /// Expected format:
    /// ```json
    /// {
    ///   "edges": [{"src": "#F36", "dst": "#F24", "kind": "both"}],
    ///   "vertices": [{"name": "main", "decl": "#F36"}, ...]
    /// }
    /// ```
    pub fn update_callgraph(&mut self, graph: &serde_json::Value) {
        self.callgraph_edges.clear();
        self.callgraph_vertices.clear();

        if let Some(edges) = graph.get("edges").and_then(|v| v.as_array()) {
            for edge in edges {
                let src = edge["src"].as_str().unwrap_or_default().to_string();
                let dst = edge["dst"].as_str().unwrap_or_default().to_string();
                let kind = edge["kind"].as_str().unwrap_or_default().to_string();
                if !src.is_empty() && !dst.is_empty() {
                    self.callgraph_edges.push(CallEdge { src, dst, kind });
                }
            }
        }

        if let Some(vertices) = graph.get("vertices").and_then(|v| v.as_array()) {
            for vertex in vertices {
                let name = vertex["name"].as_str().unwrap_or_default().to_string();
                let declaration = vertex["decl"].as_str().unwrap_or_default().to_string();
                if !name.is_empty() {
                    self.callgraph_vertices.push(CallVertex { name, declaration });
                }
            }
        }
    }

    /// Find all callers of a function by its declaration marker.
    /// Direction is encoded by src→dst; kind is metadata (e.g. "both",
    /// "inter_functions"), not a direction filter.
    pub fn get_callers(&self, decl_marker: &str) -> Vec<&str> {
        self.callgraph_edges
            .iter()
            .filter(|e| e.dst == decl_marker)
            .map(|e| e.src.as_str())
            .collect()
    }

    /// Find all callees of a function by its declaration marker.
    pub fn get_callees(&self, decl_marker: &str) -> Vec<&str> {
        self.callgraph_edges
            .iter()
            .filter(|e| e.src == decl_marker)
            .map(|e| e.dst.as_str())
            .collect()
    }

    /// Resolve a declaration marker to a function name using callgraph vertices.
    pub fn resolve_decl_to_name(&self, decl_marker: &str) -> Option<&str> {
        self.callgraph_vertices
            .iter()
            .find(|v| v.declaration == decl_marker)
            .map(|v| v.name.as_str())
    }

    pub fn invalidate_all(&mut self) {
        self.project_loaded = false;
        self.eva_completed = false;
        self.wp_completed = false;
        self.functions.clear();
        self.globals.clear();
        self.callgraph_edges.clear();
        self.callgraph_vertices.clear();
        // Note: conclusions and project_state are NOT cleared (preserved across reload)
    }

    // --- Conclusion methods ---

    pub fn store_conclusion(&mut self, update: FunctionConclusionUpdate) {
        let entry = self.conclusions
            .entry(update.function.clone())
            .or_insert_with(|| FunctionVerificationState {
                function: update.function.clone(),
                status: VerificationStatus::InProgress,
                specs: Vec::new(),
                reference_specs: Vec::new(),
                unsound_specs: Vec::new(),
                wp_results: Vec::new(),
                wp_summary: None,
                notes: String::new(),
                callees: Vec::new(),
                callee_info: HashMap::new(),
                existing_asserts: Vec::new(),
                proposed_behaviors: Vec::new(),
                proposed_requires: Vec::new(),
                proposed_ensures: Vec::new(),
                proposed_assigns: Vec::new(),
                proposed_loop_annots: Vec::new(),
                proposed_terminates: None,
                callee_requests: Vec::new(),
                sp_revision_count: 0,
                last_sp_error_analysis: String::new(),
                proposed_revision_count: 0,
                last_proposed_error_analysis: String::new(),
                failure_evidence: None,
                verified_source: None,
                unsound_reason_type: None,
                blocking_callee_requires: None,
                infeasible_requests: Vec::new(),
                ast_stmt_count: None,
                sandbox_clean: true,
                annotation_count: 0,
                sandbox_deleted: false,
                conclusion_history: Vec::new(),
            });

        // push_history: 在合并新字段之前快照当前 entry
        if update.push_history {
            let mut snapshot = entry.clone();
            // 不要把 conclusion_history 自身嵌套进 snapshot（避免无限增长）
            snapshot.conclusion_history.clear();
            // 注：长文本字段已从 FunctionVerificationState 删除（Plan A），snapshot 自动
            // 不含长文本。要查阅历史长文本，让用户把 .frama-c-mcp/ 纳入 git，git log + git show 即可。
            entry.conclusion_history.push(snapshot);
        }

        // 通用 merge：Some → 覆盖；None → 保留
        // 注：长文本字段（semantic_proof / semiformal_proof / program_summary）
        // 不在 state 层 merge——它们由 caller 用 Write 工具直接写到 .md 文件（Plan A）。
        // 这些字段也已从 FunctionConclusionUpdate 删除（不接受 API 输入）。
        if let Some(s) = update.status { entry.status = s; }
        if let Some(v) = update.specs { entry.specs = v; }
        // 保持 annotation_count 与 specs.length 一致（#54：Revision 缩减 specs 时同步）
        entry.annotation_count = entry.specs.len() as u32;
        if let Some(v) = update.reference_specs { entry.reference_specs = v; }
        if let Some(v) = update.unsound_specs { entry.unsound_specs = v; }
        if let Some(v) = update.wp_results { entry.wp_results = v; }
        if let Some(s) = update.wp_summary { entry.wp_summary = Some(s); }
        if let Some(s) = update.notes { entry.notes = s; }

        if let Some(v) = update.callees { entry.callees = v; }
        if let Some(v) = update.callee_info { entry.callee_info = v; }
        if let Some(v) = update.existing_asserts { entry.existing_asserts = v; }

        if let Some(v) = update.proposed_behaviors { entry.proposed_behaviors = v; }
        if let Some(v) = update.proposed_requires { entry.proposed_requires = v; }
        if let Some(v) = update.proposed_ensures { entry.proposed_ensures = v; }
        if let Some(v) = update.proposed_assigns { entry.proposed_assigns = v; }
        if let Some(v) = update.proposed_loop_annots { entry.proposed_loop_annots = v; }
        if let Some(v) = update.proposed_terminates { entry.proposed_terminates = Some(v); }

        if let Some(v) = update.callee_requests { entry.callee_requests = v; }

        if let Some(v) = update.sp_revision_count { entry.sp_revision_count = v; }
        if let Some(v) = update.last_sp_error_analysis { entry.last_sp_error_analysis = v; }
        if let Some(v) = update.proposed_revision_count { entry.proposed_revision_count = v; }
        if let Some(v) = update.last_proposed_error_analysis { entry.last_proposed_error_analysis = v; }

        if let Some(s) = update.failure_evidence { entry.failure_evidence = Some(s); }
        // C2 (fsmint-6 fix): 转为 Verified 时清掉上一次失败 attempt 残留的 failure_evidence。
        // 否则重派成功（如 r2 failed → r3 verified）后 meta 仍挂旧 failure_evidence，误导审计/报告。
        // 只清 Verified（Failed/Unsound/BlockedOnCallee 的 evidence 是当前态的，保留）。
        if matches!(entry.status, VerificationStatus::Verified) {
            entry.failure_evidence = None;
        }
        if let Some(s) = update.verified_source { entry.verified_source = Some(s); }
        // verify-program-fsm v1 接入 (detailed-design §6.4)
        if let Some(s) = update.unsound_reason_type { entry.unsound_reason_type = Some(s); }
        if let Some(s) = update.blocking_callee_requires { entry.blocking_callee_requires = Some(s); }
        if let Some(v) = update.infeasible_requests { entry.infeasible_requests = v; }
        // 注：长文本字段（semantic_proof / semiformal_proof / program_summary）已从
        // FunctionConclusionUpdate 删除（Plan A 收尾）；caller 用 Write 工具直接写 .md 文件。
    }

    pub fn get_conclusion(&self, function: &str) -> Option<&FunctionVerificationState> {
        self.conclusions.get(function)
    }

    // --- sandbox lifecycle 副作用（§13.6 改动 5/15）---

    /// create_sandbox 后：初始化 sandbox 状态字段。如 conclusion 不存在则建。
    pub fn on_sandbox_created(&mut self, function: &str, ast_stmt_count: Option<u32>) {
        // 先准备 fallback conclusion（避开 entry().or_insert_with 闭包里再借 self 的冲突）
        let fallback = Self::empty_conclusion_static(function);
        let entry = self.conclusions.entry(function.to_string()).or_insert(fallback);
        entry.ast_stmt_count = ast_stmt_count;
        entry.sandbox_clean = true;
        entry.annotation_count = 0;
        entry.sandbox_deleted = false;
        // 重新创建 sandbox 等于"重新开始验证"，把已有 finalized 状态重置为 in_progress
        if matches!(
            entry.status,
            VerificationStatus::Verified
                | VerificationStatus::Failed
                | VerificationStatus::Unsound
                | VerificationStatus::BlockedOnCallee
        ) {
            entry.status = VerificationStatus::InProgress;
        }
    }

    /// add_annotation_sandbox 后：sandbox_clean=false + 累加 annotation_count。
    pub fn on_annotation_added(&mut self, function: &str) {
        if let Some(entry) = self.conclusions.get_mut(function) {
            entry.sandbox_clean = false;
            entry.annotation_count = entry.annotation_count.saturating_add(1);
        }
    }

    /// reset_sandbox 后：sandbox_clean=true + annotation_count=0。
    pub fn on_sandbox_reset(&mut self, function: &str) {
        if let Some(entry) = self.conclusions.get_mut(function) {
            entry.sandbox_clean = true;
            entry.annotation_count = 0;
        }
    }

    /// delete_sandbox 后：sandbox_deleted=true（保留其他字段供 audit）。
    pub fn on_sandbox_deleted(&mut self, function: &str) {
        if let Some(entry) = self.conclusions.get_mut(function) {
            entry.sandbox_deleted = true;
        }
    }

    fn empty_conclusion_static(function: &str) -> FunctionVerificationState {
        FunctionVerificationState {
            function: function.to_string(),
            status: VerificationStatus::InProgress,
            specs: Vec::new(),
            reference_specs: Vec::new(),
            unsound_specs: Vec::new(),
            wp_results: Vec::new(),
            wp_summary: None,
            notes: String::new(),
            callees: Vec::new(),
            callee_info: HashMap::new(),
            existing_asserts: Vec::new(),
            proposed_behaviors: Vec::new(),
            proposed_requires: Vec::new(),
            proposed_ensures: Vec::new(),
            proposed_assigns: Vec::new(),
            proposed_loop_annots: Vec::new(),
            proposed_terminates: None,
            callee_requests: Vec::new(),
            sp_revision_count: 0,
            last_sp_error_analysis: String::new(),
            proposed_revision_count: 0,
            last_proposed_error_analysis: String::new(),
            failure_evidence: None,
            verified_source: None,
            unsound_reason_type: None,
            blocking_callee_requires: None,
            infeasible_requests: Vec::new(),
            ast_stmt_count: None,
            sandbox_clean: true,
            annotation_count: 0,
            sandbox_deleted: false,
            conclusion_history: Vec::new(),
        }
    }

    pub fn list_conclusions(&self, status_filter: Option<&VerificationStatus>) -> Vec<ConclusionSummary> {
        self.conclusions.values()
            .filter(|c| match status_filter {
                Some(filter) => c.status == *filter,
                None => true,
            })
            .map(|c| ConclusionSummary {
                function: c.function.clone(),
                status: c.status.clone(),
                wp_summary: c.wp_summary.clone(),
            })
            .collect()
    }

    // --- Project state methods ---

    pub fn store_project_state(&mut self, update: ProjectStateUpdate) {
        let state = self.project_state.get_or_insert_with(ProjectVerificationState::default);
        // 旧字段
        if let Some(v) = update.source_files { state.source_files = v; }
        if let Some(v) = update.verification_order { state.verification_order = v; }
        if let Some(i) = update.current_index { state.current_index = i; }
        if let Some(s) = update.global_notes { state.global_notes = s; }
        // verify-program-fsm v1 新字段（levels 字段已删 = VO-completeness fix；server-owned 不经 update）
        if let Some(v) = update.current_level { state.current_level = v; }
        if let Some(v) = update.scc_groups { state.scc_groups = v; }
        if let Some(v) = update.completion_map { state.completion_map = v; }
        if let Some(v) = update.feedback_pending { state.feedback_pending = v; }
        if let Some(v) = update.scc_iteration_counters { state.scc_iteration_counters = v; }
        if let Some(v) = update.scc_spec_hashes { state.scc_spec_hashes = v; }
        if let Some(v) = update.last_spec_hash { state.last_spec_hash = v; }
        if let Some(v) = update.last_progress_snapshot { state.last_progress_snapshot = v; }
        if let Some(v) = update.failure_evidence { state.failure_evidence = Some(v); }
        if let Some(v) = update.final_gate_result { state.final_gate_result = Some(v); }
        if let Some(v) = update.merged_source { state.merged_source = Some(v); }
        if let Some(v) = update.locked { state.locked = v; }
    }

    pub fn get_project_state(&self) -> Option<&ProjectVerificationState> {
        self.project_state.as_ref()
    }

    /// full-replace 入口（vp-fsm store_project_state 主路径）。
    /// store_project_state 是字段 merge，整体替换需用本方法。
    /// 注意：服务端字段（locked）由调用方在替换前自行保留。
    pub fn set_project_state_full(&mut self, new_state: ProjectVerificationState) {
        self.project_state = Some(new_state);
    }

    pub fn set_eva_completed(&mut self) {
        self.eva_completed = true;
    }

    pub fn set_wp_completed(&mut self) {
        self.wp_completed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_and_resolve() {
        let mut state = SessionState::default();
        let entries = vec![serde_json::json!({
            "name": "main",
            "key": "kf#36",
            "decl": "#F36",
            "signature": "int main(void); /* main */",
            "defined": true,
            "sloc": {
                "file": "/tmp/test.c",
                "line": 10,
                "base": "test.c",
                "dir": ""
            }
        })];
        state.update_functions(&entries);
        assert_eq!(state.functions.len(), 1);
        let info = state.resolve_function("main").unwrap();
        assert_eq!(info.marker, "kf#36");
        assert_eq!(info.declaration, "#F36");
        assert_eq!(info.signature, "int main(void); /* main */");
        assert_eq!(info.file, "/tmp/test.c");
        assert_eq!(info.line, 10);
    }

    #[test]
    fn resolve_missing() {
        let state = SessionState::default();
        assert!(state.resolve_function("nonexistent").is_none());
    }

    #[test]
    fn invalidate_all() {
        let mut state = SessionState::default();
        state.project_loaded = true;
        state.eva_completed = true;
        state.wp_completed = true;
        state.functions.insert(
            "f".into(),
            FunctionInfo {
                name: "f".into(),
                marker: "kf#1".into(),
                declaration: "#F1".into(),
                signature: "void f(void);".into(),
                file: "a.c".into(),
                line: 1,
                defined: true,
            },
        );
        state.globals.insert(
            "g".into(),
            GlobalInfo {
                name: "g".into(),
                marker: "kv#1".into(),
                declaration: "#V1".into(),
                typ: "int".into(),
                file: "a.c".into(),
                line: 1,
            },
        );
        state.callgraph_edges.push(CallEdge {
            src: "#F1".into(),
            dst: "#F2".into(),
            kind: "both".into(),
        });
        state.callgraph_vertices.push(CallVertex {
            name: "f".into(),
            declaration: "#F1".into(),
        });
        state.invalidate_all();
        assert!(!state.project_loaded);
        assert!(!state.eva_completed);
        assert!(!state.wp_completed);
        assert!(state.functions.is_empty());
        assert!(state.globals.is_empty());
        assert!(state.callgraph_edges.is_empty());
        assert!(state.callgraph_vertices.is_empty());
    }

    #[test]
    fn skip_empty_name() {
        let mut state = SessionState::default();
        let entries = vec![serde_json::json!({
            "name": "",
            "key": "#F1"
        })];
        state.update_functions(&entries);
        assert!(state.functions.is_empty());
    }

    #[test]
    fn invariants() {
        let mut state = SessionState::default();
        state.set_eva_completed();
        assert!(state.eva_completed);
        state.set_wp_completed();
        assert!(state.wp_completed);
    }

    #[test]
    fn update_and_resolve_globals() {
        let mut state = SessionState::default();
        let entries = vec![serde_json::json!({
            "name": "counter",
            "key": "vi#24",
            "decl": "#G24",
            "type": "int",
            "const": false,
            "volatile": false,
            "sloc": {
                "file": "/tmp/test.c",
                "line": 3
            }
        })];
        state.update_globals(&entries);
        assert_eq!(state.globals.len(), 1);
        let info = state.resolve_global("counter").unwrap();
        assert_eq!(info.marker, "vi#24");
        assert_eq!(info.declaration, "#G24");
        assert_eq!(info.typ, "int");
        assert_eq!(info.file, "/tmp/test.c");
        assert_eq!(info.line, 3);
    }

    #[test]
    fn resolve_global_missing() {
        let state = SessionState::default();
        assert!(state.resolve_global("nonexistent").is_none());
    }

    #[test]
    fn skip_empty_global_name() {
        let mut state = SessionState::default();
        let entries = vec![serde_json::json!({
            "name": "",
            "key": "kv#1",
            "decl": "#V1",
            "type": "int"
        })];
        state.update_globals(&entries);
        assert!(state.globals.is_empty());
    }

    #[test]
    fn update_callgraph_and_query() {
        let mut state = SessionState::default();
        // Uses actual Frama-C kinds: "both" and "inter_functions"
        let graph = serde_json::json!({
            "edges": [
                {"src": "#F44", "dst": "#F37", "kind": "both"},
                {"src": "#F37", "dst": "#F33", "kind": "inter_functions"},
                {"src": "#F37", "dst": "#F26", "kind": "inter_functions"}
            ],
            "vertices": [
                {"name": "main", "decl": "#F44"},
                {"name": "process", "decl": "#F37"},
                {"name": "increment", "decl": "#F33"},
                {"name": "clamp", "decl": "#F26"}
            ]
        });
        state.update_callgraph(&graph);

        assert_eq!(state.callgraph_edges.len(), 3);
        assert_eq!(state.callgraph_vertices.len(), 4);

        // main calls process
        let main_callees = state.get_callees("#F44");
        assert_eq!(main_callees.len(), 1);
        assert!(main_callees.contains(&"#F37"));

        // process calls clamp and increment
        let process_callees = state.get_callees("#F37");
        assert_eq!(process_callees.len(), 2);
        assert!(process_callees.contains(&"#F33"));
        assert!(process_callees.contains(&"#F26"));

        // clamp is called by process
        let clamp_callers = state.get_callers("#F26");
        assert_eq!(clamp_callers.len(), 1);
        assert!(clamp_callers.contains(&"#F37"));

        // process is called by main
        let process_callers = state.get_callers("#F37");
        assert_eq!(process_callers.len(), 1);
        assert!(process_callers.contains(&"#F44"));

        // resolve decl to name
        assert_eq!(state.resolve_decl_to_name("#F44"), Some("main"));
        assert_eq!(state.resolve_decl_to_name("#F26"), Some("clamp"));
        assert_eq!(state.resolve_decl_to_name("#F99"), None);
    }

    #[test]
    fn callgraph_empty_edges() {
        let state = SessionState::default();
        assert!(state.get_callers("#F1").is_empty());
        assert!(state.get_callees("#F1").is_empty());
    }

    // --- Conclusion tests ---

    #[test]
    fn store_and_get_conclusion() {
        let mut state = SessionState::default();
        state.store_conclusion(FunctionConclusionUpdate {
            function: "abs".into(),
            status: Some(VerificationStatus::InProgress),

            specs: None, reference_specs: None, wp_results: None,
            notes: None, wp_summary: None, ..Default::default()
        });
        let c = state.get_conclusion("abs").unwrap();
        assert_eq!(c.status, VerificationStatus::InProgress);
        assert!(c.specs.is_empty());

        state.store_conclusion(FunctionConclusionUpdate {
            function: "abs".into(),
            status: Some(VerificationStatus::Verified),
            specs: Some(vec![AnnotationEntry {
                hash_label: "re_001".into(),
                user_label: None,
                kind: "spec".into(),
                acsl: "val >= -2147483647".into(),
                stmt_id: None,
                derived_from: "proposed_requires[0]".into(),
                source: AnnotationSource::Generated,
                purpose: "avoid signed overflow on negation".into(),
                proof_target: None,
                wp_status: None,
                wp_time_ms: None,
                wp_prover: None,
            }]),
            reference_specs: None, wp_results: None,
            notes: None,
            wp_summary: Some(WpGoalSummary {
                total: 3, valid: 3, unknown: 0, timeout: 0, failed: 0,
                model: None, timeout_used: None, recorded_at_retry: None,
                failed_goal_labels: vec![], failed_source_asserts: vec![],
            }),
            ..Default::default()
        });
        let c = state.get_conclusion("abs").unwrap();
        assert_eq!(c.status, VerificationStatus::Verified);
        assert_eq!(c.specs.len(), 1);
        // kind 是顶层二分类 "spec" / "annot"（state.rs:181），ACSL 子类型由 derived_from 携带
        assert_eq!(c.specs[0].kind, "spec");
        assert_eq!(c.specs[0].derived_from, "proposed_requires[0]");
        // 长文本字段不在 in-memory state（Plan A），handler 层从磁盘读
        assert_eq!(c.wp_summary.as_ref().unwrap().valid, 3);
    }

    #[test]
    fn upsert_preserves_none_fields() {
        let mut state = SessionState::default();
        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(),
            status: Some(VerificationStatus::InProgress),
            specs: Some(vec![AnnotationEntry {
                hash_label: "en_001".into(), user_label: None,
                kind: "spec".into(), acsl: "\\result >= 0".into(),
                stmt_id: None, derived_from: "proposed_ensures[0]".into(),
                source: AnnotationSource::Generated,
                purpose: "main postcondition".into(), proof_target: None,
                wp_status: Some("valid".into()), wp_time_ms: Some(100), wp_prover: Some("Qed".into()),
            }]),
            reference_specs: None, wp_results: None,
            notes: Some("some note".into()), wp_summary: None, ..Default::default()
        });
        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(),
            status: Some(VerificationStatus::Verified),

            specs: None, reference_specs: None, wp_results: None,
            notes: None, wp_summary: None, ..Default::default()
        });
        let c = state.get_conclusion("f").unwrap();
        assert_eq!(c.status, VerificationStatus::Verified);
        // 长文本字段 (semantic_proof / semiformal_proof / program_summary) 不在 in-memory state (Plan A)
        assert_eq!(c.specs.len(), 1);
        assert_eq!(c.notes, "some note");
    }

    #[test]
    fn list_conclusions_filter() {
        let mut state = SessionState::default();
        for (name, status) in [("a", VerificationStatus::Verified), ("b", VerificationStatus::Unsound), ("c", VerificationStatus::Failed)] {
            state.store_conclusion(FunctionConclusionUpdate {
                function: name.into(), status: Some(status),
                specs: None, reference_specs: None, wp_results: None,
                notes: None, wp_summary: None, ..Default::default()
            });
        }
        assert_eq!(state.list_conclusions(None).len(), 3);
        assert_eq!(state.list_conclusions(Some(&VerificationStatus::Verified)).len(), 1);
        assert_eq!(state.list_conclusions(Some(&VerificationStatus::InProgress)).len(), 0);
    }

    #[test]
    fn store_and_get_project_state() {
        let mut state = SessionState::default();
        assert!(state.get_project_state().is_none());
        state.store_project_state(ProjectStateUpdate {
            source_files: Some(vec!["a.c".into()]),
            verification_order: Some(vec!["f".into(), "g".into()]),
            current_index: Some(0),
            global_notes: Some("test".into()),
            ..Default::default()
        });
        let ps = state.get_project_state().unwrap();
        assert_eq!(ps.source_files, vec!["a.c"]);
        assert_eq!(ps.current_index, 0);

        state.store_project_state(ProjectStateUpdate {
            current_index: Some(1),
            ..Default::default()
        });
        let ps = state.get_project_state().unwrap();
        assert_eq!(ps.current_index, 1);
        assert_eq!(ps.source_files, vec!["a.c"]);
    }

    #[test]
    fn invalidate_all_preserves_conclusions() {
        let mut state = SessionState::default();
        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(), status: Some(VerificationStatus::Verified),

            specs: None, reference_specs: None, wp_results: None,
            notes: None, wp_summary: None, ..Default::default()
        });
        state.store_project_state(ProjectStateUpdate {
            source_files: Some(vec!["a.c".into()]),
            ..Default::default()
        });
        state.invalidate_all();
        assert!(!state.project_loaded);
        assert!(state.functions.is_empty());
        assert_eq!(state.conclusions.len(), 1);
        assert!(state.get_project_state().is_some());
    }

    /// Regression test: sandbox client must NOT share SessionState with main client.
    ///
    /// Before fix: create_sandbox passed self.state.clone() (same Arc) to sandbox client.
    /// Sandbox's fetchFunctions called state.update_functions(), clearing main's 20 functions.
    ///
    /// After fix: sandbox client gets its own SessionState::default().
    /// Main's state is never touched by sandbox operations.
    #[test]
    fn sandbox_must_not_clobber_main_state() {
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let main_state = Arc::new(RwLock::new(SessionState::default()));

        // Main instance loads 20 functions
        let main_entries: Vec<serde_json::Value> = (0..20)
            .map(|i| serde_json::json!({
                "name": format!("func_{}", i),
                "key": format!("kf#{}", i),
                "decl": format!("#F{}", i),
                "signature": format!("int func_{}(void);", i),
                "defined": true,
                "sloc": {"file": "/tmp/main.c", "line": i + 1, "base": "main.c", "dir": ""}
            }))
            .collect();

        {
            let mut st = main_state.blocking_write();
            st.update_functions(&main_entries);
        }
        assert_eq!(main_state.blocking_read().functions.len(), 20);

        // Fix: sandbox gets INDEPENDENT state (not main_state.clone())
        let sandbox_state = Arc::new(RwLock::new(SessionState::default()));

        // Sandbox fetches its own (smaller) function list
        let sandbox_entries = vec![
            serde_json::json!({
                "name": "func_15",
                "key": "kf#0",
                "decl": "#F0",
                "signature": "int func_15(void);",
                "defined": true,
                "sloc": {"file": "/tmp/sandbox.c", "line": 1, "base": "sandbox.c", "dir": ""}
            }),
            serde_json::json!({
                "name": "func_3",
                "key": "kf#1",
                "decl": "#F1",
                "signature": "int func_3(void);",
                "defined": true,
                "sloc": {"file": "/tmp/sandbox.c", "line": 5, "base": "sandbox.c", "dir": ""}
            }),
        ];

        {
            let mut st = sandbox_state.blocking_write();
            st.update_functions(&sandbox_entries);
        }

        // Main state must be unaffected
        assert_eq!(
            main_state.blocking_read().functions.len(), 20,
            "Main state must not be clobbered by sandbox update_functions"
        );
        // Sandbox has its own state
        assert_eq!(sandbox_state.blocking_read().functions.len(), 2);
    }

    /// Round-trip 回归测试：覆盖本 PR 加的 14 个 conclusion 字段。
    ///
    /// 测两件事：
    /// 1. **Store-Load**：FunctionConclusionUpdate.X = Some(v) 经 store_conclusion
    ///    merge 后，get_conclusion 拿回的 entry.X 必须与 v 一致——保护
    ///    `if let Some(v) = update.X { entry.X = v; }` 那 14 行不会未来漏接某条。
    /// 2. **Serde JSON**：完整 FunctionVerificationState → JSON → struct →
    ///    再 to_value 必须等于原 to_value——保护 #[serde(default)] /
    ///    #[serde(skip_serializing_if = ...)] 配置不会让某字段写得出读不回（或反之）。
    ///
    /// 没有这条测试，未来若有人删 `if let Some(v) = update.failure_evidence`，
    /// 现有所有测试仍会过，但 FSM 的 failed 路径会突然 store 不进去。
    #[test]
    fn round_trip_new_conclusion_fields() {
        let mut state = SessionState::default();

        let mut callee_info_map = HashMap::new();
        callee_info_map.insert(
            "g".to_string(),
            CalleeInfo {
                status: "verified".into(),
                sources: CalleeSources {
                    verified_spec: Some("/*@ ensures \\result >= 0; */".into()),
                    semantic_proof: Some("g returns nonneg".into()),
                    ..Default::default()
                },
            },
        );

        state.store_conclusion(FunctionConclusionUpdate {
            function: "F".into(),
            status: Some(VerificationStatus::Failed),
            // 长文本字段已从 store API 删除（Plan A 收尾，见 remove-store-long-text-fields.md）。
            // 本测试只测短/结构化字段的 round-trip。
            specs: None, reference_specs: None, wp_results: None,
            wp_summary: None, notes: None,

            // S1
            callees: Some(vec!["g".into(), "h".into()]),
            callee_info: Some(callee_info_map),
            existing_asserts: Some(vec![ExistingAssert {
                stmt_id: 42,
                acsl: "assert n >= 0;".into(),
                origin: "source".into(),
            }]),

            // S2.5 — schema v2: behaviors 顶层 + assigns Vec
            proposed_behaviors: Some(vec![ProposedBehavior {
                name: "nonneg".into(),
                assumes: vec!["n >= 0".into()],
            }]),
            proposed_requires: Some(vec![ProposedRequires {
                acsl: "n >= 0".into(),
                behavior: None,
                necessity: "防止 UB".into(),
            }]),
            proposed_ensures: Some(vec![ProposedEnsures {
                acsl: "\\result == n + 1".into(),
                from: "step 8 path-1".into(),
                behavior: Some("nonneg".into()),
            }]),
            proposed_assigns: Some(vec![ProposedAssigns {
                acsl: "\\nothing".into(),
                behavior: None,
            }]),
            proposed_loop_annots: Some(vec![ProposedLoopAnnot {
                stmt_id: 7,
                loop_label: "main loop".into(),
                invariants: vec![
                    ProposedLoopInvariant { acsl: "i >= 0".into(), behavior: None },
                    ProposedLoopInvariant { acsl: "i <= n".into(), behavior: None },
                ],
                assigns: vec![ProposedLoopAssigns { acsl: "i".into(), behavior: None }],
                variant: Some(ProposedLoopVariant { acsl: "n - i".into(), behavior: None }),
            }]),
            proposed_terminates: None,

            // callee_gap
            callee_requests: Some(vec![CalleeRequest {
                callee: "h".into(),
                required_property: "\\result > 0".into(),
                reason: "用于步骤 5 的 ensures 推导".into(),
            }]),

            // revision counters（替代旧 history Vec）
            sp_revision_count: Some(1),
            last_sp_error_analysis: Some("step 5 漏掉 negative branch".into()),
            proposed_revision_count: Some(1),
            last_proposed_error_analysis: Some("translation issue".into()),

            // failure
            failure_evidence: Some(FailureEvidence {
                failure_type: "rte_overflow".into(),
                location: "stmt#42".into(),
                acsl: "x + y <= INT_MAX".into(),
                unsupported_predicate: None,
                attempted_reformulations: vec!["x <= INT_MAX - y".into()],
                counterexample: Some("x=INT_MAX, y=1".into()),
            }),

            // unsound
            unsound_specs: Some(vec![UnsoundSpec {
                hash_label: "li_a3f2".into(),
                kind: "spec".into(),
                acsl: "\\result > 0".into(),
                stmt_id: None,
                counterexample: "n=0 → \\result=0 矛盾".into(),
                removed_at_retry: Some(2),
            }]),

            // S5
            verified_source: Some("/tmp/F_verified.c".into()),

            // verify-program-fsm v1 接入 (detailed-design §6.4) — 旧测试不用
            unsound_reason_type: None,
            blocking_callee_requires: None,
            infeasible_requests: None,

            // cross-FSM: program_summary 不在 in-memory state（Plan A），handler 写文件

            push_history: false,
        });

        let stored = state.get_conclusion("F").expect("conclusion stored").clone();

        // ── (1) Store-Load 字段断言：每条 if let Some(v) → entry.v 都 round-trip ──
        assert_eq!(stored.status, VerificationStatus::Failed);
        assert_eq!(stored.callees, vec!["g".to_string(), "h".to_string()]);
        assert_eq!(stored.callee_info.len(), 1);
        assert_eq!(stored.callee_info["g"].status, "verified");
        assert_eq!(stored.existing_asserts.len(), 1);
        assert_eq!(stored.existing_asserts[0].stmt_id, 42);
        // schema v2: behaviors 顶层 + assigns Vec + loop_annots typed
        assert_eq!(stored.proposed_behaviors.len(), 1);
        assert_eq!(stored.proposed_behaviors[0].name, "nonneg");
        assert_eq!(stored.proposed_behaviors[0].assumes, vec!["n >= 0".to_string()]);
        assert_eq!(stored.proposed_requires.len(), 1);
        assert_eq!(stored.proposed_requires[0].acsl, "n >= 0");
        assert_eq!(stored.proposed_requires[0].behavior, None);
        assert_eq!(stored.proposed_ensures.len(), 1);
        assert_eq!(stored.proposed_ensures[0].behavior.as_deref(), Some("nonneg"));
        assert_eq!(stored.proposed_assigns.len(), 1);
        assert_eq!(stored.proposed_assigns[0].acsl, "\\nothing");
        assert_eq!(stored.proposed_loop_annots.len(), 1);
        assert_eq!(stored.proposed_loop_annots[0].invariants.len(), 2);
        assert_eq!(stored.proposed_loop_annots[0].invariants[0].acsl, "i >= 0");
        assert_eq!(stored.proposed_loop_annots[0].assigns.len(), 1);
        assert_eq!(stored.proposed_loop_annots[0].assigns[0].acsl, "i");
        assert_eq!(stored.proposed_loop_annots[0].variant.as_ref().unwrap().acsl, "n - i");
        assert_eq!(stored.callee_requests.len(), 1);
        assert_eq!(stored.callee_requests[0].callee, "h");
        assert_eq!(stored.sp_revision_count, 1);
        assert_eq!(stored.last_sp_error_analysis, "step 5 漏掉 negative branch");
        assert_eq!(stored.proposed_revision_count, 1);
        assert_eq!(stored.last_proposed_error_analysis, "translation issue");
        assert_eq!(
            stored.failure_evidence.as_ref().unwrap().failure_type,
            "rte_overflow"
        );
        assert_eq!(stored.unsound_specs.len(), 1);
        assert_eq!(stored.unsound_specs[0].hash_label, "li_a3f2");
        assert_eq!(stored.verified_source.as_deref(), Some("/tmp/F_verified.c"));
        // program_summary 长文本不在 in-memory state（Plan A）

        // ── (2) Serde JSON round-trip：完整结构序列化再反序列化必须 byte-equal ──
        // 用 to_value 比较而非 to_string，避免 HashMap 字段顺序差异导致假阳性。
        let original_json = serde_json::to_value(&stored).expect("serialize");
        let json_str = serde_json::to_string(&stored).expect("to_string");
        let recovered: FunctionVerificationState =
            serde_json::from_str(&json_str).expect("deserialize");
        let recovered_json = serde_json::to_value(&recovered).expect("re-serialize");
        assert_eq!(
            original_json, recovered_json,
            "JSON round-trip lost or mutated fields. original={:#}, recovered={:#}",
            original_json, recovered_json
        );
    }

    /// C2 (fsmint-6 fix): store status=Verified 时清掉上一次失败 attempt 残留的 failure_evidence。
    /// 复现 parse_NoticeReference r2(failed)→r3(verified) 后 meta 仍挂 r2 failure_evidence 的 bug。
    #[test]
    fn verified_clears_stale_failure_evidence() {
        let mut state = SessionState::default();
        // 1. r2 失败：存 Failed + failure_evidence
        state.store_conclusion(FunctionConclusionUpdate {
            function: "F".into(),
            status: Some(VerificationStatus::Failed),
            failure_evidence: Some(FailureEvidence {
                failure_type: "termination_waiver_unapplied".into(),
                location: "WP 81/82".into(),
                acsl: "terminates \\false".into(),
                unsupported_predicate: None,
                attempted_reformulations: vec![],
                counterexample: None,
            }),
            ..Default::default()
        });
        assert!(
            state.get_conclusion("F").unwrap().failure_evidence.is_some(),
            "Failed 态 failure_evidence 应存在"
        );
        // 2. r3 成功：存 Verified（不带 failure_evidence）
        state.store_conclusion(FunctionConclusionUpdate {
            function: "F".into(),
            status: Some(VerificationStatus::Verified),
            ..Default::default()
        });
        // 3. failure_evidence 应被清掉（不留 r2 旧证据）
        assert!(
            state.get_conclusion("F").unwrap().failure_evidence.is_none(),
            "Verified 后应清掉旧 failure_evidence (C2)"
        );
    }

    /// 边缘 round-trip：所有新字段为空/None 时也要 round-trip 干净
    ///（保护 skip_serializing_if 跟 default 不对称的情况）。
    #[test]
    fn round_trip_empty_conclusion_fields() {
        let mut state = SessionState::default();
        state.store_conclusion(FunctionConclusionUpdate {
            function: "leaf".into(),
            status: Some(VerificationStatus::Verified),
            ..Default::default()
        });
        let stored = state.get_conclusion("leaf").unwrap().clone();

        let original_json = serde_json::to_value(&stored).unwrap();
        let json_str = serde_json::to_string(&stored).unwrap();
        let recovered: FunctionVerificationState = serde_json::from_str(&json_str).unwrap();
        let recovered_json = serde_json::to_value(&recovered).unwrap();
        assert_eq!(original_json, recovered_json);

        // 关键：S1 hard check 要求 callees/callee_info/existing_asserts 等字段
        // 在持久化 JSON 里**必须存在**（哪怕空数组），不能 skip_serializing_if。
        // 否则叶函数的 hard check 会失败。
        let json_obj = original_json.as_object().expect("top-level object");
        for must_exist in [
            "callees", "callee_info", "existing_asserts",
            "proposed_requires", "proposed_ensures", "proposed_loop_annots",
            "callee_requests",
            // 新设计：替代 sp_revision_history / proposed_revision_history Vec
            "sp_revision_count", "last_sp_error_analysis",
            "proposed_revision_count", "last_proposed_error_analysis",
        ] {
            assert!(
                json_obj.contains_key(must_exist),
                "field '{}' must be serialized even when empty (hard check requires it)",
                must_exist
            );
        }
    }

    // ── verify-program-fsm v1 ProjectVerificationState 扩字段单测 (§5.3.2) ──

    /// 3.2.1 字段 serialize/deserialize round-trip
    #[test]
    fn project_state_serialize_round_trip() {
        let mut state = ProjectVerificationState::default();
        state.source_files = vec!["a.c".into()];
        state.current_level = 0;
        state.completion_map.insert(
            "foo".into(),
            FunctionCompletion {
                status: "completed".into(),
                last_attempt_at: None,
                verified_source: Some("/path/foo_verified.c".into()),
            },
        );
        state.scc_groups.push(SccGroup {
            id: 0,
            members: vec!["foo".into()],
            level: 0,
            is_cycle: false,
        });
        state.locked = true;

        let json = serde_json::to_string(&state).unwrap();
        let restored: ProjectVerificationState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.source_files, state.source_files);
        assert_eq!(restored.completion_map.len(), 1);
        assert_eq!(restored.completion_map["foo"].status, "completed");
        assert_eq!(restored.scc_groups.len(), 1);
        assert!(!restored.scc_groups[0].is_cycle);
        assert!(restored.locked);
    }

    /// 3.2.2 partial update: None 字段不动既有值
    #[test]
    fn project_state_partial_update_keeps_unset_fields() {
        let mut session = SessionState::default();

        // 初始化: current_level=5, locked=true
        session.store_project_state(ProjectStateUpdate {
            current_level: Some(5),
            locked: Some(true),
            ..Default::default()
        });

        // 仅更 current_level
        session.store_project_state(ProjectStateUpdate {
            current_level: Some(7),
            locked: None,  // 不动 locked
            ..Default::default()
        });

        let state = session.get_project_state().expect("project state set");
        assert_eq!(state.current_level, 7);
        assert!(state.locked, "locked must remain true after partial update with None");
    }

    /// #112 fix 方案 C 回归：state_json 省略全部 level 系列字段
    /// （completion_map 条目无 level [C-1 删字段]、顶层无 levels/current_level [C-3 default]、
    ///  scc_groups 条目无 level [C-3 default]）→ 反序列化成功（不再 `missing field 'level'`）。
    #[test]
    fn project_state_deserializes_without_level_fields() {
        let json = r#"{
            "source_files": ["x.c"],
            "verification_order": ["f"],
            "current_index": 0,
            "global_notes": "",
            "scc_groups": [{"id": 0, "members": ["f"], "is_cycle": false}],
            "completion_map": {"f": {"status": "completed", "last_attempt_at": null, "verified_source": null}},
            "feedback_pending": {},
            "scc_iteration_counters": {},
            "scc_spec_hashes": {},
            "last_spec_hash": {},
            "last_progress_snapshot": {}
        }"#;
        let restored: ProjectVerificationState =
            serde_json::from_str(json).expect("缺 level 系列字段应能解析");
        assert_eq!(restored.completion_map["f"].status, "completed");
        assert_eq!(restored.scc_groups[0].members, vec!["f".to_string()]);
        assert_eq!(restored.current_level, 0, "current_level 缺省 0");
        assert_eq!(restored.scc_groups[0].level, 0, "SccGroup.level 缺省 0");

        // VO-completeness fix（F6）：`levels` 字段已删——旧 _program.json 含 `levels` key
        // 仍能反序列化（serde 忽略未知 key，全 struct 无 deny_unknown_fields）= backward compat。
        let json_with_old_levels = r#"{
            "source_files": ["x.c"], "verification_order": ["f"], "current_index": 0,
            "global_notes": "", "levels": [{"level": 0, "groups": []}],
            "scc_groups": [{"id": 0, "members": ["f"], "is_cycle": false}],
            "completion_map": {}, "feedback_pending": {},
            "scc_iteration_counters": {}, "scc_spec_hashes": {}, "last_spec_hash": {}, "last_progress_snapshot": {}
        }"#;
        let _r2: ProjectVerificationState = serde_json::from_str(json_with_old_levels)
            .expect("旧含已删 levels key 应被忽略而非报错");
    }

    /// #112 fix 方案 C 反向回归：活字段（completion_map.status）缺失仍 loud-fail
    /// （确认反漂移对活字段保留，只放宽了 level 死字段）。
    #[test]
    fn project_state_missing_live_field_still_fails() {
        let json = r#"{
            "source_files": ["x.c"], "verification_order": ["f"], "current_index": 0,
            "global_notes": "", "scc_groups": [],
            "completion_map": {"f": {"last_attempt_at": null, "verified_source": null}},
            "feedback_pending": {}, "scc_iteration_counters": {}, "scc_spec_hashes": {},
            "last_spec_hash": {}, "last_progress_snapshot": {}
        }"#;
        assert!(
            serde_json::from_str::<ProjectVerificationState>(json).is_err(),
            "缺活字段 status 必须 loud-fail"
        );
    }

    /// 3.2.3 ProgramFailureEvidence 完整 schema serialize + jq 路径
    #[test]
    fn project_failure_evidence_full_schema() {
        let evidence = ProgramFailureEvidence {
            failure_type: "global_stuck".into(),
            details: "x".repeat(100),
            attempted_operations: vec![],
            unsound_specs: vec![],
            caller_request_infeasible: vec![],
            scc_oscillating: vec![],
            stuck_triples: vec![StuckTriple {
                caller: "a".into(),
                callee: "b".into(),
                request_type: "strengthen_ensures".into(),
                blocking_requires_or_required_ensures: "ensure result > 0".into(),
            }],
            failed_funcs: vec![],
            wp_invocation_errors: vec![],
            last_completed_level: Some(2),
        };
        let json = serde_json::to_value(&evidence).unwrap();

        // hard check 脚本读这些 jq 路径，确认字段名一致
        assert_eq!(json["failure_type"], "global_stuck");
        assert_eq!(json["details"].as_str().unwrap().len(), 100);
        assert_eq!(json["stuck_triples"][0]["caller"], "a");
        assert_eq!(json["stuck_triples"][0]["request_type"], "strengthen_ensures");
        assert_eq!(json["last_completed_level"], 2);

        // round-trip
        let restored: ProgramFailureEvidence = serde_json::from_value(json).unwrap();
        assert_eq!(restored.failure_type, "global_stuck");
        assert_eq!(restored.stuck_triples.len(), 1);
    }

    // --- Regression tests for GitHub #54: annotation_count stale metadata ---

    /// 正常流：sandbox 创建后添加注解，然后 store_conclusion 写入 specs，
    /// annotation_count 应与 specs.length 一致。
    /// 这在修复前后都应通过。
    #[test]
    fn annotation_count_syncs_on_normal_flow() {
        let mut state = SessionState::default();

        // 模拟 create_sandbox 副作用
        state.on_sandbox_created("bubble_sort", Some(17));
        let c = state.get_conclusion("bubble_sort").unwrap();
        assert_eq!(c.annotation_count, 0);
        assert!(c.specs.is_empty());

        // 模拟 5 次 add_annotation_sandbox + 每次后 store_conclusion 追加一条 spec
        for i in 0..5 {
            state.on_annotation_added("bubble_sort");
            state.store_conclusion(FunctionConclusionUpdate {
                function: "bubble_sort".into(),
                specs: Some(
                    (0..=i)
                        .map(|j| AnnotationEntry {
                            hash_label: format!("h{:03}", j),
                            user_label: None,
                            kind: "spec".into(),
                            acsl: format!("prop_{}", j),
                            stmt_id: None,
                            derived_from: format!("proposed_requires[{}]", j),
                            source: AnnotationSource::Generated,
                            purpose: "test".into(),
                            proof_target: None,
                            wp_status: None,
                            wp_time_ms: None,
                            wp_prover: None,
                        })
                        .collect(),
                ),
                ..Default::default()
            });
        }

        let c = state.get_conclusion("bubble_sort").unwrap();
        assert_eq!(c.specs.len(), 5);
        assert_eq!(c.annotation_count, 5);
    }

    /// 修复前失败、修复后应通过：Revision 缩减 specs 后，
    /// annotation_count 应自动同步到新的 specs.length。
    ///
    /// 复现 GitHub #54 的完整场景：
    ///   1. sandbox 创建，annotation_count=0
    ///   2. 14 条注解添加到 sandbox → annotation_count=14
    ///   3. store_conclusion 写入 14 条 spec → specs.length=14，一致
    ///   4. validate_acsl 拒绝 3 条排列相关 spec → Revision 移除
    ///   5. store_conclusion(specs=13条) → 修复前: annotation_count=14 ✗
    ///                                          修复后: annotation_count=13 ✓
    #[test]
    fn annotation_count_syncs_on_revision_reduce() {
        let mut state = SessionState::default();

        // 1. sandbox 创建
        state.on_sandbox_created("bubble_sort", Some(17));

        // 2. 模拟 14 条注解添加到 sandbox
        for _ in 0..14 {
            state.on_annotation_added("bubble_sort");
        }

        // 3. 初始 store_conclusion：14 条 spec
        let initial_specs: Vec<AnnotationEntry> = (0..14)
            .map(|i| AnnotationEntry {
                hash_label: format!("h{:03}", i),
                user_label: None,
                kind: "spec".into(),
                acsl: format!("prop_{}", i),
                stmt_id: None,
                derived_from: format!("proposed_ensures[{}]", i),
                source: AnnotationSource::Generated,
                purpose: "test".into(),
                proof_target: None,
                wp_status: None,
                wp_time_ms: None,
                wp_prover: None,
            })
            .collect();

        state.store_conclusion(FunctionConclusionUpdate {
            function: "bubble_sort".into(),
            specs: Some(initial_specs.clone()),
            ..Default::default()
        });
        let c = state.get_conclusion("bubble_sort").unwrap();
        assert_eq!(c.specs.len(), 14);
        assert_eq!(c.annotation_count, 14);

        // 4-5. Revision：移除第 1 条（模拟排列相关 spec 被 validate_acsl 拒绝）
        let revised_specs: Vec<AnnotationEntry> = initial_specs
            .into_iter()
            .enumerate()
            .filter(|&(i, _)| i != 1)
            .map(|(_, s)| s)
            .collect();

        state.store_conclusion(FunctionConclusionUpdate {
            function: "bubble_sort".into(),
            specs: Some(revised_specs.clone()),
            ..Default::default()
        });
        let c = state.get_conclusion("bubble_sort").unwrap();

        // 关键断言：annotation_count 必须与缩减后的 specs.length 一致
        assert_eq!(c.specs.len(), 13);
        assert_eq!(c.annotation_count, 13,
            "Revision 缩减 specs 后 annotation_count 应自动同步，否则硬检查 \
             '.annotation_count == (.specs | length)' 将失败（GitHub #54）");
    }

    /// 验证 JSON 序列化后 annotation_count 与 specs.length 一致，
    /// 确保硬检查脚本能从磁盘 JSON 读到正确的值。
    #[test]
    fn annotation_count_json_roundtrip_after_revision() {
        let mut state = SessionState::default();
        state.on_sandbox_created("f", Some(10));
        for _ in 0..5 {
            state.on_annotation_added("f");
        }

        // 初始 5 条 spec
        let specs: Vec<AnnotationEntry> = (0..5)
            .map(|i| AnnotationEntry {
                hash_label: format!("h{:03}", i),
                user_label: None,
                kind: "spec".into(),
                acsl: format!("p{}", i),
                stmt_id: None,
                derived_from: format!("proposed_requires[{}]", i),
                source: AnnotationSource::Generated,
                purpose: "test".into(),
                proof_target: None,
                wp_status: None,
                wp_time_ms: None,
                wp_prover: None,
            })
            .collect();

        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(),
            specs: Some(specs.clone()),
            ..Default::default()
        });

        // 缩减到 2 条
        let revised: Vec<AnnotationEntry> = specs.into_iter().take(2).collect();
        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(),
            specs: Some(revised),
            ..Default::default()
        });

        // 序列化 → 反序列化，验证 annotation_count 在 JSON 中正确
        let c = state.get_conclusion("f").unwrap().clone();
        let json = serde_json::to_string_pretty(&c).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        let ann_cnt = parsed["annotation_count"].as_u64().unwrap();
        let specs_len = parsed["specs"].as_array().unwrap().len() as u64;
        assert_eq!(ann_cnt, specs_len,
            "JSON 中 annotation_count 应与 specs.length 一致");
    }

    /// 边界情况：store_conclusion 不更新 specs 时（specs=None），
    /// annotation_count 不应被重置，应保持当前值。
    #[test]
    fn annotation_count_unchanged_when_specs_none() {
        let mut state = SessionState::default();
        state.on_sandbox_created("f", Some(5));
        for _ in 0..3 {
            state.on_annotation_added("f");
        }

        let specs: Vec<AnnotationEntry> = (0..3)
            .map(|i| AnnotationEntry {
                hash_label: format!("h{:03}", i),
                user_label: None,
                kind: "spec".into(),
                acsl: format!("p{}", i),
                stmt_id: None,
                derived_from: format!("proposed_requires[{}]", i),
                source: AnnotationSource::Generated,
                purpose: "test".into(),
                proof_target: None,
                wp_status: None,
                wp_time_ms: None,
                wp_prover: None,
            })
            .collect();

        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(),
            specs: Some(specs),
            ..Default::default()
        });
        assert_eq!(state.get_conclusion("f").unwrap().annotation_count, 3);

        // 只更新 status，不更新 specs → annotation_count 应保持 3
        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(),
            status: Some(VerificationStatus::Verified),
            specs: None, // 不更新 specs
            ..Default::default()
        });
        let c = state.get_conclusion("f").unwrap();
        assert_eq!(c.status, VerificationStatus::Verified);
        assert_eq!(c.annotation_count, 3,
            "不更新 specs 时 annotation_count 不应被重置");
    }

    /// 边界情况：空 specs 应使 annotation_count 归零。
    #[test]
    fn annotation_count_zero_on_empty_specs() {
        let mut state = SessionState::default();
        state.on_sandbox_created("f", Some(5));
        for _ in 0..7 {
            state.on_annotation_added("f");
        }

        state.store_conclusion(FunctionConclusionUpdate {
            function: "f".into(),
            specs: Some(vec![]),
            ..Default::default()
        });
        assert_eq!(state.get_conclusion("f").unwrap().annotation_count, 0);
    }
}
