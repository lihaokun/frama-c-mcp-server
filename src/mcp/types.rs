use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 宽容的 Vec 反序列化器：接受标准 JSON 数组，也接受被 stringify 的 JSON 数组
/// （Claude Code 的 MCP 客户端有时会把嵌套数组序列化成 string）。
///
/// 用法：`#[serde(default, deserialize_with = "deserialize_vec_or_string")]`
pub fn deserialize_vec_or_string<'de, D, T>(
    deserializer: D,
) -> Result<Option<Vec<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::de::Error;
    let v: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    let arr_value = match v {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(serde_json::Value::Array(_)) => v.unwrap(),
        Some(serde_json::Value::String(s)) => {
            // 空字符串当作 None（LLM 偶尔传 ""）
            if s.trim().is_empty() {
                return Ok(None);
            }
            let parsed: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
                D::Error::custom(format!(
                    "field passed as string but not valid JSON array: {}",
                    e
                ))
            })?;
            if !parsed.is_array() {
                return Err(D::Error::custom(
                    "field passed as string but parsed JSON is not an array",
                ));
            }
            parsed
        }
        Some(other) => {
            return Err(D::Error::custom(format!(
                "expected array or stringified JSON array, got {}",
                match other {
                    serde_json::Value::Object(_) => "object",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Bool(_) => "bool",
                    _ => "other",
                }
            )));
        }
    };
    let arr = arr_value.as_array().expect("checked above");
    arr.iter()
        .cloned()
        .map(|item| serde_json::from_value(item).map_err(D::Error::custom))
        .collect::<Result<Vec<T>, _>>()
        .map(Some)
}

/// 宽容的 JSON Value 反序列化器：接受任意 JSON value；如果是 string，则
/// 尝试 parse 它的内容（Claude Code 把对象 stringify 时用）。
///
/// 用法：`#[serde(default, deserialize_with = "deserialize_value_or_string")]`
pub fn deserialize_value_or_string<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => {
            if s.trim().is_empty() {
                return Ok(None);
            }
            let parsed: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
                D::Error::custom(format!(
                    "field passed as string but not valid JSON: {}",
                    e
                ))
            })?;
            Ok(Some(parsed))
        }
        Some(other) => Ok(Some(other)),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReloadProjectParams {
    /// C source file paths to reload. If omitted, reloads currently loaded files.
    /// Uses `deserialize_vec_or_string` to tolerate Claude Code MCP client偶发
    /// 把 array 序列化成 stringified JSON 的行为（同 store_function_conclusion 的
    /// specs / reference_specs 等已采用模式；用户报错 `invalid type: string ...
    /// expected a sequence` 即此 case）。
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub files: Option<Vec<String>>,
    /// Enable RTE (Runtime Error) annotation generation. When true, restarts Frama-C
    /// with -rte flag, which inserts assert annotations for signed overflow, division
    /// by zero, pointer validity, array bounds, etc. Use when no main() exists (EVA
    /// cannot run). Default: false.
    pub rte: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFunctionInfoParams {
    /// Function name to query
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetEvaAlarmsParams {
    /// Filter by function name
    pub function: Option<String>,
    /// Filter by alarm kind (e.g. "mem_access", "division_by_zero")
    pub alarm_kind: Option<String>,
    /// Filter by verification status
    pub status: Option<String>,
}

// P2.10: Added callstack support
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetEvaValueParams {
    /// Statement or expression marker (e.g. "#s2")
    pub marker: String,
    /// Callstack index (from getCallstacks). Omit for combined values.
    pub callstack: Option<u32>,
}

// P2.8: Added EVA parameters
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunEvaParams {
    /// EVA precision level (-1 to 11, default: current setting)
    pub precision: Option<i32>,
    /// Entry function name (default: "main")
    pub main_function: Option<String>,
    /// Loop unrolling level (default: current setting)
    pub slevel: Option<u32>,
}

// P2.9: Multi-function WP support
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunWpParams {
    /// Function name(s) to verify. If omitted, verifies all annotated functions.
    /// 同 ReloadProjectParams.files：用 helper 兼容 Claude Code stringified array。
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub functions: Option<Vec<String>>,
    /// SMT prover override. Default uses all three: Alt-Ergo + CVC5 + Z3. Only set to restrict to a single prover if needed.
    pub prover: Option<String>,
    /// Prover timeout in seconds (default: current setting)
    pub timeout: Option<u32>,
    /// WP memory model: "Bytes" (default, safe) or "Typed+nocast" (better for assigns/validity)
    pub model: Option<String>,
    /// Property filter (comma-separated). +name to include, -name to exclude.
    pub prop: Option<String>,
}

// --- Phase 2 new tool params ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetWpGoalsParams {
    /// Filter by function name
    pub function: Option<String>,
    /// Filter by status: "valid", "unknown", "timeout", "failed"
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetAnnotationsParams {
    /// Function name (required)
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindCallersParams {
    /// Function name to find callers of
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupSymbolParams {
    /// Identifier name (function, global variable)
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceCallChainParams {
    /// Starting function name
    pub function: String,
    /// Direction: "callers" (who calls me) or "callees" (who I call)
    pub direction: String,
    /// Max traversal depth (default 5, max 20)
    pub max_depth: Option<u32>,
    /// Stop at these function names
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub stop_at: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InvestigateAlarmParams {
    /// Property key from get_eva_alarms results (e.g. "#p10")
    pub property_key: String,
    /// Depth: "quick" (property only), "normal" (+ values + callers), "deep" (+ annotations)
    pub depth: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SuggestPlanParams {
    /// Focus target: "all", or a function name
    pub target: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunLinearInvariantParams {
    /// Transition system as JSON (variables, locations, transitions)
    pub input: serde_json::Value,
}

// --- Agent Phase 1 new tool params ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFunctionsParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListGlobalsParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDeclarationsParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFunctionAstParams {
    /// Function name
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidateAcslParams {
    /// Function name
    pub function: String,
    /// Annotation kind — the OCaml execAddAnnotation accepts only two buckets:
    ///   "spec"  — function contract clause (requires/ensures/assigns), no stmt
    ///   "annot" — statement-level annotation (loop invariant/assigns/variant,
    ///             assert), requires `stmt`.
    /// NOT per-clause names like "requires"/"loop_invariant" — those hit the
    /// dispatch default → error "unknown kind". (acsl_kind_to_ast_kind derives
    /// the right bucket from the acsl text for inject_all_*.)
    pub kind: String,
    /// ACSL annotation string
    pub acsl: String,
    /// Statement id (for statement-level annotations like assert, loop_invariant)
    pub stmt: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AddAnnotationParams {
    /// Function name
    pub function: String,
    /// Annotation kind — the OCaml execAddAnnotation accepts only two buckets:
    ///   "spec"  — function contract clause (requires/ensures/assigns), no stmt
    ///   "annot" — statement-level annotation (loop invariant/assigns/variant,
    ///             assert), requires `stmt`.
    /// NOT per-clause names like "requires"/"loop_invariant" — those hit the
    /// dispatch default → error "unknown kind". (acsl_kind_to_ast_kind derives
    /// the right bucket from the acsl text for inject_all_*.)
    pub kind: String,
    /// ACSL annotation string (without label — hash_label is auto-injected)
    pub acsl: String,
    /// Statement id (for statement-level annotations)
    pub stmt: Option<i64>,
    /// Optional semantic label (e.g. "bounds", "frame"). Injected after hash_label.
    pub user_label: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveAnnotationParams {
    /// Function name (removes all added annotations for this function)
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetVcDetailsParams {
    /// Function name to get WP verification condition details for
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HoareAnalyzeParams {
    /// Function name to analyze
    pub function: String,
    /// Callee context: pre-computed specs for called functions
    pub callee_context: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetHoareTraceParams {
    /// Function name
    pub function: String,
}

// --- Conclusion tools ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreFunctionConclusionParams {
    /// Function name (required)
    pub function: String,
    /// Status: "verified" | "failed" | "unsound" | "blocked_on_callee" | "in_progress"
    pub status: Option<String>,
    // NOTE (Plan A 收尾): long-text fields removed from store API:
    //   semantic_proof / semiformal_proof / program_summary
    // Long-text fields live ONLY in `.frama-c-mcp/<func>/<field>.md` files.
    // Use the Write/Edit tool to write them directly. The store API only handles
    // short / structured fields. See docs/fixes/remove-store-long-text-fields.md.
    // NOTE: `analysis_summary` was a 4th long-text field, removed 2026-05-26
    // due to Claude Code subagent guard regex collision; content moved into
    // semiformal_proof.md `## function_summary` section.
    // See docs/fixes/rename-analysis-summary-subagent-guard.md.
    /// Committed annotations (含 hash_label / kind / acsl / stmt_id / wp_status / derived_from)
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub specs: Option<Vec<serde_json::Value>>,
    /// Reference annotations (元信息：曾经设计但 degrade 到 reference，不进 verified.c，不影响 status)
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub reference_specs: Option<Vec<serde_json::Value>>,
    /// Per-goal WP results
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub wp_results: Option<Vec<serde_json::Value>>,
    /// Free-form notes
    pub notes: Option<String>,
    /// WP goal summary {total, valid, unknown, timeout, failed, model, timeout_used, recorded_at_retry, failed_goal_labels, failed_source_asserts}
    #[serde(default, deserialize_with = "deserialize_value_or_string")]
    pub wp_summary: Option<serde_json::Value>,

    // --- S1_info_gather outputs ---
    /// Callee names list
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub callees: Option<Vec<String>>,
    /// Callee status + sources (key: callee name → CalleeInfo {status, sources})
    #[serde(default, deserialize_with = "deserialize_value_or_string")]
    pub callee_info: Option<serde_json::Value>,
    /// Source asserts existing in code: [{stmt_id, acsl, origin}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub existing_asserts: Option<Vec<serde_json::Value>>,

    // --- S2.5 step 12 structured spec proposals ---
    /// Named behavior declarations: [{name, assumes: [...]}].
    /// Other clauses reference these by `behavior: "<name>"`.
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_behaviors: Option<Vec<serde_json::Value>>,
    /// [{acsl, behavior?, necessity}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_requires: Option<Vec<serde_json::Value>>,
    /// [{acsl, from, behavior?}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_ensures: Option<Vec<serde_json::Value>>,
    /// [{acsl, behavior?}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_assigns: Option<Vec<serde_json::Value>>,
    /// [{stmt_id, loop_label, invariants: [{acsl, behavior?}], assigns: [{acsl, behavior?}], variant?: {acsl, behavior?}}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_loop_annots: Option<Vec<serde_json::Value>>,
    /// fsmint-6: 函数级 terminates waiver，{acsl: "terminates \\false", derived_from: "termination_waived"}。
    /// S_prepare（prove_termination=false 时）经本工具写入 conclusion，供 inject_all sandbox/main 注入。
    #[serde(default)]
    pub proposed_terminates: Option<serde_json::Value>,

    // --- callee_gap path ---
    /// callee_requests: [{callee, required_property, reason}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub callee_requests: Option<Vec<serde_json::Value>>,

    // --- Revision counters（替代 sp_revision_history / proposed_revision_history Vec） ---
    /// SP revision 已尝试轮次（防 runaway loop；hard check 限 ≤ N 轮）
    #[serde(default)]
    pub sp_revision_count: Option<u32>,
    /// 上轮 SP revision 的错误分析（LLM 下轮 prompt context，每轮覆盖）
    #[serde(default)]
    pub last_sp_error_analysis: Option<String>,
    /// proposed revision 已尝试轮次
    #[serde(default)]
    pub proposed_revision_count: Option<u32>,
    /// 上轮 proposed revision 的错误分析
    #[serde(default)]
    pub last_proposed_error_analysis: Option<String>,

    // --- failure path (status=failed/unsound) ---
    /// failure_evidence: {type, location, acsl, unsupported_predicate?, attempted_reformulations[], counterexample?}
    #[serde(default, deserialize_with = "deserialize_value_or_string")]
    pub failure_evidence: Option<serde_json::Value>,
    /// unsound_specs (status=unsound only): [{hash_label, kind, acsl, stmt_id, counterexample, removed_at_retry}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub unsound_specs: Option<Vec<serde_json::Value>>,

    // --- S5_output ---
    /// Path to verified.c (.frama-c-mcp/verified/<func>_verified.c)
    pub verified_source: Option<String>,

    // --- verify-program-fsm v1 接入 (detailed-design §6.4) ---
    /// status=unsound 时的子分类:
    ///   "internal_logic_error" / "callee_requires_too_strict" / "other"
    pub unsound_reason_type: Option<String>,
    /// 仅 unsound_reason_type="callee_requires_too_strict" 时填:
    /// {callee, blocking_requires, caller_state_at_site, strengthening_attempt, evidence}
    #[serde(default, deserialize_with = "deserialize_value_or_string")]
    pub blocking_callee_requires: Option<serde_json::Value>,
    /// status="caller_request_infeasible" 时填:
    /// [{caller, rejected_request_type, rejected_requires, infeasibility_reason}, ...]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub infeasible_requests: Option<Vec<serde_json::Value>>,

    // --- cross-FSM injection ---
    // NOTE (Plan A 收尾): program_summary moved to file-based storage.
    // Use Write tool on `.frama-c-mcp/<func>/program_summary.md` (cross-FSM
    // caller flow). See docs/fixes/remove-store-long-text-fields.md.

    // --- audit ---
    /// 设为 true 时把当前 conclusion 快照 push 到 conclusion_history（修复前用）
    #[serde(default)]
    pub push_history: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFunctionConclusionParams {
    /// Function name
    pub function: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListConclusionsParams {
    /// Filter by status: "verified" | "failed" | "unsound" | "blocked_on_callee" | "in_progress". Omit for all.
    pub status: Option<String>,
}

// --- Project state tools ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StoreProjectStateParams {
    /// Full `ProjectVerificationState` as a JSON string (main path, used by vp-fsm).
    /// When present, the whole project state is replaced (full-replace) and persisted
    /// to `.frama-c-mcp/_program.json`; the server-owned `locked` field is preserved
    /// from the current in-memory state (lock_project/unlock_project own it).
    /// Takes priority over the four thin fields below.
    pub state_json: Option<String>,

    // --- thin-variant compatibility fields (legacy callers / simple updates) ---
    /// Source files loaded
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub source_files: Option<Vec<String>>,
    /// Function verification order (topological sort)
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub verification_order: Option<Vec<String>>,
    /// Current progress index
    pub current_index: Option<usize>,
    /// Global notes
    pub global_notes: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetProjectStateParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LockProjectParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnlockProjectParams {}

// --- Print source ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PrintSourceParams {
    /// Output file path. If omitted, returns the source as text in the response.
    pub output: Option<String>,
    /// Sandbox name (e.g. "abc123:func"). If provided, prints from the sandbox instance.
    pub sandbox_name: Option<String>,
}

// --- Sandbox tools ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateSandboxParams {
    /// Function name to create sandbox copy of
    pub function: String,
    /// Optional experiment ID. If provided, sandbox_name = "{experiment_id}:{function}"
    /// and the sandbox is registered under this ID. Useful when the caller (e.g. an FSM
    /// session) already chose a stable, human-readable ID. Must be unique across active
    /// sandboxes — collision returns an error. If omitted, server generates a random ID.
    pub experiment_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResetSandboxParams {
    /// Sandbox function name (e.g. "__sandbox__copy_counter_a3f2e1b7")
    pub sandbox_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteSandboxParams {
    /// Sandbox function name to delete
    pub sandbox_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractAnnotationsParams {
    /// Sandbox function name to extract annotations from
    pub sandbox_name: String,
}

// --- inject_all_annotations_sandbox (schema v2) ---
//
// Input shape: structured proposed_* from S2.5 conclusion. Named behaviors are
// declared once in `proposed_behaviors` and referenced by name from
// requires/ensures/assigns/loop_*. inject_all wraps each entry independently:
//   - behavior=None             → top-level `requires R;` / `assigns Y;` / ...
//   - behavior=Some("X")        → look up X's assumes, emit
//                                 `behavior X: assumes A1; <clause>;`
//                                 (loop clauses use `for X: ...`).
//   - behavior referenced but not declared → InjectionFailure ProposedError.
//
// Field types are Option<Vec<serde_json::Value>> so rmcp JsonSchema exposes
// flexible shapes; server.rs parses each entry into typed state.rs structs
// (ProposedBehavior / ProposedRequires / ProposedEnsures / ProposedAssigns /
// ProposedLoopAnnot) for strong validation + per-entry failure attribution.

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InjectAllAnnotationsSandboxParams {
    /// Sandbox name (e.g. "exp42:func"). Must include experiment_id prefix.
    pub sandbox_name: String,
    /// Named behavior declarations: [{name, assumes: [...]}].
    /// Other clauses reference these by `behavior: "<name>"`. Undeclared reference
    /// → that entry fails with ProposedError. Empty assumes ⇒ ACSL `assumes \true`.
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_behaviors: Option<Vec<serde_json::Value>>,
    /// [{acsl, behavior?, necessity}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_requires: Option<Vec<serde_json::Value>>,
    /// [{acsl, from, behavior?}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_ensures: Option<Vec<serde_json::Value>>,
    /// [{acsl, behavior?}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_assigns: Option<Vec<serde_json::Value>>,
    /// [{stmt_id, loop_label, invariants: [{acsl, behavior?}], assigns: [{acsl, behavior?}], variant?: {acsl, behavior?}}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_loop_annots: Option<Vec<serde_json::Value>>,
    /// Function-level terminates clause (single, not a list): `{acsl: "terminates \\false"}` or null.
    /// fsmint-6: when prove_termination=false the function waives termination; this carries the
    /// `terminates \\false` clause through the same structured channel as the other proposed_*,
    /// so it merges to main. Injected as a funspec clause via add_spec (~force:true overrides the
    /// kernel-default `terminates \\true`). See docs/fixes/fsmint6-terminates-injection-fix-second-block.md.
    #[serde(default)]
    pub proposed_terminates: Option<serde_json::Value>,
}

/// proposed_*.acsl may be bare (`x < 2`) or already carry the clause keyword
/// (`requires x < 2`) — inject_all normalizes a leading dup keyword before
/// wrapping, so both forms are accepted (see inject-all-wrap-double-keyword.md).
///
/// Params for `inject_all_annotations_main`: same structured `proposed_*` as the
/// sandbox variant, but targets a MAIN-instance function (bare name, no `:`).
/// Used by vp-fsm S2 merge to write a verified callee's contract to main from
/// the conclusion's ground-truth proposed_* (mechanism C). Shares the plan-build
/// + injection logic with the sandbox variant → bit-identical ACSL to what was
/// verified (see docs/fixes/vp-fsm-s2merge-add-annotation-main-toolname.md §5 O2).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InjectAllAnnotationsMainParams {
    /// Main-instance function name. Must NOT include an experiment_id prefix (`:`).
    pub function: String,
    /// Named behavior declarations: [{name, assumes: [...]}]. See sandbox variant.
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_behaviors: Option<Vec<serde_json::Value>>,
    /// [{acsl, behavior?, necessity}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_requires: Option<Vec<serde_json::Value>>,
    /// [{acsl, from, behavior?}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_ensures: Option<Vec<serde_json::Value>>,
    /// [{acsl, behavior?}]
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_assigns: Option<Vec<serde_json::Value>>,
    /// [{stmt_id, loop_label, invariants, assigns, variant?}]. stmt_id is a sandbox
    /// sid; the main injection re-resolves loops to main sids by source order (O3).
    #[serde(default, deserialize_with = "deserialize_vec_or_string")]
    pub proposed_loop_annots: Option<Vec<serde_json::Value>>,
    /// Function-level terminates clause (single): `{acsl: "terminates \\false"}` or null.
    /// fsmint-6: vp-fsm S2 merge passes `conc.proposed_terminates` so the waived termination
    /// propagates to merged main (else main re-emits unprovable kernel-default `terminates \\true`
    /// for looping functions → final-gate spurious FAIL). See sandbox variant + the fix doc.
    #[serde(default)]
    pub proposed_terminates: Option<serde_json::Value>,
}

/// Failure type classification for ACSL injection errors.
///
/// The upstream `S2_5_revise_proposed` agent uses this to choose how to
/// repair the spec: surface-level rewrite (SyntaxError),
/// scope/name correction (ProposedSelfReferential or
/// ProposedLocalVarInFunspec), or design rethink (ProposedError).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureType {
    /// ACSL syntax/parse error (e.g. unknown keyword, malformed expression).
    /// Triggered by lexer-level or parser-level failure.
    SyntaxError,
    /// References an undefined name: logic variable/predicate/function/type,
    /// unknown enum/struct/union, unknown logic label, unknown behavior, etc.
    /// Agent should fix the name or remove the reference.
    ProposedSelfReferential,
    /// Funspec (function-level contract) references a function local variable,
    /// violating ACSL §2.3 which restricts function-level contracts to
    /// caller-visible state (formals, globals, \result, \old(formal)).
    /// Agent should replace with the caller-visible state being modified
    /// (e.g. `assigns i, j` → `assigns arr[0..n-1]`).
    ProposedLocalVarInFunspec,
    /// Other proposed design error: type mismatch, invalid cast, non-lvalue
    /// in assigns, duplicate behavior, etc. May require design rethink.
    ProposedError,
}

/// A successfully injected annotation. Structurally compatible with AnnotationEntry
/// for direct use as store_function_conclusion(specs=<successful array>).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InjectedAnnotationEntry {
    pub hash_label: String,
    pub user_label: Option<String>,
    /// Top-level binary kind: "spec" (function-level requires/ensures/assigns)
    /// or "annot" (stmt-level loop_invariant/loop_assigns/loop_variant/assert)
    pub kind: String,
    /// Full ACSL clause text (e.g. "requires P;" or "loop invariant Q;")
    pub acsl: String,
    /// null for kind="spec"; stmt_id for kind="annot"
    pub stmt_id: Option<i64>,
    /// Must match proposed_* JSON path (e.g. "proposed_requires[0]")
    pub derived_from: String,
    pub source: String,
    /// One-line reason for this annotation
    pub purpose: String,
    pub proof_target: Option<String>,
    pub wp_status: Option<serde_json::Value>,
    pub wp_time_ms: Option<u64>,
    pub wp_prover: Option<String>,
}

/// A single injection failure with classified error type.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InjectionFailure {
    /// Classified failure type (see FailureType)
    #[serde(rename = "type")]
    pub failure_type: FailureType,
    /// The proposed_* JSON path that caused this failure
    pub proposed_path: String,
    /// The ACSL text that was attempted
    pub acsl_text: String,
    /// Raw error message from Frama-C CLI pre-check
    pub frama_c_error: String,
}

/// Summary counts for the injection operation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InjectionSummary {
    pub total_attempted: usize,
    pub successful_count: usize,
    pub failure_count: usize,
}

/// Response from inject_all_annotations_sandbox.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InjectAllAnnotationsSandboxResponse {
    /// "success" (no failures), "partial" (only SyntaxError failures),
    /// or "proposed_error" (any ProposedSelfReferential or ProposedError)
    pub status: String,
    /// Successfully injected annotations (compatible with AnnotationEntry)
    pub successful: Vec<InjectedAnnotationEntry>,
    /// Failed injections with error classification
    pub failures: Vec<InjectionFailure>,
    pub summary: InjectionSummary,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComputeTopologicalOrderParams {}

/// fsmint-3 依赖驱动调度：`get_ready_functions` 入参（纯函数，状态全由参数传入）。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetReadyFunctionsParams {
    /// 契约已 merge 进 main 可被消费的函数（v-p 从 completion_map 提 {completed, failed_merged}）
    pub done: Vec<String>,
    /// 当前在跑/已派未回的函数（排除，不重复派）
    pub in_progress: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct VecHolder {
        #[serde(default, deserialize_with = "deserialize_vec_or_string")]
        items: Option<Vec<String>>,
    }

    #[derive(Debug, Deserialize)]
    struct ValueHolder {
        #[serde(default, deserialize_with = "deserialize_value_or_string")]
        v: Option<serde_json::Value>,
    }

    fn parse_vec(json: &str) -> Result<Option<Vec<String>>, String> {
        let h: VecHolder = serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(h.items)
    }

    fn parse_value(json: &str) -> Result<Option<serde_json::Value>, String> {
        let h: ValueHolder = serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(h.v)
    }

    #[test]
    fn vec_accepts_real_array() {
        assert_eq!(
            parse_vec(r#"{"items": ["a", "b"]}"#).unwrap(),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn vec_accepts_stringified_array() {
        assert_eq!(
            parse_vec(r#"{"items": "[\"a\", \"b\"]"}"#).unwrap(),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn vec_accepts_empty_string_as_none() {
        assert_eq!(parse_vec(r#"{"items": ""}"#).unwrap(), None);
    }

    #[test]
    fn vec_accepts_null() {
        assert_eq!(parse_vec(r#"{"items": null}"#).unwrap(), None);
    }

    #[test]
    fn vec_accepts_missing() {
        assert_eq!(parse_vec(r#"{}"#).unwrap(), None);
    }

    #[test]
    fn vec_rejects_non_array_string() {
        let err = parse_vec(r#"{"items": "not json"}"#).unwrap_err();
        assert!(err.contains("not valid JSON"), "got: {}", err);
    }

    #[test]
    fn vec_rejects_object_string() {
        let err = parse_vec(r#"{"items": "{}"}"#).unwrap_err();
        assert!(err.contains("not an array"), "got: {}", err);
    }

    #[test]
    fn value_accepts_object() {
        let v = parse_value(r#"{"v": {"k": 1}}"#).unwrap();
        assert_eq!(v, Some(serde_json::json!({"k": 1})));
    }

    #[test]
    fn value_accepts_stringified_object() {
        let v = parse_value(r#"{"v": "{\"k\": 1}"}"#).unwrap();
        assert_eq!(v, Some(serde_json::json!({"k": 1})));
    }

    #[test]
    fn value_accepts_empty_string_as_none() {
        assert_eq!(parse_value(r#"{"v": ""}"#).unwrap(), None);
    }
}
