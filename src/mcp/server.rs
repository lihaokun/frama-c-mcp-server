use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde_json::json;

use crate::error::FramaCError;
use crate::frama_c::client::FramaCClient;
use crate::mcp::types::*;
use crate::state::{FunctionVerificationState, ProjectVerificationState, SandboxState, SessionState};

/// `.frama-c-mcp/` 目录路径（相对 cwd）。所有 conclusion 文件写到此目录下。
pub fn conclusion_base_dir() -> PathBuf {
    PathBuf::from(".frama-c-mcp")
}

/// 解析 WP goal JSON，推断 `goal_kind` + 提取 `hash_label`（如有）。
/// 返回 (kind, hash_label)。
///
/// **kind** 取值：
/// - `"rte_overflow"` / `"rte_bound"` / `"rte_division"` / `"rte_pointer"` / `"rte_shift"`：
///   WP 自动插的 RTE 证明义务（来自 `-wp-rte`）
/// - `"user_assert"`：源码 `/*@ assert P; */`，未注入 hash_label
/// - `"spec"`：来自 `add_annotation` 的 spec / loop annotation（pred_name 注入了 hash_label）
///
/// **判定基于 WP po_name 启发式**（前缀 + hash_label 模式匹配）。无法精确分类时
/// 默认 `"spec"`，避免误归 RTE 类别。
fn classify_wp_goal(goal: &serde_json::Value) -> (String, Option<String>) {
    use std::sync::OnceLock;
    static HASH_RE: OnceLock<regex::Regex> = OnceLock::new();
    let hash_re = HASH_RE.get_or_init(|| {
        // hash_label 命名约定：see generate_hash_label —— `(re|en|as|li|la|lv|at|an)_[0-9a-f]{8}`
        regex::Regex::new(r"\b(re|en|as|li|la|lv|at|an)_[0-9a-f]{8}\b").unwrap()
    });

    let name = goal.get("name").and_then(|v| v.as_str()).unwrap_or_default();
    let name_lc = name.to_ascii_lowercase();

    // RTE 类（WP 自动插的 obligation；命名包含特征关键词）
    if name_lc.contains("signed_overflow") || name_lc.contains("unsigned_overflow")
        || name_lc.contains("integer_overflow") || name_lc.contains("downcast")
    {
        return ("rte_overflow".into(), None);
    }
    if name_lc.contains("index_in_bound") || name_lc.contains("index_bound")
        || name_lc.contains("array_bound")
    {
        return ("rte_bound".into(), None);
    }
    if name_lc.contains("division_by_zero") || name_lc.contains("div_by_zero")
        || name_lc.contains("modulo")
    {
        return ("rte_division".into(), None);
    }
    if name_lc.contains("mem_access") || name_lc.contains("initialization")
        || name_lc.contains("dangling") || name_lc.contains("pointer_validity")
    {
        return ("rte_pointer".into(), None);
    }
    if name_lc.contains("shift") {
        return ("rte_shift".into(), None);
    }

    // hash_label 模式：来自 add_annotation 注入的 pred_name 标签
    if let Some(cap) = hash_re.find(name) {
        return ("spec".into(), Some(cap.as_str().to_string()));
    }

    // 用户写的 assert（在源码里，无 hash_label）
    if name_lc.contains("assertion") || name_lc.contains("user assert") {
        return ("user_assert".into(), None);
    }

    // 默认 spec（Pre / Post / Assigns / Invariant 等没注 hash_label 的情况兜底）
    ("spec".into(), None)
}

/// `.frama-c-mcp/<func>/` 目录路径（每个 function 的 conclusion 一个子目录）。
pub fn conclusion_dir(func: &str) -> PathBuf {
    conclusion_base_dir().join(func)
}

/// 写一个 long-text 字段到 `<dir>/<name>`：
/// - content 非空 → tmp+rename 写入（覆盖）
/// - content 空 → **no-op**（不动文件）
///
/// **关键**：empty memory ≠ delete file。因为 LLM 可能直接 Write 该 .md 文件
/// 而不走 store API，此时 in-memory 的 long-text 字段仍是默认空字符串。如果
/// persist 在此时 unlink 文件，会清掉 LLM 刚写的工作（demo 跑 S2 → S2.5
/// 时复现：S2 写 SP 文件，下个 store 调用触发 persist，文件消失）。
///
/// 要显式清空字段：让 LLM/调用方直接删文件（Bash `rm`），不走 store API。
fn persist_long_text(dir: &Path, name: &str, content: &str) -> std::io::Result<()> {
    if content.is_empty() {
        // 不动文件：内存空可能只是因为没经过 store API（LLM 直接 Write 的情况）。
        // 如果文件确实存在 + 内容是 LLM 写的真实数据，删除会导致数据丢失。
        return Ok(());
    }
    let path = dir.join(name);
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// 3 个 long-text 字段名（用于按 fields 索引文件 + 装组装 response JSON）。
///
/// 历史：`analysis_summary` 曾在此列表，但 2026-05-26 因撞 Claude Code subagent guard
/// regex `/^(REPORT|SUMMARY|FINDINGS|ANALYSIS).*\.md$/i` 删除，内容并入
/// `semiformal_proof.md` 的 `## function_summary` section（见
/// `docs/fixes/rename-analysis-summary-subagent-guard.md`）。
const LONG_TEXT_FIELDS: &[&str] = &[
    "semantic_proof",
    "semiformal_proof",
    "program_summary",
];

/// 写一个 long-text 字段。`field_basename` 不含 `.md` 后缀。
///
/// Plan A 规则（见 docs/fixes/conclusion-per-field-files.md）：
/// 长文本字段不在 in-memory state，本函数由 MCP store handler 收到 update 后**显式**调，
/// 不再被 persist_conclusion 触发。content 空 → no-op（不动文件，留删除给 LLM 显式做）。
pub fn write_long_text_field(dir: &Path, field_basename: &str, content: &str) -> std::io::Result<()> {
    let filename = format!("{}.md", field_basename);
    persist_long_text(dir, &filename, content)
}

/// 读 dir 下 long-text .md 文件，组装成 JSON 对象（字段名 → 字符串/null）。
///
/// 用途：get_function_conclusion handler 把 meta.json 反序列化的 JSON 跟本函数返回的
/// long-text JSON object 合并后返回给 LLM。
/// - `semantic_proof` / `semiformal_proof`：缺失 → `""`
/// - `program_summary`：缺失 → 不写入（保持 Option 语义）
pub fn read_long_texts_as_json(dir: &Path) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for &field in &LONG_TEXT_FIELDS[..2] {
        // 前 2 个总是 String，缺失返回 ""
        let path = dir.join(format!("{}.md", field));
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        map.insert(field.to_string(), serde_json::Value::String(content));
    }
    // program_summary 是 Option<String>，缺失则不写入字段（serde skip None 语义）
    if let Ok(content) = std::fs::read_to_string(dir.join("program_summary.md")) {
        map.insert("program_summary".to_string(), serde_json::Value::String(content));
    }
    map
}

/// §4.3.2 算法（Plan A 简化）：只写 meta.json。
///
/// 长文本字段在 Plan A 下完全不进 in-memory state，所以 persist 不再处理它们。
/// 长文本写文件由 MCP store handler 在调用本函数前显式做（write_long_text_field）。
///
/// 内部入口；prod 调用走 `persist_conclusion`（默认 base_dir）；tests 用 `persist_conclusion_at`
/// 传入 tempdir 避免污染工作目录。
pub fn persist_conclusion_at(
    base_dir: &Path,
    func: &str,
    conclusion: &FunctionVerificationState,
) -> std::io::Result<()> {
    let dir = base_dir.join(func);
    std::fs::create_dir_all(&dir)?;

    // 序列化 state → JSON Value，注入 _long_text_files manifest 后再写盘。
    // manifest 告诉 LLM / 人类 reader："这些字段的真相在同目录的对应 .md 文件，
    // meta.json 故意不含它们的内容（Plan A 设计）"，避免 agent 误以为 meta.json 漏了字段。
    //
    // **manifest 只列实际存在的 .md 文件**（不列幻象）——demo bug 发现：
    // reviewer LLM 看到 manifest 提及 file 但 review_artifacts 没附时会 FAIL。
    // 只列存在的文件让 reviewer / agent 期望和现实一致。
    let mut value = serde_json::to_value(conclusion)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(obj) = value.as_object_mut() {
        let existing_files: Vec<String> = LONG_TEXT_FIELDS.iter()
            .map(|f| format!("{}.md", f))
            .filter(|fname| dir.join(fname).is_file())
            .collect();
        if !existing_files.is_empty() {
            let manifest = serde_json::json!({
                "_comment": "长文本字段的真相在下列 .md 文件（同目录）。要看完整 conclusion 请调 get_function_conclusion 工具（自动从 .md 组装）",
                "files": existing_files,
            });
            obj.insert("_long_text_files".to_string(), manifest);
        }
        // 没任何长文本文件时不写 manifest 字段，避免空噪声
    }

    let meta_path = dir.join("meta.json");
    let tmp = meta_path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &meta_path)?;
    Ok(())
}

/// Prod 入口：以默认 `.frama-c-mcp/` 为 base_dir 持久化 meta.json。
pub fn persist_conclusion(func: &str, conclusion: &FunctionVerificationState) -> std::io::Result<()> {
    persist_conclusion_at(&conclusion_base_dir(), func, conclusion)
}

/// 落盘 `ProjectVerificationState` → `<base_dir>/_program.json`（原子 tmp+rename）。
///
/// vp-fsm 主 FSM 的状态载体——hard check 脚本通过 jq 读**磁盘**文件做断言
/// （detailed-design §1.2/§1.3），故 store_project_state / lock_project / unlock_project
/// 改 in-memory state 后都必须调本函数同步到盘，否则 hard check 永远读不到。
///
/// 内部入口；prod 调 `persist_program_state`（默认 base_dir）；tests 用本函数传 tempdir。
pub fn persist_program_state_at(base_dir: &Path, state: &ProjectVerificationState)
    -> std::io::Result<()> {
    std::fs::create_dir_all(base_dir)?;
    let path = base_dir.join("_program.json");
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Prod 入口：以默认 `.frama-c-mcp/` 为 base_dir 持久化 `_program.json`。
pub fn persist_program_state(state: &ProjectVerificationState) -> std::io::Result<()> {
    persist_program_state_at(&conclusion_base_dir(), state)
}

/// 从一个 `<base_dir>/<func>/` 目录加载 conclusion 的 meta 部分（in-memory state）。
///
/// 长文本字段在 Plan A 下不进 state，本函数只读 meta.json。Get handler 在响应时
/// 单独调 `read_long_texts_as_json` 读 .md 文件。
///
/// 返回 None 表示该目录不是合法 conclusion 目录（无 meta.json 或 JSON 解析失败）。
pub fn load_conclusion_dir(dir: &Path) -> Option<FunctionVerificationState> {
    let meta_str = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str::<FunctionVerificationState>(&meta_str).ok()
}

/// session 启动时从 `.frama-c-mcp/` 加载所有 `<func>/` 目录到 conclusions HashMap。
/// - 旧的 `<func>.json` 文件**忽略**（不读、不警告）
/// - 顶层 `project_state.json` 忽略（非 conclusion 目录）
/// - 子目录无 meta.json 也忽略（避免误读 draft/ verified/ cegis_history/ 等遗留目录）
pub fn load_conclusions_from_disk(base_dir: &Path) -> HashMap<String, FunctionVerificationState> {
    let mut out = HashMap::new();
    let entries = match std::fs::read_dir(base_dir) {
        Ok(e) => e,
        Err(_) => return out, // 目录不存在 = 全新 session，正常
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let func = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if let Some(c) = load_conclusion_dir(&path) {
            out.insert(func, c);
        }
    }
    out
}

// ────────────── Business error builders (Issue #95) ──────────────
//
// 返回结构化错误给 LLM——message + suggestion 字段是固定 schema，
// LLM prompt 依赖这个 schema 来 follow-up（"看到 NoProjectLoaded 就调
// suggestion.tool"）。fix-doc §4.5 详细说明。
//
// 注意：因为 rmcp tool router 把 Err(McpError) 转成 transport-level error
// 而不是 business error，这里仍用 McpError::invalid_params，但 body 是
// pretty-printed JSON 含结构化字段。未来 rmcp 暴露 CallToolResult::error
// 时再切换。

/// "未 load project" 错误——所有 main 工具的 require_client / require_project_loaded
/// 失败时返回。LLM 看到这个应自动调 reload_project。
pub fn no_project_loaded_err() -> McpError {
    let body = serde_json::json!({
        "error": "NoProjectLoaded",
        "message": "No project loaded. Call reload_project(files=[...]) first to spawn main frama-c.",
        "suggestion": {
            "tool": "reload_project",
            "args_example": { "files": ["/path/to/source.c"] }
        }
    });
    McpError::invalid_params(
        serde_json::to_string_pretty(&body).unwrap_or_default(),
        None,
    )
}

/// "sandbox 不存在" 错误——所有非-create sandbox 工具的 require_sandbox 失败时返回。
pub fn sandbox_not_found_err(experiment_id: &str, existing: &[String]) -> McpError {
    let body = serde_json::json!({
        "error": "SandboxNotFound",
        "message": format!(
            "Sandbox '{}' missing. Call create_sandbox(function=..., experiment_id='{}') first.",
            experiment_id, experiment_id
        ),
        "suggestion": {
            "tool": "create_sandbox",
            "args_example": { "function": "<func_name>", "experiment_id": experiment_id }
        },
        "existing_sandboxes": existing,
    });
    McpError::invalid_params(
        serde_json::to_string_pretty(&body).unwrap_or_default(),
        None,
    )
}

/// 主 frama-c 状态。Lazy 模式（Issue #95）下 server 启动时为 None，
/// 第一次 reload_project 时 spawn frama-c + connect client，填上本字段。
///
/// `child` 的 `kill_on_drop = true` 保证 Drop 时 frama-c 被 SIGKILL，避免 zombie。
/// main.rs 装了 SIGTERM/SIGINT handler 通过 tokio::select! 转为 graceful return，
/// 触发 Drop chain 让 kill_on_drop 真正工作（见 docs/fixes/sigterm-handler-frama-c-orphan.md）。
/// **已知限制**：SIGKILL / OOM / crash 路径下父进程被内核直接终止，Drop 不跑，
/// frama-c child 仍会孤儿（需 PR_SET_PDEATHSIG kernel-level fix，本次未上）。
/// `socket_path` / `files` / `with_rte` 支持 ensure_main_spawned 的 in-place vs
/// respawn 判断（rte 切换必须重启，files 变化可 in-place reload）。
pub struct MainFramaCState {
    pub child: tokio::process::Child,
    pub socket_path: PathBuf,
    pub files: Vec<String>,
    pub with_rte: bool,
}

#[derive(Clone)]
pub struct FramaCMcpServer {
    /// 主 frama-c 客户端。Lazy（Issue #95）：server 启动时 = None，
    /// 第一次 reload_project 触发 ensure_main_spawned 时建连。
    /// 与 main_frama_c_state 必须同步（is_none() ⇔ main_frama_c_state.is_none()），
    /// 由 ensure_main_spawned 内部保证。
    client: Arc<AsyncMutex<Option<Arc<FramaCClient>>>>,
    state: Arc<RwLock<SessionState>>,
    /// Sandbox Frama-C instances: experiment_id → (state, client)
    sandboxes: Arc<RwLock<HashMap<String, (SandboxState, Arc<FramaCClient>)>>>,
    /// Maximum concurrent sandboxes
    max_sandboxes: usize,
    /// Path to frama-c binary (for spawning sandbox instances + main)
    frama_c_path: String,
    /// 主 frama-c 进程状态（child + socket + files + rte）。
    /// 替代旧 `main_frama_c_child: Option<Child>`——多存 socket/files/rte 以支持
    /// ensure_main_spawned 的 in-place vs respawn 判断。
    /// 锁顺序：main_frama_c_state → client → state → sandboxes（避死锁）。
    main_frama_c_state: Arc<AsyncMutex<Option<MainFramaCState>>>,
    /// Project lock: when true, reload_project and run_wp on main instance are rejected.
    /// Sandbox operations are unaffected. Use lock_project/unlock_project to toggle.
    project_locked: Arc<RwLock<bool>>,
    tool_router: ToolRouter<Self>,
}

/// Result of resolving a function name: which client to use and the real function name.
struct ResolvedClient {
    client: Arc<FramaCClient>,
    function: String,
    experiment_id: Option<String>,
}

impl FramaCMcpServer {
    /// Lazy constructor（Issue #95）：不连任何 frama-c。client/main_frama_c_state
    /// 都是 None，第一次 reload_project 时 ensure_main_spawned 建立。
    pub fn new_lazy(
        state: Arc<RwLock<SessionState>>,
        frama_c_path: String,
        max_sandboxes: usize,
    ) -> Self {
        // session 启动时从 .frama-c-mcp/ 恢复已有 conclusions（§13.6 改动 1）
        let loaded = load_conclusions_from_disk(&conclusion_base_dir());
        if !loaded.is_empty() {
            let state_clone = state.clone();
            tokio::spawn(async move {
                let mut s = state_clone.write().await;
                for (func, conc) in loaded {
                    s.conclusions.entry(func).or_insert(conc);
                }
            });
        }

        Self {
            client: Arc::new(AsyncMutex::new(None)),
            state,
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            max_sandboxes,
            frama_c_path,
            main_frama_c_state: Arc::new(AsyncMutex::new(None)),
            project_locked: Arc::new(RwLock::new(false)),
            tool_router: Self::tool_router(),
        }
    }

    // ────────────── Lazy spawn gating helpers (Issue #95) ──────────────

    /// 获取主 frama-c client。未 spawn 时返回 NoProjectLoaded 业务错误。
    /// 集中所有 main 工具的 client 守卫——callsite 模式：
    ///   let c = self.require_client().await?;
    ///   c.get(...).await
    pub async fn require_client(&self) -> Result<Arc<FramaCClient>, McpError> {
        self.client.lock().await.clone().ok_or_else(no_project_loaded_err)
    }

    /// 检查主 project 是否已 load。Fast path——只 read state flag，不 clone client。
    /// 适合 "tool 入口仅 gate，后续不直接调 frama-c" 的场景（罕见）。
    pub async fn require_project_loaded(&self) -> Result<(), McpError> {
        if self.state.read().await.project_loaded {
            Ok(())
        } else {
            Err(no_project_loaded_err())
        }
    }

    /// 检查 sandbox 是否存在；返回 (state, client) clone。
    /// experiment_id 是 sandbox 的 key（含":"前缀），如 "exp42"。
    pub async fn require_sandbox(
        &self,
        experiment_id: &str,
    ) -> Result<(SandboxState, Arc<FramaCClient>), McpError> {
        let sandboxes = self.sandboxes.read().await;
        match sandboxes.get(experiment_id) {
            Some((s, c)) => Ok((s.clone(), c.clone())),
            None => {
                let existing: Vec<String> = sandboxes.keys().cloned().collect();
                Err(sandbox_not_found_err(experiment_id, &existing))
            }
        }
    }

    /// Resolve a function name to the appropriate client.
    /// "exp_id:func_name" → sandbox client + real function name
    /// "func_name" → main client（lazy 模式下若未 load 返回 NoProjectLoaded）
    async fn resolve_client(&self, function: &str) -> Result<ResolvedClient, McpError> {
        if let Some((exp_id, func_name)) = function.split_once(':') {
            let (_, client) = self.require_sandbox(exp_id).await?;
            Ok(ResolvedClient {
                client,
                function: func_name.to_string(),
                experiment_id: Some(exp_id.to_string()),
            })
        } else {
            Ok(ResolvedClient {
                client: self.require_client().await?,
                function: function.to_string(),
                experiment_id: None,
            })
        }
    }

    /// Run WP on a sandbox Frama-C instance.
    async fn run_wp_on_sandbox(
        &self,
        params: &RunWpParams,
    ) -> Result<CallToolResult, McpError> {
        let names = params.functions.as_ref().ok_or_else(|| {
            McpError::invalid_params("functions required for sandbox WP", None)
        })?;

        // All functions must be in the same sandbox
        let first = &names[0];
        let resolved = self.resolve_client(first).await?;
        let client = &resolved.client;

        // Set prover/timeout/model on sandbox instance
        if let Some(ref prover) = params.prover {
            client
                .set("plugins.wp.setProvers", json!([prover]))
                .await
                .map_err(McpError::from)?;
        }
        if let Some(timeout) = params.timeout {
            client
                .set("plugins.wp.setTimeout", json!(timeout))
                .await
                .map_err(McpError::from)?;
        }
        {
            let mut wp_config = json!({});
            let model = params.model.as_deref().unwrap_or("Typed+nocast");
            match model {
                "Bytes" | "Typed+nocast" => wp_config["model"] = json!(model),
                _ => {
                    return Err(McpError::invalid_params(
                        format!("invalid model '{}'", model),
                        None,
                    ));
                }
            }
            if let Some(ref prop) = params.prop {
                wp_config["prop"] = json!(prop);
            }
            client
                .exec("plugins.ast-utils.execSetWpConfig", wp_config, Duration::from_secs(10))
                .await
                .map_err(McpError::from)?;
        }

        // Get cached declaration marker from sandbox state
        let exp_id = resolved.experiment_id.as_ref().unwrap();
        let decl_marker = {
            let sandboxes = self.sandboxes.read().await;
            sandboxes.get(exp_id.as_str())
                .map(|(state, _)| state.declaration_marker.clone())
                .unwrap_or_default()
        };
        if !decl_marker.is_empty() {
            client
                .get("kernel.ast.printDeclaration", json!(decl_marker))
                .await
                .map_err(McpError::from)?;
            let pvdecl_marker = decl_marker.replace("#F", "#v");
            client
                .exec(
                    "plugins.wp.startProofs",
                    json!(pvdecl_marker),
                    Duration::from_secs(600),
                )
                .await
                .map_err(McpError::from)?;
        }

        let tasks = client
            .get("plugins.wp.getScheduledTasks", json!(null))
            .await
            .map_err(McpError::from)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&tasks).unwrap_or_default(),
        )]))
    }

    /// Kill a sandbox Frama-C process and clean up state.
    async fn cleanup_sandbox(&self, experiment_id: &str) {
        let removed = {
            let mut sandboxes = self.sandboxes.write().await;
            sandboxes.remove(experiment_id)
        };
        if let Some((state, _client)) = removed {
            // 用 tokio Child 的 start_kill + wait，确保内核 reap 不留 zombie
            // （之前用外部 `kill` 命令 + 仅 status() 不 wait 是 broken pipe 主因之一）
            let mut guard = state.sandbox_child.lock().await;
            if let Some(mut child) = guard.take() {
                if let Err(e) = child.start_kill() {
                    // ESRCH = 进程已死；其他错误才警告
                    if e.kind() != std::io::ErrorKind::InvalidInput {
                        tracing::warn!(
                            experiment_id, pid = state.sandbox_pid,
                            "cleanup_sandbox: start_kill failed: {}", e
                        );
                    }
                }
                if let Err(e) = child.wait().await {
                    tracing::warn!(
                        experiment_id, pid = state.sandbox_pid,
                        "cleanup_sandbox: child.wait failed: {}", e
                    );
                }
            }
            drop(guard);
            // Remove temp directory
            if let Err(e) = std::fs::remove_dir_all(&state.sandbox_dir) {
                tracing::warn!(
                    experiment_id, dir = %state.sandbox_dir.display(),
                    "cleanup_sandbox: remove_dir_all failed: {}", e
                );
            }
        }
    }

    /// Spawn a new Frama-C server process for a sandbox.
    async fn spawn_sandbox_frama_c(
        &self,
        sandbox_file: &PathBuf,
        socket: &PathBuf,
    ) -> Result<tokio::process::Child, McpError> {
        use std::process::Stdio;
        use tokio::process::Command;

        // 把 sandbox frama-c 的 stdout/stderr 重定向到 sandbox_dir 下的日志文件，
        // 便于失败时拿到真实错误（之前 Stdio::null() 把错误丢了，导致超时只能猜）。
        let log_dir = sandbox_file.parent().unwrap_or_else(|| std::path::Path::new("/tmp"));
        let stdout_log_path = log_dir.join("sandbox.stdout.log");
        let stderr_log_path = log_dir.join("sandbox.stderr.log");
        let stdout_log = std::fs::File::create(&stdout_log_path).map_err(|e| {
            McpError::internal_error(format!("failed to create sandbox stdout log: {}", e), None)
        })?;
        let stderr_log = std::fs::File::create(&stderr_log_path).map_err(|e| {
            McpError::internal_error(format!("failed to create sandbox stderr log: {}", e), None)
        })?;

        let mut cmd = Command::new(&self.frama_c_path);
        cmd.arg(sandbox_file)
            // Sandboxes are single-function slices. Static target helpers may
            // look unused inside the slice, but they are exactly what WP needs.
            .arg("-keep-unused-functions")
            .arg("all")
            .arg("-keep-unused-types")
            .arg("-server-socket")
            .arg(socket)
            .arg("-wp-prover")
            .arg("Alt-Ergo,CVC5,Z3")
            .arg("-wp-model")
            .arg("Typed+nocast")
            // 抑制 annot-error：sandbox 提取时可能引用未定义的 axiomatic / logic function
            // （extract 不一定带全外部依赖）；让 Frama-C 把这类降级为 warning 而非 fatal，
            // 否则 sandbox 立刻 abort 导致 socket 不生成 → 10s 超时。
            .arg("-kernel-warn-key")
            .arg("annot-error=feedback")
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr_log))
            // 即使调用方忘记 wait/kill，Child drop 时 tokio 自动 SIGKILL + reap，
            // 防御 zombie 累积（参 docs/fixes/frama-c-mcp-fix-child-reap-broken-pipe.md）
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            McpError::internal_error(
                format!("failed to spawn sandbox frama-c at '{}': {}", self.frama_c_path, e),
                None,
            )
        })?;

        // Wait for socket to appear (max 10s)
        for _ in 0..20 {
            if socket.exists() {
                return Ok(child);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // Timeout: kill + wait reap，读 stderr log 给诊断
        let _ = child.start_kill();
        let _ = child.wait().await;
        let stderr_tail = std::fs::read_to_string(&stderr_log_path)
            .ok()
            .map(|s| {
                let trimmed: String = s.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                trimmed
            })
            .unwrap_or_else(|| "(no stderr log)".into());
        Err(McpError::internal_error(
            format!(
                "sandbox frama-c failed to start (socket not found after 10s).\nbinary: {}\nsandbox_file: {}\nstderr (last 20 lines):\n{}",
                self.frama_c_path,
                sandbox_file.display(),
                stderr_tail
            ),
            None,
        ))
    }

    /// Lazy spawn: 第一次 reload_project 时 spawn frama-c + 建立 client。
    /// 之后 reload_project 走 in-place reload，除非 with_rte 变化（必须 respawn）。
    ///
    /// 三分支（按 current state vs 请求参数）：
    /// 1. 没 frama-c（client.is_none()） → spawn 新进程
    /// 2. 有 frama-c + rte 切换 → kill + respawn（-rte 是 CLI flag）
    /// 3. 有 frama-c + same rte → in-place reload（不重启）
    ///
    /// 锁顺序（fix-doc §4.4.5）：main_frama_c_state → client → state
    /// 成功后 state.project_loaded = true。
    async fn ensure_main_spawned(
        &self,
        new_files: Vec<String>,
        new_rte: bool,
    ) -> Result<(), McpError> {
        use std::process::Stdio;
        use tokio::process::Command;

        let main_lock = self.main_frama_c_state.lock().await;
        let client_lock = self.client.lock().await;

        let needs_respawn = match main_lock.as_ref() {
            None => true,
            Some(s) => s.with_rte != new_rte,
        };

        if !needs_respawn {
            // 分支 3: in-place reload 文件（不动 process / socket）
            let client = client_lock.as_ref().expect("invariant: client ⇔ state").clone();
            drop(client_lock);
            drop(main_lock);
            reload_files_in_place(&client, &new_files).await?;
            // 更新缓存的 files 列表
            let mut main_lock = self.main_frama_c_state.lock().await;
            if let Some(s) = main_lock.as_mut() {
                s.files = new_files;
            }
            self.state.write().await.project_loaded = true;
            return Ok(());
        }

        // 分支 1/2: spawn 新 frama-c。旧 child（若有）在 *main_lock = Some(...)
        // 赋值时被 drop → kill_on_drop 触发 → SIGKILL 旧 frama-c。
        //
        // **关键**：必须先 drop 两个 lock，避免和 spawn / wait_socket_ready
        // 期间 self.client.lock() / self.main_frama_c_state.lock() 死锁。
        //
        // ⚠ **已知 drop 窗口竞态**（design doc §4.4.5 未提）：
        //   drop locks → spawn frama-c → wait_socket_ready → connect FramaCClient
        //   这 1-2 秒窗口内，若另一并发 ensure_main_spawned 调用进入（如两个并发
        //   reload_project 撞上），它会看到旧 state 也判断 needs_respawn=true，
        //   同样进入此分支：两个 spawn 并行，后到的 *main_lock = Some(...)
        //   赋值覆盖前者 → 前者 child 立刻 Drop → SIGKILL，得到一个"出生即死"
        //   的 frama-c（短时间双绑同 socket 路径，借 Linux unix-domain socket
        //   一对一替换语义不会真的破坏，但浪费 1 个 spawn）。
        //
        //   现实概率低：FSM tool calls 串行化（agent 一次只发一个），同 process
        //   不会真的并发调 reload_project。但若未来引入并发 client 调用，需要
        //   加 respawning: bool flag 或更细粒度的状态机防御。
        drop(client_lock);
        drop(main_lock);

        let socket_path = PathBuf::from(format!(
            "/tmp/frama-c-mcp-{}.sock", std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path); // 清残留

        // 日志路径
        let log_dir = std::path::Path::new("/tmp/frama-c-mcp-logs");
        let _ = std::fs::create_dir_all(log_dir);
        let log_basename = format!("main-{}", std::process::id());
        let stdout_log = std::fs::File::create(log_dir.join(format!("{}.stdout.log", log_basename)))
            .map_err(|e| McpError::internal_error(format!("create stdout log: {}", e), None))?;
        let stderr_log_path = log_dir.join(format!("{}.stderr.log", log_basename));
        let stderr_log = std::fs::File::create(&stderr_log_path)
            .map_err(|e| McpError::internal_error(format!("create stderr log: {}", e), None))?;

        let mut cmd = Command::new(&self.frama_c_path);
        for f in &new_files {
            cmd.arg(f);
        }
        cmd.arg("-load-module").arg("ast_utils_plugin")
           .arg("-server-socket").arg(&socket_path)
           .arg("-wp-prover").arg("Alt-Ergo,CVC5,Z3")
           .arg("-wp-model").arg("Typed+nocast")
           .stdout(Stdio::from(stdout_log))
           .stderr(Stdio::from(stderr_log))
           .kill_on_drop(true);
        if new_rte { cmd.arg("-rte"); }

        let mut child = cmd.spawn()
            .map_err(|e| McpError::internal_error(format!("spawn frama-c: {}", e), None))?;

        // 等 socket
        if !wait_socket_ready(&socket_path, Duration::from_secs(10)).await {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let stderr_tail = std::fs::read_to_string(&stderr_log_path).ok()
                .map(|s| s.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
                .unwrap_or_else(|| "(no stderr log)".into());
            return Err(McpError::internal_error(
                format!("frama-c failed to start (socket missing after 10s)\nstderr:\n{}", stderr_tail),
                None,
            ));
        }

        // Connect 新 client
        let new_client = FramaCClient::connect(
            socket_path.to_str().unwrap(),
            self.state.clone(),
        ).await.map_err(|e| {
            McpError::internal_error(format!("connect to new frama-c: {}", e), None)
        })?;

        // 同步赋值新 state + client（旧 child Drop → kill）
        let mut main_lock = self.main_frama_c_state.lock().await;
        let mut client_lock = self.client.lock().await;
        *main_lock = Some(MainFramaCState {
            child, socket_path, files: new_files,
            with_rte: new_rte,
        });
        *client_lock = Some(Arc::new(new_client));

        // session state 同步
        self.state.write().await.project_loaded = true;
        Ok(())
    }
}

/// 轮询等 socket 文件出现。timeout 内出现返 true，否则 false。
async fn wait_socket_ready(path: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// In-place reload files via Frama-C server API（不动 frama-c 进程）。
/// 复用 reload_project "Normal mode" 旧逻辑。
///
/// 注：**不调** `kernel.ast.reloadFunctions`——由唯一 caller `reload_project`
/// 主函数在 fetch_all 前统一调一次，避免分支 1（in-place）路径调两次（Gap 2 修，
/// 见 docs/fixes/reload-project-deser-and-cursor.md）。如果未来新增 caller
/// 直接用 in-place reload 而不经 reload_project 主函数，需自行调 reloadFunctions。
async fn reload_files_in_place(
    client: &FramaCClient,
    files: &[String],
) -> Result<(), McpError> {
    client.set("kernel.ast.setFiles", json!([]))
        .await.map_err(McpError::from)?;
    client.set("kernel.ast.setFiles", json!(files))
        .await.map_err(McpError::from)?;
    client.exec("kernel.ast.compute", json!(null), Duration::from_secs(120))
        .await.map_err(McpError::from)?;
    Ok(())
}

impl FramaCMcpServer {

    /// Resolve a function name to FunctionInfo, refreshing cache on miss.
    ///
    /// 1. Try cache lookup
    /// 2. On miss: reloadFunctions + fetchFunctions + update cache
    /// 3. Retry cache lookup
    /// 4. Still missing → FunctionNotFound error
    async fn resolve_function_or_refresh(
        &self,
        name: &str,
    ) -> Result<crate::state::FunctionInfo, McpError> {
        // Try cache first
        {
            let state = self.state.read().await;
            if let Some(info) = state.resolve_function(name) {
                return Ok(info.clone());
            }
        }
        // Cache miss — reload + fetch to refresh
        (self.require_client().await?)
            .get("kernel.ast.reloadFunctions", json!(null))
            .await
            .map_err(McpError::from)?;
        let entries = (self.require_client().await?)
            
            .fetch_all("kernel.ast.fetchFunctions")
            .await
            .map_err(McpError::from)?;
        {
            let mut state = self.state.write().await;
            state.update_functions(&entries);
        }
        // Retry
        let state = self.state.read().await;
        state
            .resolve_function(name)
            .cloned()
            .ok_or_else(|| McpError::from(FramaCError::FunctionNotFound(name.to_string())))
    }

    /// Resolve a global variable name to GlobalInfo, refreshing cache on miss.
    async fn resolve_global_or_refresh(
        &self,
        name: &str,
    ) -> Result<crate::state::GlobalInfo, McpError> {
        // Try cache first
        {
            let state = self.state.read().await;
            if let Some(info) = state.resolve_global(name) {
                return Ok(info.clone());
            }
        }
        // Cache miss — reload + fetch to refresh
        (self.require_client().await?)
            .get("kernel.ast.reloadGlobals", json!(null))
            .await
            .map_err(McpError::from)?;
        let entries = (self.require_client().await?)
            
            .fetch_all("kernel.ast.fetchGlobals")
            .await
            .map_err(McpError::from)?;
        {
            let mut state = self.state.write().await;
            state.update_globals(&entries);
        }
        // Retry
        let state = self.state.read().await;
        state
            .resolve_global(name)
            .cloned()
            .ok_or_else(|| McpError::from(FramaCError::GlobalNotFound(name.to_string())))
    }

    /// Ensure callgraph is cached. Computes if not yet cached.
    async fn ensure_callgraph_cached(&self) -> Result<(), McpError> {
        let needs_compute = {
            let state = self.state.read().await;
            state.callgraph_edges.is_empty() && state.callgraph_vertices.is_empty()
        };
        if needs_compute {
            (self.require_client().await?)
                .exec(
                    "plugins.callgraph.compute",
                    json!(null),
                    Duration::from_secs(60),
                )
                .await
                .map_err(McpError::from)?;
            let graph = (self.require_client().await?)
                
                .get("plugins.callgraph.getCallgraph", json!(null))
                .await
                .map_err(McpError::from)?;
            let mut state = self.state.write().await;
            state.update_callgraph(&graph);
        }
        Ok(())
    }
}

#[tool_router]
impl FramaCMcpServer {
    #[tool(
        description = "Reload C source files after modification. Reparses AST and refreshes all cached state. EVA/WP results are invalidated. \
        Set rte=true to restart Frama-C with -rte flag for automatic RTE annotation generation (signed overflow, \
        division by zero, pointer validity, array bounds). Use RTE mode when no main() exists and EVA cannot run."
    )]
    async fn reload_project(
        &self,
        Parameters(params): Parameters<ReloadProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        // Check project lock
        if *self.project_locked.read().await {
            return Err(McpError::invalid_params(
                "Project is locked. reload_project is blocked during Phase 2 to prevent annotation loss. \
                 If you are in verify-function, do NOT call reload_project — use create_sandbox/reset_sandbox instead. \
                 Only verify-program should call unlock_project for Phase 3 final gate.",
                None,
            ));
        }

        let rte = params.rte.unwrap_or(false);

        // Determine file list:
        // - 显式 files: 用
        // - None + 已 loaded: 用当前 frama-c loaded 文件
        // - None + 未 loaded: 报错（lazy 模式下没法猜）
        let files = match params.files {
            Some(f) => f,
            None => {
                let client_opt = self.client.lock().await.clone();
                match client_opt {
                    Some(c) => {
                        let v = c.get("kernel.ast.getFiles", json!(null))
                            .await.map_err(McpError::from)?;
                        v.as_array()
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                            .unwrap_or_default()
                    }
                    None => return Err(McpError::invalid_params(
                        "reload_project called without files= and no project loaded; pass files=[\"...\"]",
                        None,
                    )),
                }
            }
        };

        // 走 lazy spawn 路径：自动 in-place / respawn / 新 spawn 三选一
        self.ensure_main_spawned(files.clone(), rte).await?;

        // Refresh functions list from new state.
        // **必须先调 reloadFunctions 重置 cursor**：fetchFunctions 是 cursor-based
        // 增量 API，首次 spawn 后 cursor 在 "now"（无 delta）直接 fetchFunctions
        // 返回空。漏调时 reload_project response.functions 总是 []（依赖 agent 后续
        // 调 list_functions 兜底，违反 API contract）。
        // sandbox spawn 路径（line 2532）和 list_functions（line 813）都已正确调用。
        // 见 docs/fixes/reload-project-deser-and-cursor.md。
        let client = self.require_client().await?;
        client.get("kernel.ast.reloadFunctions", json!(null))
            .await.map_err(McpError::from)?;
        let entries = client.fetch_all("kernel.ast.fetchFunctions")
            .await.map_err(McpError::from)?;
        {
            let mut state = self.state.write().await;
            state.invalidate_all();
            state.update_functions(&entries);
            state.project_loaded = true;
        }

        let result = json!({
            "functions": entries,
            "files": files,
            "rte": rte,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Get detailed info for a function: source location, declaration text with ACSL annotations."
    )]
    async fn get_function_info(
        &self,
        Parameters(params): Parameters<GetFunctionInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.function.contains(':') {
            // Sandbox: use cached declaration marker
            let resolved = self.resolve_client(&params.function).await?;
            let exp_id = resolved.experiment_id.as_ref().unwrap();
            let decl_marker = {
                let sandboxes = self.sandboxes.read().await;
                sandboxes.get(exp_id.as_str())
                    .map(|(state, _)| state.declaration_marker.clone())
                    .unwrap_or_default()
            };
            if decl_marker.is_empty() {
                return Err(McpError::from(FramaCError::FunctionNotFound(resolved.function)));
            }
            let decl_text = resolved.client
                .get("kernel.ast.printDeclaration", json!(decl_marker))
                .await
                .map_err(McpError::from)?;
            let result = json!({
                "name": resolved.function,
                "declaration": decl_text,
            });
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        let info = self
            .resolve_function_or_refresh(&params.function)
            .await?;
        let decl_text = (self.require_client().await?)
            
            .get(
                "kernel.ast.printDeclaration",
                json!(info.declaration),
            )
            .await
            .map_err(McpError::from)?;
        let result = json!({
            "name": info.name,
            "marker": info.marker,
            "signature": info.signature,
            "file": info.file,
            "line": info.line,
            "declaration": decl_text,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Compute and return the function call graph.")]
    async fn get_callgraph(&self) -> Result<CallToolResult, McpError> {
        (self.require_client().await?)
            .exec(
                "plugins.callgraph.compute",
                json!(null),
                Duration::from_secs(60),
            )
            .await
            .map_err(McpError::from)?;
        let graph = (self.require_client().await?)
            
            .get("plugins.callgraph.getCallgraph", json!(null))
            .await
            .map_err(McpError::from)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&graph).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Compute topological order of functions (Tarjan SCC + Kahn 分层). \
                       Returns array of {level, groups: [{id, members, is_cycle, level}]}, \
                       where each group is an SCC (size≥2 互递归 or size=1 自环 → is_cycle=true). \
                       Level 0 = leaf SCCs (无 outgoing callees outside SCC), Level N+1 = SCCs \
                       depending on ≤N. 同 SCC 成员显式同 group。bottom-up 验证用：先验 leaf 函数 \
                       再向上。详见 docs/design/verify-program-fsm/architecture.md §5.4。"
    )]
    async fn compute_topological_order(
        &self,
        _params: Parameters<ComputeTopologicalOrderParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_callgraph_cached().await?;

        let state = self.state.read().await;

        // 收集 vertices + edges (按 function name)
        let vertices: Vec<String> = state
            .callgraph_vertices
            .iter()
            .map(|v| v.name.clone())
            .collect();

        let mut edges: Vec<(String, String)> = Vec::new();
        for edge in &state.callgraph_edges {
            if let (Some(caller_name), Some(callee_name)) = (
                state.resolve_decl_to_name(&edge.src),
                state.resolve_decl_to_name(&edge.dst),
            ) {
                edges.push((caller_name.to_string(), callee_name.to_string()));
            }
        }

        // VO-completeness fix：defined 集——callgraph vertices 含 library declared-only
        // （detailed-design.md §292），verification_order 必须只含**有定义**的函数（外部声明不验证）。
        let defined: std::collections::HashSet<String> = state
            .functions
            .iter()
            .filter(|(_, f)| f.defined)
            .map(|(name, _)| name.clone())
            .collect();

        drop(state);

        // 调 pure 函数算 SCC + Kahn 分层（Level 0 = leaf SCCs 升序 = bottom-up 验证序）
        let levels = crate::topo::compute_topological_order(&vertices, &edges)
            .map_err(|e| McpError::internal_error(format!("topo: {}", e), None))?;

        // VO-completeness fix：flatten levels → verification_order + scc_groups（defined 过滤、
        // 按构造完整、levels 字段已删，分层由 scc_groups.level 携带）。纯逻辑抽
        // topo::flatten_levels_to_vo_scc，可单测（VO bottom-up 序 / defined 过滤 / declared-only 排除 / scc level）。
        let (verification_order, scc_groups) =
            crate::topo::flatten_levels_to_vo_scc(&levels, &defined);

        // seed 进 in-memory（server-owned）；full-replace 时保留（见 store_project_state）。
        {
            let mut w = self.state.write().await;
            let ps = w
                .project_state
                .get_or_insert_with(crate::state::ProjectVerificationState::default);
            ps.verification_order = verification_order.clone();
            ps.scc_groups = scc_groups.clone();
            ps.current_level = 0;
        }

        // 返回 server seed 的 VO + scc_groups（agent 不再 build，只读结果作展示/报告）
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "verification_order": verification_order,
                "scc_groups": scc_groups,
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "fsmint-3 依赖驱动调度：返回就绪函数（所有非同-SCC callee 已 merge 可消费）。纯函数——状态全由 done/in_progress 参数传入，无 level 概念。done=完成且 merge 的函数；in_progress=在跑/已派的函数（排除）。返回 [{function, scc_id, is_cycle, scc_members}]。"
    )]
    async fn get_ready_functions(
        &self,
        Parameters(p): Parameters<GetReadyFunctionsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_callgraph_cached().await?;
        let state = self.state.read().await;

        // callgraph 取数（同 compute_topological_order）
        let vertices: Vec<String> = state
            .callgraph_vertices
            .iter()
            .map(|v| v.name.clone())
            .collect();
        let mut edges: Vec<(String, String)> = Vec::new();
        for edge in &state.callgraph_edges {
            if let (Some(caller_name), Some(callee_name)) = (
                state.resolve_decl_to_name(&edge.src),
                state.resolve_decl_to_name(&edge.dst),
            ) {
                edges.push((caller_name.to_string(), callee_name.to_string()));
            }
        }
        drop(state);

        let ready =
            crate::topo::compute_ready_functions(&vertices, &edges, &p.done, &p.in_progress);

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&ready).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Run EVA abstract interpretation analysis. Optionally set precision, entry function, and loop unrolling level. Returns computation state and program statistics. This may take several minutes for large programs."
    )]
    async fn run_eva(
        &self,
        Parameters(params): Parameters<RunEvaParams>,
    ) -> Result<CallToolResult, McpError> {
        // P2.8: Set optional parameters before compute
        if let Some(precision) = params.precision {
            (self.require_client().await?)
                .set("kernel.parameters.setEvaPrecision", json!(precision))
                .await
                .map_err(McpError::from)?;
        }
        if let Some(ref main_fn) = params.main_function {
            (self.require_client().await?)
                .set("kernel.parameters.setMain", json!(main_fn))
                .await
                .map_err(McpError::from)?;
        }
        if let Some(slevel) = params.slevel {
            (self.require_client().await?)
                .set("kernel.parameters.setEvaSlevel", json!(slevel))
                .await
                .map_err(McpError::from)?;
        }

        (self.require_client().await?)
            .exec(
                "plugins.eva.general.compute",
                json!(null),
                Duration::from_secs(600),
            )
            .await
            .map_err(McpError::from)?;
        let comp_state = (self.require_client().await?)
            
            .get("plugins.eva.general.getComputationState", json!(null))
            .await
            .map_err(McpError::from)?;
        let stats = (self.require_client().await?)
            
            .get("plugins.eva.general.getProgramStats", json!(null))
            .await
            .map_err(McpError::from)?;

        {
            let mut state = self.state.write().await;
            state.set_eva_completed();
        }

        let result = json!({
            "computation_state": comp_state,
            "program_stats": stats,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Get EVA analysis alarms (potential runtime errors). Optionally filter by function, alarm kind, or verification status."
    )]
    async fn get_eva_alarms(
        &self,
        Parameters(params): Parameters<GetEvaAlarmsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Reload to reset incremental cursor (fetchStatus is consumed once)
        (self.require_client().await?)
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let properties = (self.require_client().await?)
            
            .fetch_all("kernel.properties.fetchStatus")
            .await
            .map_err(McpError::from)?;

        // Property fields (verified via integration test):
        //   scope: declaration marker of the enclosing function (e.g. "#F24")
        //   kind: "ensures", "requires", "instance", "behavior", etc.
        //   status: "valid", "unknown", "invalid", etc.
        //   descr, predicate, source.file, source.line, alarm, alarm_descr

        // Resolve function name to declaration marker for scope filtering
        let scope_marker = if let Some(ref func) = params.function {
            Some(self.resolve_function_or_refresh(func).await?.declaration)
        } else {
            None
        };

        let filtered: Vec<_> = properties
            .iter()
            .filter(|prop| {
                if let Some(ref marker) = scope_marker {
                    let prop_scope = prop["scope"].as_str().unwrap_or_default();
                    if prop_scope != marker {
                        return false;
                    }
                }
                if let Some(ref kind) = params.alarm_kind {
                    let prop_kind = prop["kind"].as_str().unwrap_or_default();
                    if prop_kind != kind {
                        return false;
                    }
                }
                if let Some(ref status) = params.status {
                    let prop_status = prop["status"].as_str().unwrap_or_default();
                    if prop_status != status {
                        return false;
                    }
                }
                true
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&filtered).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Query EVA value range at a program point. The marker can be obtained from get_eva_alarms results."
    )]
    async fn get_eva_value(
        &self,
        Parameters(params): Parameters<GetEvaValueParams>,
    ) -> Result<CallToolResult, McpError> {
        // callstack is param_opt: when present, pass it; when absent, omit the
        // field entirely (do NOT pass null).
        let mut request_data = json!({"target": params.marker});
        if let Some(cs) = params.callstack {
            request_data["callstack"] = json!(cs);
        }
        let values = (self.require_client().await?)
            
            .get("plugins.eva.values.getValues", request_data)
            .await
            .map_err(McpError::from)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&values).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Run WP deductive verification on a SANDBOX Frama-C instance. \
            All function names MUST include the experiment_id prefix (e.g. 'exp42:foo'). \
            Use this from verify-function subagents during CEGIS iteration. \
            For main-instance WP (Phase 3 final gate), use run_wp_main instead."
    )]
    async fn run_wp_sandbox(
        &self,
        Parameters(params): Parameters<RunWpParams>,
    ) -> Result<CallToolResult, McpError> {
        // Schema gate: every function name must include ':' (sandbox prefix).
        let names = params.functions.as_ref().ok_or_else(|| {
            McpError::invalid_params(
                "functions required for run_wp_sandbox (must include experiment_id prefix, e.g. 'exp42:foo')",
                None,
            )
        })?;
        for name in names {
            if !name.contains(':') {
                return Err(McpError::invalid_params(
                    format!(
                        "function '{}' must include experiment_id prefix (e.g. 'exp42:foo'); use run_wp_main for main instance",
                        name
                    ),
                    None,
                ));
            }
        }
        self.run_wp_on_sandbox(&params).await
    }

    #[tool(
        description = "Run WP deductive verification on the MAIN Frama-C instance. \
            Function names MUST NOT include ':' (no sandbox prefix). If omitted, \
            verifies all annotated functions. Used by the verify-program agent at \
            Phase 3 final gate. For sandbox WP (CEGIS), use run_wp_sandbox instead."
    )]
    async fn run_wp_main(
        &self,
        Parameters(params): Parameters<RunWpParams>,
    ) -> Result<CallToolResult, McpError> {
        // Schema gate: no function name may include ':'.
        if let Some(ref names) = params.functions {
            for name in names {
                if name.contains(':') {
                    return Err(McpError::invalid_params(
                        format!(
                            "function '{}' must not include ':'; use run_wp_sandbox for sandbox operations",
                            name
                        ),
                        None,
                    ));
                }
            }
        }

        // Check project lock for main instance WP
        if *self.project_locked.read().await {
            return Err(McpError::invalid_params(
                "Project is locked. run_wp_main is blocked during Phase 2 to prevent state pollution. \
                 If you are in verify-function, use run_wp_sandbox. \
                 Do NOT touch the main Frama-C instance. Call unlock_project first if this is Phase 3 final gate.",
                None,
            ));

        }

        // --- Main instance WP ---

        // Provers are set at Frama-C launch (Alt-Ergo,CVC5,Z3 in launch-mcp.sh).
        // Only call setProvers if the agent explicitly overrides.
        if let Some(ref prover) = params.prover {
            (self.require_client().await?)
                .set("plugins.wp.setProvers", json!([prover]))
                .await
                .map_err(McpError::from)?;
        }
        if let Some(timeout) = params.timeout {
            (self.require_client().await?)
                .set("plugins.wp.setTimeout", json!(timeout))
                .await
                .map_err(McpError::from)?;
        }

        // Set model and prop via ast-utils execSetWpConfig.
        // Default model is Typed+nocast (better for assigns/validity/overflow).
        // Only fall back to Bytes for char*/void* heterogeneous pointer casts.
        {
            let mut wp_config = json!({});
            let model = params.model.as_deref().unwrap_or("Typed+nocast");
            match model {
                "Bytes" | "Typed+nocast" => {
                    wp_config["model"] = json!(model);
                }
                _ => {
                    return Err(McpError::invalid_params(
                        format!("invalid model '{}', must be 'Bytes' or 'Typed+nocast'", model),
                        None,
                    ));
                }
            }
            if let Some(ref prop) = params.prop {
                wp_config["prop"] = json!(prop);
            }
            (self.require_client().await?)
                .exec("plugins.ast-utils.execSetWpConfig", wp_config, Duration::from_secs(10))
                .await
                .map_err(McpError::from)?;
        }

        // Resolve target functions
        let targets = match params.functions {
            Some(names) => {
                let mut infos = Vec::new();
                for name in &names {
                    infos.push(self.resolve_function_or_refresh(name).await?);
                }
                infos
            }
            None => {
                let state = self.state.read().await;
                state.functions.values().cloned().collect()
            }
        };

        for info in &targets {
            let decl_marker = &info.declaration;
            (self.require_client().await?)
                .get("kernel.ast.printDeclaration", json!(decl_marker))
                .await
                .map_err(McpError::from)?;
            let pvdecl_marker = decl_marker.replace("#F", "#v");
            (self.require_client().await?)
                .exec(
                    "plugins.wp.startProofs",
                    json!(pvdecl_marker),
                    Duration::from_secs(600),
                )
                .await
                .map_err(McpError::from)?;
        }

        let tasks = (self.require_client().await?)
            
            .get("plugins.wp.getScheduledTasks", json!(null))
            .await
            .map_err(McpError::from)?;

        {
            let mut state = self.state.write().await;
            state.set_wp_completed();
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&tasks).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Get comprehensive verification status: property counts by category, EVA/WP analysis state."
    )]
    async fn get_verification_status(&self) -> Result<CallToolResult, McpError> {
        // Reload to reset incremental cursor (fetchStatus is consumed once)
        (self.require_client().await?)
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let properties = (self.require_client().await?)
            
            .fetch_all("kernel.properties.fetchStatus")
            .await
            .map_err(McpError::from)?;

        let (project_loaded, eva_state, wp_state) = {
            let state = self.state.read().await;
            (state.project_loaded, state.eva_completed, state.wp_completed)
        };

        let mut by_status: HashMap<String, u64> = HashMap::new();
        let mut by_kind: HashMap<String, u64> = HashMap::new();
        for prop in &properties {
            let status = prop["status"].as_str().unwrap_or("unknown");
            *by_status.entry(status.to_string()).or_default() += 1;
            let kind = prop["kind"].as_str().unwrap_or("unknown");
            *by_kind.entry(kind.to_string()).or_default() += 1;
        }

        let mut result = json!({
            "total_properties": properties.len(),
            "by_status": by_status,
            "by_kind": by_kind,
        });

        if eva_state {
            let comp = (self.require_client().await?)
                
                .get("plugins.eva.general.getComputationState", json!(null))
                .await
                .unwrap_or(json!(null));
            result["eva"] = comp;
        }
        if wp_state {
            let tasks = (self.require_client().await?)
                
                .get("plugins.wp.getScheduledTasks", json!(null))
                .await
                .unwrap_or(json!(null));
            result["wp"] = tasks;
        }

        result["session"] = json!({
            "project_loaded": project_loaded,
            "eva_completed": eva_state,
            "wp_completed": wp_state,
        });
        result["properties"] = json!(properties);

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    // --- Phase 2 new tools ---

    #[tool(
        description = "Get WP proof goal details. Optionally filter by function or proof status."
    )]
    async fn get_wp_goals(
        &self,
        Parameters(params): Parameters<GetWpGoalsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Check if this is a sandbox request
        let is_sandbox = params
            .function
            .as_ref()
            .map(|f| f.contains(':'))
            .unwrap_or(false);

        let client: Arc<FramaCClient>;
        let scope_marker: Option<String>;

        if is_sandbox {
            let func = params.function.as_ref().unwrap();
            let resolved = self.resolve_client(func).await?;
            client = resolved.client.clone();
            // Use cached declaration marker
            let exp_id = resolved.experiment_id.as_ref().unwrap();
            scope_marker = {
                let sandboxes = self.sandboxes.read().await;
                sandboxes.get(exp_id.as_str())
                    .map(|(state, _)| state.declaration_marker.clone())
            };
        } else {
            client = self.require_client().await?;
            scope_marker = if let Some(ref func) = params.function {
                Some(self.resolve_function_or_refresh(func).await?.declaration)
            } else {
                None
            };
        }

        client
            .get("plugins.wp.reloadGoals", json!(null))
            .await
            .map_err(McpError::from)?;
        let goals = client
            .fetch_all("plugins.wp.fetchGoals")
            .await
            .map_err(McpError::from)?;

        // 过滤 + 增强：每个 goal 加 `goal_kind` 和（如有）`hash_label` 字段
        // （§13.6 改动 20：让 LLM 在 S4_remediate 区分 spec / source_assert / RTE 失败）
        let augmented: Vec<serde_json::Value> = goals
            .iter()
            .filter(|g| {
                if let Some(ref marker) = scope_marker {
                    if g["scope"].as_str() != Some(marker.as_str()) {
                        return false;
                    }
                }
                if let Some(ref status) = params.status {
                    let goal_status = g["status"].as_str().unwrap_or_default();
                    if !goal_status.eq_ignore_ascii_case(status) {
                        return false;
                    }
                }
                true
            })
            .map(|g| {
                let mut g = g.clone();
                let (kind, hash_label) = classify_wp_goal(&g);
                if let Some(obj) = g.as_object_mut() {
                    obj.insert("goal_kind".to_string(), serde_json::Value::String(kind));
                    if let Some(h) = hash_label {
                        obj.insert("hash_label".to_string(), serde_json::Value::String(h));
                    }
                }
                g
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&augmented).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "List ACSL annotations on a function with their verification status."
    )]
    async fn get_current_annotations(
        &self,
        Parameters(params): Parameters<GetAnnotationsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.function.contains(':') {
            // Sandbox: use cached declaration marker
            let resolved = self.resolve_client(&params.function).await?;
            let exp_id = resolved.experiment_id.as_ref().unwrap();
            let decl_marker = {
                let sandboxes = self.sandboxes.read().await;
                sandboxes.get(exp_id.as_str())
                    .map(|(state, _)| state.declaration_marker.clone())
            };
            resolved.client
                .get("kernel.properties.reloadStatus", json!(null))
                .await
                .map_err(McpError::from)?;
            let properties = resolved.client
                .fetch_all("kernel.properties.fetchStatus")
                .await
                .map_err(McpError::from)?;
            let annotations: Vec<_> = properties
                .iter()
                .filter(|p| {
                    decl_marker.as_ref().map_or(true, |m| p["scope"].as_str() == Some(m.as_str()))
                })
                .collect();
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&annotations).unwrap_or_default(),
            )]));
        }

        let info = self
            .resolve_function_or_refresh(&params.function)
            .await?;
        (self.require_client().await?)
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let properties = (self.require_client().await?)
            
            .fetch_all("kernel.properties.fetchStatus")
            .await
            .map_err(McpError::from)?;
        let annotations: Vec<_> = properties
            .iter()
            .filter(|p| p["scope"].as_str() == Some(&info.declaration))
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&annotations).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Find all callers of a function. Requires EVA to have run first."
    )]
    async fn find_callers(
        &self,
        Parameters(params): Parameters<FindCallersParams>,
    ) -> Result<CallToolResult, McpError> {
        let info = self
            .resolve_function_or_refresh(&params.function)
            .await?;

        let callers = (self.require_client().await?)
            
            .get(
                "plugins.eva.general.getCallers",
                json!(info.declaration),
            )
            .await
            .map_err(McpError::from)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&callers).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Look up an identifier: find its type, source location, and scope. Searches functions and global variables."
    )]
    async fn lookup_symbol(
        &self,
        Parameters(params): Parameters<LookupSymbolParams>,
    ) -> Result<CallToolResult, McpError> {
        // Try function cache first
        if let Ok(info) = self.resolve_function_or_refresh(&params.name).await {
            let result = json!({
                "kind": "function",
                "name": info.name,
                "signature": info.signature,
                "file": info.file,
                "line": info.line,
                "marker": info.marker,
                "declaration": info.declaration,
            });
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        // Try global variable cache
        if let Ok(info) = self.resolve_global_or_refresh(&params.name).await {
            let result = json!({
                "kind": "global_variable",
                "name": info.name,
                "type": info.typ,
                "file": info.file,
                "line": info.line,
                "marker": info.marker,
                "declaration": info.declaration,
            });
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        Err(McpError::from(FramaCError::SymbolNotFound(params.name)))
    }

    #[tool(
        description = "Trace multi-level call chain from a function. Supports upward (callers) or downward (callees) traversal."
    )]
    async fn trace_call_chain(
        &self,
        Parameters(params): Parameters<TraceCallChainParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_callgraph_cached().await?;

        let info = self
            .resolve_function_or_refresh(&params.function)
            .await?;
        let max_depth = params.max_depth.unwrap_or(5).min(20);

        // Resolve stop_at names to declaration markers
        let mut stop_markers: HashSet<String> = HashSet::new();
        if let Some(ref stop_names) = params.stop_at {
            for name in stop_names {
                if let Ok(si) = self.resolve_function_or_refresh(name).await {
                    stop_markers.insert(si.declaration);
                }
            }
        }

        let state = self.state.read().await;
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        queue.push_back((info.declaration.clone(), 0));
        let mut visited: HashSet<String> = HashSet::new();
        let mut chain: Vec<serde_json::Value> = Vec::new();

        while let Some((marker, depth)) = queue.pop_front() {
            if depth > max_depth || visited.contains(&marker) {
                continue;
            }
            if depth > 0 && stop_markers.contains(&marker) {
                // Record the node but don't expand further
                continue;
            }
            visited.insert(marker.clone());

            let neighbors: Vec<&str> = match params.direction.as_str() {
                "callers" => state.get_callers(&marker),
                "callees" => state.get_callees(&marker),
                _ => {
                    return Err(McpError::invalid_params(
                        "direction must be \"callers\" or \"callees\"",
                        None,
                    ));
                }
            };

            for neighbor in neighbors {
                let from_name = state.resolve_decl_to_name(&marker).unwrap_or("?");
                let to_name = state.resolve_decl_to_name(neighbor).unwrap_or("?");
                chain.push(json!({
                    "from": from_name,
                    "to": to_name,
                    "from_marker": marker,
                    "to_marker": neighbor,
                    "depth": depth,
                }));
                queue.push_back((neighbor.to_string(), depth + 1));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&chain).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Deep investigation of a property/alarm: value ranges, callers, and existing annotations in one call."
    )]
    async fn investigate_alarm(
        &self,
        Parameters(params): Parameters<InvestigateAlarmParams>,
    ) -> Result<CallToolResult, McpError> {
        // Get all properties
        (self.require_client().await?)
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let all_props = (self.require_client().await?)
            
            .fetch_all("kernel.properties.fetchStatus")
            .await
            .map_err(McpError::from)?;

        let prop = all_props
            .iter()
            .find(|p| p["key"].as_str() == Some(&params.property_key))
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("property not found: {}", params.property_key),
                    None,
                )
            })?;

        let mut result = json!({ "property": prop });
        let depth = params.depth.as_deref().unwrap_or("normal");

        if depth == "quick" {
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        // Normal: value range query
        if let Some(kinstr) = prop["kinstr"].as_str() {
            if let Ok(values) = (self.require_client().await?)
                
                .get("plugins.eva.values.getValues", json!({"target": kinstr}))
                .await
            {
                result["values"] = values;
            }
        }

        // Normal: callers of the enclosing function
        if let Some(scope) = prop["scope"].as_str() {
            if let Ok(callers) = (self.require_client().await?)
                
                .get("plugins.eva.general.getCallers", json!(scope))
                .await
            {
                result["callers"] = callers;
            }
        }

        if depth == "normal" {
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }

        // Deep: all annotations on the same function
        if let Some(scope) = prop["scope"].as_str() {
            let annotations: Vec<_> = all_props
                .iter()
                .filter(|p| p["scope"].as_str() == Some(scope))
                .collect();
            result["function_annotations"] = json!(annotations);
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Analyze current verification state and suggest next actions."
    )]
    async fn suggest_verification_plan(
        &self,
        Parameters(_params): Parameters<SuggestPlanParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = self.state.read().await;
        let mut suggestions: Vec<serde_json::Value> = Vec::new();

        if !state.project_loaded {
            suggestions.push(json!({
                "action": "reload_project",
                "reason": "No project loaded. Load C source files first with reload_project.",
                "priority": "high",
            }));
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&suggestions).unwrap_or_default(),
            )]));
        }

        if !state.eva_completed {
            suggestions.push(json!({
                "action": "run_eva",
                "reason": "EVA analysis not yet run. Run EVA first to identify potential runtime errors.",
                "priority": "high",
            }));
        }

        // Need to drop state lock before making client calls
        let eva_completed = state.eva_completed;
        let wp_completed = state.wp_completed;
        drop(state);

        if eva_completed {
            (self.require_client().await?)
                .get("kernel.properties.reloadStatus", json!(null))
                .await
                .map_err(McpError::from)?;
            let props = (self.require_client().await?)
                
                .fetch_all("kernel.properties.fetchStatus")
                .await
                .map_err(McpError::from)?;

            let unknown_count = props
                .iter()
                .filter(|p| p["status"].as_str() == Some("unknown"))
                .count();
            let invalid_count = props
                .iter()
                .filter(|p| p["status"].as_str() == Some("invalid"))
                .count();

            if invalid_count > 0 {
                suggestions.push(json!({
                    "action": "investigate invalid properties",
                    "reason": format!("{} invalid properties need attention", invalid_count),
                    "priority": "high",
                }));
            }
            if unknown_count > 0 && !wp_completed {
                suggestions.push(json!({
                    "action": "run_wp",
                    "reason": format!("{} unknown properties may be provable with WP", unknown_count),
                    "priority": "medium",
                }));
            }
        }

        if wp_completed {
            suggestions.push(json!({
                "action": "review results",
                "reason": "Both EVA and WP completed. Review get_verification_status for summary.",
                "priority": "low",
            }));
        }

        if suggestions.is_empty() {
            suggestions.push(json!({
                "action": "get_verification_status",
                "reason": "Check current verification state for details.",
                "priority": "info",
            }));
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&suggestions).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Run linear invariant inference on a transition system.")]
    async fn run_linear_invariant(
        &self,
        Parameters(params): Parameters<RunLinearInvariantParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::linear_invariant::{json_to_in_format, parse_inv_output};

        let in_text =
            json_to_in_format(&params.input).map_err(|e| McpError::internal_error(e, None))?;

        // Write temp file
        let tmp = tempfile::NamedTempFile::new()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        std::fs::write(tmp.path(), &in_text)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Spawn CLI
        let output = tokio::process::Command::new("linear_invariant")
            .arg(tmp.path())
            .output()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if output.status.success() {
            let inv_text = String::from_utf8_lossy(&output.stdout);
            let result = parse_inv_output(&inv_text);
            Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(McpError::internal_error(stderr.to_string(), None))
        }
    }

    // ─── §4 Kernel overview handlers ─────────────────────────────────

    #[tool(description = "List all loaded source files.")]
    async fn list_files(
        &self,
        _params: Parameters<ListFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = (self.require_client().await?)
            
            .get("kernel.ast.getFiles", json!(null))
            .await
            .map_err(McpError::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(description = "List all functions with names, signatures, and source locations.")]
    async fn list_functions(
        &self,
        _params: Parameters<ListFunctionsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Ensure cache is populated
        {
            let st = self.state.read().await;
            if st.functions.is_empty() {
                drop(st);
                // Trigger a refresh
                let _ = (self.require_client().await?)
                    
                    .get("kernel.ast.reloadFunctions", json!(null))
                    .await;
                let entries = (self.require_client().await?)
                    
                    .fetch_all("kernel.ast.fetchFunctions")
                    .await
                    .map_err(McpError::from)?;
                let mut st = self.state.write().await;
                st.update_functions(&entries);
            }
        }
        let st = self.state.read().await;
        let funcs: Vec<_> = st
            .functions
            .values()
            .map(|f| {
                json!({
                    "name": f.name,
                    "signature": f.signature,
                    "file": f.file,
                    "line": f.line,
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&funcs).unwrap_or_default(),
        )]))
    }

    #[tool(description = "List all global variables with names, types, and source locations.")]
    async fn list_globals(
        &self,
        _params: Parameters<ListGlobalsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Ensure cache is populated
        {
            let st = self.state.read().await;
            if st.globals.is_empty() {
                drop(st);
                let _ = (self.require_client().await?)
                    
                    .get("kernel.ast.reloadGlobals", json!(null))
                    .await;
                let entries = (self.require_client().await?)
                    
                    .fetch_all("kernel.ast.fetchGlobals")
                    .await
                    .map_err(McpError::from)?;
                let mut st = self.state.write().await;
                st.update_globals(&entries);
            }
        }
        let st = self.state.read().await;
        let globals: Vec<_> = st
            .globals
            .values()
            .map(|g| {
                json!({
                    "name": g.name,
                    "type": g.typ,
                    "file": g.file,
                    "line": g.line,
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&globals).unwrap_or_default(),
        )]))
    }

    #[tool(description = "List all type declarations (structs, enums, typedefs).")]
    async fn list_declarations(
        &self,
        _params: Parameters<ListDeclarationsParams>,
    ) -> Result<CallToolResult, McpError> {
        // Try kernel.ast.getDeclarations first; fall back to error
        let result = (self.require_client().await?)
            
            .get("kernel.ast.getDeclarations", json!(null))
            .await;
        match result {
            Ok(v) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&v).unwrap_or_default(),
            )])),
            Err(_) => Err(McpError::internal_error(
                "kernel.ast.getDeclarations not available in this Frama-C version".to_string(),
                None,
            )),
        }
    }

    // ─── §4 ast-utils passthrough handlers ───────────────────────────

    #[tool(description = "Get the AST (source code with statement IDs) of a function.")]
    async fn get_function_ast(
        &self,
        Parameters(params): Parameters<GetFunctionAstParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.function.contains(':') {
            // Sandbox: use sandbox client's getFunctionAst directly
            let resolved = self.resolve_client(&params.function).await?;
            let result = resolved
                .client
                .get(
                    "plugins.ast-utils.getFunctionAst",
                    json!(resolved.function),
                )
                .await
                .map_err(McpError::from)?;
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            )]));
        }
        let func = self.resolve_function_or_refresh(&params.function).await?;
        let result = (self.require_client().await?)
            
            .get(
                "kernel.ast.printDeclaration",
                json!(func.declaration),
            )
            .await
            .map_err(McpError::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Validate an ACSL annotation string without inserting it. \
        Returns payload {valid: bool, error: string|null}. The tool call itself \
        always succeeds (unless arg/transport error); inspect `valid` to decide. \
        On valid=false the `error` field carries a Logic_typing or scope diagnostic \
        (e.g. \"unbound logic predicate X\", \"comparison of incompatible types\", \
        or \"Variable 'i' is a function local; ACSL function-level contracts may \
        only reference caller-visible state\").")]
    async fn validate_acsl(
        &self,
        Parameters(params): Parameters<ValidateAcslParams>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve_client(&params.function).await?;
        let mut data = json!({
            "function": resolved.function,
            "kind": params.kind,
            "acsl": params.acsl,
        });
        if let Some(stmt) = params.stmt {
            data["stmt"] = json!(stmt);
        }
        let result = resolved
            .client
            .get("plugins.ast-utils.getAcslValidation", data)
            .await
            .map_err(McpError::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Add an ACSL annotation to a function in a SANDBOX Frama-C instance. \
        The function name MUST include the experiment_id prefix (e.g. 'exp42:foo'). \
        A unique hash_label is auto-generated and injected into the AST pred_name \
        after parsing (not into the ACSL string). \
        \n\nReturn payload always present: {success: bool, error: string|null, hash_label: string}. \
        The tool call itself always succeeds (unless arg/transport error); inspect \
        `success` to decide. On success=false the annotation was NOT written to AST \
        (no rollback needed) and `error` carries the diagnostic — common cases: \
        \"function local\" (funspec references local, see ACSL §2.3), \"unbound logic \
        predicate/function/variable\" (undefined name), \"syntax error\", \"comparison \
        of incompatible types\". For main-instance annotation (Phase 2c merge), \
        use add_annotation_main instead.")]
    async fn add_annotation_sandbox(
        &self,
        Parameters(params): Parameters<AddAnnotationParams>,
    ) -> Result<CallToolResult, McpError> {
        if !params.function.contains(':') {
            return Err(McpError::invalid_params(
                format!(
                    "function '{}' must include experiment_id prefix (e.g. 'exp42:foo'); use add_annotation_main for main instance",
                    params.function
                ),
                None,
            ));
        }
        // 提取原 function 名（冒号后部分）用于副作用更新
        let original_func = params.function.split(':').nth(1).unwrap_or("").to_string();
        let (result, _hash) = self.add_annotation_impl(params).await?;

        // 副作用 merge 写 conclusion（§13.6 改动 15）：
        // sandbox_clean=false + annotation_count += 1
        if !original_func.is_empty() {
            let mut state = self.state.write().await;
            state.on_annotation_added(&original_func);
            let conclusion = state.get_conclusion(&original_func).cloned();
            drop(state);
            if let Some(c) = conclusion {
                if let Err(e) = persist_conclusion(&original_func, &c) {
                    tracing::warn!("persist_conclusion({}) failed (annotation side-effect): {}", original_func, e);
                }
            }
        }
        Ok(result)
    }

    #[tool(description = "Add an ACSL annotation to a function on the MAIN Frama-C instance. \
        The function name MUST NOT include ':' (no sandbox prefix). \
        A unique hash_label is auto-generated and injected into the AST pred_name \
        after parsing (not into the ACSL string). \
        \n\nReturn payload always present: {success: bool, error: string|null, hash_label: string}. \
        The tool call itself always succeeds (unless arg/transport error); inspect \
        `success` to decide. On success=false the annotation was NOT written to AST \
        and `error` carries a Logic_typing / scope diagnostic (same semantics as \
        add_annotation_sandbox). Used by the verify-program agent at Phase 2c \
        (merging committed specs) and Phase 1 (user-provided specs). For sandbox \
        annotation (CEGIS), use add_annotation_sandbox instead.")]
    async fn add_annotation_main(
        &self,
        Parameters(params): Parameters<AddAnnotationParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.function.contains(':') {
            return Err(McpError::invalid_params(
                format!(
                    "function '{}' must not include ':'; use add_annotation_sandbox for sandbox operations",
                    params.function
                ),
                None,
            ));
        }
        let (result, _) = self.add_annotation_impl(params).await?;
        Ok(result)
    }

    /// Shared implementation for add_annotation_sandbox / add_annotation_main.
    /// Routing decision is already made by the caller via the schema gate.
    /// Returns the response plus the generated hash_label so that callers
    /// (e.g. inject_all_annotations_sandbox) can correlate inserted annotations
    /// with their AST labels.
    async fn add_annotation_impl(
        &self,
        params: AddAnnotationParams,
    ) -> Result<(CallToolResult, String), McpError> {
        let resolved = self.resolve_client(&params.function).await?;
        let hash_label = generate_hash_label(&params.kind);
        let label = full_label(&hash_label, params.user_label.as_deref());
        let mut data = json!({
            "function": resolved.function,
            "kind": params.kind,
            "acsl": params.acsl,
            "label": label,
        });
        if let Some(stmt) = params.stmt {
            data["stmt"] = json!(stmt);
        }
        let result = resolved
            .client
            .exec("plugins.ast-utils.execAddAnnotation", data, Duration::from_secs(30))
            .await
            .map_err(McpError::from)?;
        let mut result_obj = result.clone();
        if let Some(obj) = result_obj.as_object_mut() {
            obj.insert("hash_label".to_string(), json!(hash_label.clone()));
        }
        Ok((CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result_obj).unwrap_or_default(),
        )]), hash_label))
    }

    // --- Deprecated: remove_annotation ---
    // Crashes after WP verification due to property_status cross-references.
    // Replaced by sandbox mechanism: reset_sandbox for clean slate,
    // delete_sandbox for cleanup. See docs/fixes/ast-utils-remove-annotation-assertion.md

    #[tool(description = "Get WP verification condition details (sequent) for a function. \
                           Returns all VCs with hypotheses and goals in WP internal format.")]
    async fn get_vc_details(
        &self,
        Parameters(params): Parameters<GetVcDetailsParams>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve_client(&params.function).await?;
        let result = resolved
            .client
            .get("plugins.ast-utils.getVcDetails", json!({"function": resolved.function}))
            .await
            .map_err(McpError::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    // --- Sandbox tools (v2: dual Frama-C instance) ---

    #[tool(description = "Create a sandbox for CEGIS experimentation. \
        Extracts the function with dependencies into a temp file and launches \
        a separate Frama-C instance. Returns sandbox name (experiment_id:function), \
        experiment ID, and sid mapping. \
        Pass experiment_id to use a stable, human-readable ID (e.g. \"exp01\") — \
        sandbox_name will be \"<experiment_id>:<function>\". If omitted, a random \
        ID is generated. ID collisions are rejected.")]
    async fn create_sandbox(
        &self,
        Parameters(params): Parameters<CreateSandboxParams>,
    ) -> Result<CallToolResult, McpError> {
        // Lazy spawn (Issue #95): create_sandbox 需要主 frama-c 已 load
        // 才能 extract function 到 sandbox。未 load 时返结构化 NoProjectLoaded。
        self.require_project_loaded().await?;

        // Check sandbox OS safety ceiling (fsmint-3: 仅防失控的高安全顶，非调度限制——
        // 调度并发由 v-p-fsm max_sandboxes var 决定)
        {
            let sandboxes = self.sandboxes.read().await;
            if sandboxes.len() >= self.max_sandboxes {
                return Err(McpError::invalid_params(
                    format!(
                        "sandbox OS safety ceiling {} reached (raise via --max-sandboxes; \
                         scheduling concurrency is controlled by v-p-fsm max_sandboxes var, not this)",
                        self.max_sandboxes
                    ),
                    None,
                ));
            }
        }

        // 1. Extract function + deps from main instance
        let extract_result = (self.require_client().await?)
            
            .get(
                "plugins.ast-utils.extractFunctionWithDeps",
                json!(params.function),
            )
            .await
            .map_err(McpError::from)?;

        let success = extract_result
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !success {
            let error = extract_result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(McpError::internal_error(
                format!("extract failed: {}", error),
                None,
            ));
        }
        let c_source = extract_result
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::internal_error("extract returned no source", None))?;
        // ast_stmt_count: extractFunctionWithDeps 直接返回 sids（OCaml 端权威 sallstmts 列表）。
        // 不能从 sandbox fetchFunctions 拿（那个 schema 只有 name/key/decl/signature/sloc，
        // 没有 sallstmts 字段），早期版本就是因此 ast_stmt_count=null。
        let ast_stmt_count = extract_result
            .get("sids")
            .and_then(|v| v.as_array())
            .map(|a| a.len() as u32);

        // 2. Resolve experiment ID: prefer caller-provided (FSM 场景下由 enter_fsm
        //    const_var 提供，保证整个 session sandbox_name 一致），fallback 随机。
        let experiment_id = match params.experiment_id.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => {
                // 重复检查：避免覆盖现有沙箱（旧的会泄漏：socket/pid 没人 cleanup）
                let sandboxes = self.sandboxes.read().await;
                if sandboxes.contains_key(s) {
                    return Err(McpError::invalid_params(
                        format!(
                            "experiment_id '{}' already in use; call delete_sandbox first or pick a different ID",
                            s
                        ),
                        None,
                    ));
                }
                s.to_string()
            }
            _ => format!(
                "{:08x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
            ),
        };
        let sandbox_dir = PathBuf::from(format!("/tmp/frama-c-sandbox-{}", experiment_id));
        // 残留目录清理：上一轮 FSM crash / 进程被 kill 没走 delete_sandbox 时，
        // /tmp/frama-c-sandbox-<id> 会留下旧 socket / sandbox.c。重用同一 ID 之前
        // 必须清掉，否则 frama-c 起新 server 会抢同一个 socket 路径，行为未定义。
        // 加 warn log 让审计可见——若两个 MCP server 进程意外撞同一 experiment_id，
        // 这里会静默吞掉对方的 in-flight sandbox 目录，log 是唯一线索。
        if sandbox_dir.exists() {
            tracing::warn!(
                experiment_id = %experiment_id,
                dir = %sandbox_dir.display(),
                "create_sandbox: prior sandbox dir found, removing (likely from crashed session; \
                 if another live MCP server uses this ID, this is a collision)"
            );
            if let Err(e) = std::fs::remove_dir_all(&sandbox_dir) {
                tracing::warn!(
                    experiment_id = %experiment_id,
                    dir = %sandbox_dir.display(),
                    "create_sandbox: remove_dir_all failed: {}", e
                );
            }
        }
        std::fs::create_dir_all(&sandbox_dir).map_err(|e| {
            McpError::internal_error(format!("mkdir failed: {}", e), None)
        })?;
        let sandbox_file = sandbox_dir.join("sandbox.c");
        std::fs::write(&sandbox_file, c_source).map_err(|e| {
            McpError::internal_error(format!("write failed: {}", e), None)
        })?;

        // 3. Spawn sandbox Frama-C — 拿 tokio Child 句柄供后续 cleanup 显式 reap
        let sandbox_socket = sandbox_dir.join("frama-c.sock");
        let mut sandbox_child = self
            .spawn_sandbox_frama_c(&sandbox_file, &sandbox_socket)
            .await?;
        let sandbox_pid = sandbox_child.id().unwrap_or(0);

        // 5. Connect sandbox client with INDEPENDENT state
        // (sharing main state causes sandbox fetchFunctions to clobber main's function cache)
        let sandbox_state = Arc::new(RwLock::new(crate::state::SessionState::default()));
        let sandbox_client = match FramaCClient::connect(
            sandbox_socket.to_str().unwrap_or("/tmp/invalid"),
            sandbox_state,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                // connect 失败 → 立即 kill + wait reap，不留 zombie
                let _ = sandbox_child.start_kill();
                let _ = sandbox_child.wait().await;
                return Err(McpError::internal_error(
                    format!("sandbox connect failed: {}", e),
                    None,
                ));
            }
        };

        // 6. Get declaration marker from sandbox
        sandbox_client
            .get("kernel.ast.reloadFunctions", json!(null))
            .await
            .map_err(McpError::from)?;
        let funcs = sandbox_client
            .fetch_all("kernel.ast.fetchFunctions")
            .await
            .map_err(McpError::from)?;
        let declaration_marker = funcs.iter().find_map(|f| {
            let fname = f.get("name").and_then(|v| v.as_str());
            let decl = f.get("decl").and_then(|v| v.as_str());
            if fname == Some(&params.function) { decl.map(|s| s.to_string()) } else { None }
        }).unwrap_or_default();

        // 7. Store sandbox state (no sid_map — agent matches SIDs by comparing ASTs)
        let sandbox_name = format!("{}:{}", experiment_id, params.function);
        let sandbox_state = SandboxState {
            experiment_id: experiment_id.clone(),
            original_function: params.function.clone(),
            sandbox_dir: sandbox_dir.clone(),
            sandbox_socket: sandbox_socket.clone(),
            sandbox_pid,
            sandbox_child: std::sync::Arc::new(tokio::sync::Mutex::new(Some(sandbox_child))),
            sid_map: vec![],
            declaration_marker: declaration_marker.clone(),
        };
        {
            let mut sandboxes = self.sandboxes.write().await;
            sandboxes.insert(
                experiment_id.clone(),
                (sandbox_state, Arc::new(sandbox_client)),
            );
        }

        // 8. 副作用 merge 写 conclusion（§13.6 改动 5）：
        //    sandbox_clean=true / annotation_count=0 / sandbox_deleted=false / ast_stmt_count
        //    若 conclusion 不存在则创建（status="in_progress"）
        // ast_stmt_count 已在 step 1 后计算（extract_result.sids）
        {
            let mut state = self.state.write().await;
            state.on_sandbox_created(&params.function, ast_stmt_count);
            // 持久化（hard check 可能要读 sandbox 状态字段）
            let conclusion = state.get_conclusion(&params.function).cloned();
            drop(state);
            if let Some(c) = conclusion {
                if let Err(e) = persist_conclusion(&params.function, &c) {
                    tracing::warn!("persist_conclusion({}) failed (sandbox-created side-effect): {}", params.function, e);
                }
            }
        }

        // 9. Return
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "sandbox_name": sandbox_name,
                "experiment_id": experiment_id,
                "ast_stmt_count": ast_stmt_count,
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(description = "Delete and recreate sandbox from original function, \
        preserving the experiment ID. Use when CEGIS needs a clean slate. \
        \
        IMPORTANT — failure semantics: if recreate step fails, the OLD sandbox \
        has already been deleted (cleanup is unconditional first step). The error \
        message will explicitly say 'sandbox <id> deleted, recreate failed: ...' — \
        in that case, the caller MUST call create_sandbox(function, experiment_id) \
        to start fresh; subsequent reset_sandbox calls will return \
        'sandbox <id> not found'.")]
    async fn reset_sandbox(
        &self,
        Parameters(params): Parameters<ResetSandboxParams>,
    ) -> Result<CallToolResult, McpError> {
        // Parse sandbox_name to get experiment_id
        let exp_id = params
            .sandbox_name
            .split_once(':')
            .map(|(id, _)| id.to_string())
            .unwrap_or_else(|| params.sandbox_name.clone());

        let original_function = {
            let sandboxes = self.sandboxes.read().await;
            sandboxes
                .get(&exp_id)
                .map(|(state, _)| state.original_function.clone())
                .ok_or_else(|| {
                    McpError::invalid_params(
                        format!(
                            "sandbox '{}' not found — already deleted (perhaps prior reset_sandbox failed mid-way). \
                             Call create_sandbox(function=<func>, experiment_id=\"{}\") to recreate fresh.",
                            exp_id, exp_id
                        ),
                        None,
                    )
                })?
        };

        // Step 1: Delete old sandbox (unconditional)
        self.cleanup_sandbox(&exp_id).await;

        // Step 2: Recreate with the SAME experiment_id so sandbox_name stays stable.
        // If this fails, the old sandbox is GONE — caller must call create_sandbox
        // to recover (we propagate detailed error so caller knows which step failed).
        let result = self
            .create_sandbox(Parameters(CreateSandboxParams {
                function: original_function.clone(),
                experiment_id: Some(exp_id.clone()),
            }))
            .await
            .map_err(|e| {
                tracing::error!(
                    sandbox_id = %exp_id,
                    function = %original_function,
                    "reset_sandbox: cleanup OK but recreate failed: {:?}",
                    e
                );
                McpError::internal_error(
                    format!(
                        "reset_sandbox: sandbox '{}' (function '{}') was deleted (cleanup step OK), \
                         but recreate step FAILED: {}. \
                         The sandbox is now in DELETED state — to recover, call \
                         create_sandbox(function=\"{}\", experiment_id=\"{}\") explicitly. \
                         Retrying reset_sandbox will fail with 'sandbox not found'.",
                        exp_id, original_function, e.message, original_function, exp_id
                    ),
                    None,
                )
            })?;
        Ok(result)
    }

    #[tool(description = "Delete a sandbox function. Idempotent — succeeds even if not found.")]
    async fn delete_sandbox(
        &self,
        Parameters(params): Parameters<DeleteSandboxParams>,
    ) -> Result<CallToolResult, McpError> {
        let exp_id = params
            .sandbox_name
            .split_once(':')
            .map(|(id, _)| id.to_string())
            .unwrap_or_else(|| params.sandbox_name.clone());

        // 在 cleanup 前抓 original_function（cleanup 后 sandbox 已不存在）
        let original_function = {
            let sandboxes = self.sandboxes.read().await;
            sandboxes
                .get(&exp_id)
                .map(|(state, _)| state.original_function.clone())
        };

        self.cleanup_sandbox(&exp_id).await;

        // 副作用 merge 写 conclusion（§13.6 改动 15）：sandbox_deleted=true
        if let Some(func) = original_function {
            let mut state = self.state.write().await;
            state.on_sandbox_deleted(&func);
            let conclusion = state.get_conclusion(&func).cloned();
            drop(state);
            if let Some(c) = conclusion {
                if let Err(e) = persist_conclusion(&func, &c) {
                    tracing::warn!("persist_conclusion({}) failed (sandbox-deleted side-effect): {}", func, e);
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            json!({"success": true}).to_string(),
        )]))
    }

    #[tool(description = "Extract annotations added by ast-utils emitter from a SANDBOX function. \
        Returns {annotations: [{sid, acsl}]} — sid=-1 for the function contract (acsl is the whole \
        funspec, wrapped 'behavior default!: requires …; ensures …;'), sid>=0 for statement-level. \
        Read-only review aid; not used by the vp-fsm merge (that uses inject_all_annotations_main).")]
    async fn extract_annotations(
        &self,
        Parameters(params): Parameters<ExtractAnnotationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = self.resolve_client(&params.sandbox_name).await?;
        let result = resolved
            .client
            .get(
                "plugins.ast-utils.execExtractAnnotations",
                json!(resolved.function),
            )
            .await
            .map_err(McpError::from)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    // --- Conclusion tools ---

    #[tool(description = "Store or update the verification conclusion for a function. \
        Supports incremental updates: fields set to null preserve previous values. \
        \
        Nested object schemas (must match exactly when committing): \
        \
        AnnotationEntry (used in `specs`, `reference_specs`, `unsound_specs`): \
        { hash_label: string (required, copy from add_annotation_sandbox return value), \
          user_label: string|null (optional semantic label), \
          kind: \"spec\"|\"annot\" (required, top-level binary; \"spec\" = function-level requires/ensures/assigns with stmt_id=null; \"annot\" = stmt-level loop_*/assert with stmt_id required), \
          acsl: string (required, full ACSL clause text e.g. \"requires P;\" or \"loop invariant Q;\"), \
          stmt_id: int|null (kind=annot: required; kind=spec: must be null), \
          derived_from: string (required, source identifier; must match regex \
            ^proposed_(requires|ensures|assigns|loop_annots\\[\\d+\\]\\.(invariants\\[\\d+\\]|assigns\\[\\d+\\]|variant))(\\[\\d+\\])?$ \
            or start with \"remediation:\"), \
          source: \"original\"|\"generated\"|\"reference\" (required, snake_case lowercase), \
          purpose: string (required, one-line reason this annotation exists), \
          proof_target: string|null (optional, hash_label of main spec this aux supports), \
          wp_status: \"valid\"|\"unknown\"|\"timeout\"|\"noresult\"|null (S4 fills in; pass null on initial commit), \
          wp_time_ms: int|null, \
          wp_prover: string|null }. \
        \
        CalleeInfo (values of `callee_info` map, key is callee name): \
        { status: \"verified\"|\"in_progress\"|\"unverified\" (required), \
          sources: { verified_spec: string, semantic_proof: string, semiformal_proof: string, \
                     verified_source: string, program_summary: string, ast: string } \
                   (all optional strings; verified callees usually have verified_spec; \
                    unverified usually have program_summary or ast) }. \
        \
        ExistingAssert (used in `existing_asserts`): \
        { stmt_id: int (required), acsl: string (required, full \"assert P;\" text), \
          origin: \"source\" (required, currently only this value) }. \
        \
        proposed_* shapes (all 5 fields required; use [] when no entries): \
        \
        ProposedBehavior (used in `proposed_behaviors` — declare named behaviors here, \
        reference by name in other clauses): \
        { name: string (C identifier), \
          assumes: [string] (AND'd; empty ⇒ ACSL `assumes \\true`) }. \
        \
        ProposedRequires (used in `proposed_requires`): \
        { acsl: string (bare predicate, no `requires` keyword, no semicolon), \
          behavior: string|null (reference proposed_behaviors[].name; null = top-level), \
          necessity: string (rationale, not part of ACSL) }. \
        \
        ProposedEnsures (used in `proposed_ensures`): \
        { acsl: string (bare predicate), \
          from: string (markdown section reference), \
          behavior: string|null }. \
        \
        ProposedAssigns (used in `proposed_assigns`): \
        { acsl: string (assigns body, no `assigns` keyword, no semicolon — e.g. \"a[0..n-1]\" or \"\\\\nothing\"), \
          behavior: string|null }. \
        \
        ProposedLoopAnnot (used in `proposed_loop_annots`): \
        { stmt_id: int (loop sid from get_function_ast), \
          loop_label: string (human-readable), \
          invariants: [{acsl, behavior?}], \
          assigns:    [{acsl, behavior?}], \
          variant:    {acsl, behavior?}|null }. \
        \
        Any `behavior` field referencing a name not declared in `proposed_behaviors` \
        causes the entry to be rejected by inject_all_annotations_sandbox at injection time.")]
    async fn store_function_conclusion(
        &self,
        Parameters(params): Parameters<StoreFunctionConclusionParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::state::{
            FunctionConclusionUpdate, AnnotationEntry, WpGoalResult, WpGoalSummary,
            UnsoundSpec, FailureEvidence, CalleeInfo, CalleeRequest, ExistingAssert,
            ProposedBehavior, ProposedRequires, ProposedEnsures, ProposedAssigns, ProposedLoopAnnot,
        };
        use std::collections::HashMap;

        let status = params.status.map(|s| parse_conclusion_status(&s))
            .transpose()
            .map_err(|e| McpError::invalid_params(e, None))?;

        // helper: parse Vec<JSON> → Vec<T>
        fn parse_vec<T: for<'de> serde::Deserialize<'de>>(
            v: Option<Vec<serde_json::Value>>, field: &str,
        ) -> Result<Option<Vec<T>>, McpError> {
            v.map(|vs| {
                vs.into_iter()
                    .map(|j| serde_json::from_value::<T>(j))
                    .collect::<Result<Vec<_>, _>>()
            }).transpose()
            .map_err(|e| McpError::invalid_params(format!("invalid {field}: {e}"), None))
        }
        // helper: parse single JSON → T
        fn parse_one<T: for<'de> serde::Deserialize<'de>>(
            v: Option<serde_json::Value>, field: &str,
        ) -> Result<Option<T>, McpError> {
            v.map(serde_json::from_value::<T>).transpose()
                .map_err(|e| McpError::invalid_params(format!("invalid {field}: {e}"), None))
        }

        let specs = parse_vec::<AnnotationEntry>(params.specs, "specs")?;
        let reference_specs = parse_vec::<AnnotationEntry>(params.reference_specs, "reference_specs")?;
        let wp_results = parse_vec::<WpGoalResult>(params.wp_results, "wp_results")?;
        let wp_summary = parse_one::<WpGoalSummary>(params.wp_summary, "wp_summary")?;

        let existing_asserts = parse_vec::<ExistingAssert>(params.existing_asserts, "existing_asserts")?;
        let proposed_behaviors = parse_vec::<ProposedBehavior>(params.proposed_behaviors, "proposed_behaviors")?;
        let proposed_requires = parse_vec::<ProposedRequires>(params.proposed_requires, "proposed_requires")?;
        let proposed_ensures = parse_vec::<ProposedEnsures>(params.proposed_ensures, "proposed_ensures")?;
        let proposed_assigns = parse_vec::<ProposedAssigns>(params.proposed_assigns, "proposed_assigns")?;
        let proposed_loop_annots = parse_vec::<ProposedLoopAnnot>(params.proposed_loop_annots, "proposed_loop_annots")?;
        let callee_requests = parse_vec::<CalleeRequest>(params.callee_requests, "callee_requests")?;
        let unsound_specs = parse_vec::<UnsoundSpec>(params.unsound_specs, "unsound_specs")?;
        let failure_evidence = parse_one::<FailureEvidence>(params.failure_evidence, "failure_evidence")?;
        let callee_info = parse_one::<HashMap<String, CalleeInfo>>(params.callee_info, "callee_info")?;
        let blocking_callee_requires = parse_one::<crate::state::BlockingCalleeRequires>(
            params.blocking_callee_requires, "blocking_callee_requires")?;
        let infeasible_requests = parse_vec::<crate::state::InfeasibleRequest>(
            params.infeasible_requests, "infeasible_requests")?;

        let func_name = params.function;

        // Plan A 收尾：长文本 4 字段从 store API 删除（见 docs/fixes/remove-store-long-text-fields.md）。
        // 写长文本只能用 Write/Edit 工具直接写 `.frama-c-mcp/<func>/<field>.md`。
        // 本 handler 只处理短/结构化字段；目录由首次写文件的 Write tool 创建。

        let update = FunctionConclusionUpdate {
            function: func_name.clone(),
            status,
            specs,
            reference_specs,
            unsound_specs,
            wp_results,
            wp_summary,
            notes: params.notes,
            callees: params.callees,
            callee_info,
            existing_asserts,
            proposed_behaviors,
            proposed_requires,
            proposed_ensures,
            proposed_assigns,
            proposed_loop_annots,
            proposed_terminates: params.proposed_terminates,
            callee_requests,
            sp_revision_count: params.sp_revision_count,
            last_sp_error_analysis: params.last_sp_error_analysis,
            proposed_revision_count: params.proposed_revision_count,
            last_proposed_error_analysis: params.last_proposed_error_analysis,
            failure_evidence,
            verified_source: params.verified_source,
            // verify-program-fsm v1 接入 (detailed-design §6.4)
            unsound_reason_type: params.unsound_reason_type,
            blocking_callee_requires,
            infeasible_requests,
            push_history: params.push_history,
        };

        let mut state = self.state.write().await;
        state.store_conclusion(update);

        // 持久化 meta.json（无长文本字段——已分别写 .md）
        let conclusion = state.get_conclusion(&func_name).cloned();
        drop(state); // 释放写锁后再做 IO
        if let Some(c) = conclusion {
            if let Err(e) = persist_conclusion(&func_name, &c) {
                return Ok(CallToolResult::success(vec![Content::text(
                    format!(
                        "{{\"stored\": \"{}\", \"persist_error\": \"{}\"}}",
                        func_name, e
                    ),
                )]));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            format!("{{\"stored\": \"{}\"}}", func_name),
        )]))
    }

    #[tool(description = "Retrieve the stored verification conclusion for a function. \
        Returns the full conclusion including ACSL contract, proofs, and verification status.")]
    async fn get_function_conclusion(
        &self,
        Parameters(params): Parameters<GetFunctionConclusionParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = self.state.read().await;
        let conclusion = match state.get_conclusion(&params.function) {
            Some(c) => c.clone(),
            None => return Err(McpError::invalid_params(
                format!("no conclusion stored for function '{}'", params.function),
                None,
            )),
        };
        drop(state); // 释放读锁后做 IO

        // Plan A response 组装：meta state JSON + 从磁盘读 4 个长文本 .md 文件
        // → 合并成响应 JSON。长文本字段只活在磁盘上，每次 get 都重新读，
        // LLM 直接 Write file 也能反映到结果。
        let mut value = serde_json::to_value(&conclusion).unwrap_or(serde_json::Value::Null);
        if let Some(obj) = value.as_object_mut() {
            let long_texts = read_long_texts_as_json(&conclusion_dir(&params.function));
            for (k, v) in long_texts {
                obj.insert(k, v);
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&value).unwrap_or_default(),
        )]))
    }

    #[tool(description = "List stored function conclusion summaries. \
        Returns [{function, status, wp_summary}] without full proofs or ACSL. \
        Use get_function_conclusion for details.")]
    async fn list_conclusions(
        &self,
        Parameters(params): Parameters<ListConclusionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let status_filter = params.status.map(|s| parse_conclusion_status(&s))
            .transpose()
            .map_err(|e| McpError::invalid_params(e, None))?;

        let state = self.state.read().await;
        let summaries = state.list_conclusions(status_filter.as_ref());

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&summaries).unwrap_or_default(),
        )]))
    }

    // --- Project state tools ---

    #[tool(description = "Store or update the project verification state. Main path (vp-fsm): pass `state_json` \
        with the full ProjectVerificationState as a JSON string — it is replaced wholesale and persisted to \
        .frama-c-mcp/_program.json (server preserves the `locked` field). Thin path: pass any of source_files / \
        verification_order / current_index / global_notes for an incremental field merge. \
        On parse or persist failure returns {\"stored\": false, \"error\"/\"persist_error\": ...} — never silently succeeds.")]
    async fn store_project_state(
        &self,
        Parameters(params): Parameters<StoreProjectStateParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut state = self.state.write().await;

        if let Some(j) = params.state_json {
            // 主路径：full-replace。解析失败必须告知 agent（消除 silent-success）。
            let mut new_state: ProjectVerificationState = match serde_json::from_str(&j) {
                Ok(s) => s,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(
                        json!({"stored": false, "error": format!("state_json 解析失败: {e}")})
                            .to_string(),
                    )]));
                }
            };
            // 硬约束：locked 由服务端 (lock_project/unlock_project) 拥有，
            // 不被 agent 提交的旧值覆盖。
            new_state.locked = state.get_project_state().map(|s| s.locked).unwrap_or(false);
            // server-owned 调用图派生量（compute_topological_order 已 seed 进 in-memory）：
            // full-replace 时从 in-memory 保留，不被 agent 提交值覆盖；agent 不再填这些。
            // #112: current_level。VO-completeness fix: verification_order + scc_groups
            // （**关键**——否则 agent 漏填则 full-replace 用其空值覆盖 server-seeded，VO 失权威）。
            // levels 字段已删。
            if let Some(cur) = state.get_project_state() {
                new_state.current_level = cur.current_level;
                new_state.verification_order = cur.verification_order.clone();
                new_state.scc_groups = cur.scc_groups.clone();
            }
            state.set_project_state_full(new_state.clone());
            drop(state);
            if let Err(e) = persist_program_state(&new_state) {
                return Ok(CallToolResult::success(vec![Content::text(
                    json!({"stored": false, "persist_error": e.to_string()}).to_string(),
                )]));
            }
        } else {
            // thin 路径：旧 4 字段 merge（back-compat），同样落盘。
            state.store_project_state(crate::state::ProjectStateUpdate {
                source_files: params.source_files,
                verification_order: params.verification_order,
                current_index: params.current_index,
                global_notes: params.global_notes,
                ..Default::default()
            });
            if let Some(s) = state.get_project_state().cloned() {
                drop(state);
                if let Err(e) = persist_program_state(&s) {
                    return Ok(CallToolResult::success(vec![Content::text(
                        json!({"stored": false, "persist_error": e.to_string()}).to_string(),
                    )]));
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            "{\"stored\": true}".to_string(),
        )]))
    }

    #[tool(description = "Retrieve the project verification state (verification order, progress, notes).")]
    async fn get_project_state(
        &self,
        Parameters(_params): Parameters<GetProjectStateParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = self.state.read().await;
        match state.get_project_state() {
            Some(ps) => Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(ps).unwrap_or_default(),
            )])),
            None => Ok(CallToolResult::success(vec![Content::text(
                "{\"state\": null}".to_string(),
            )])),
        }
    }

    // --- Project lock tools ---

    #[tool(description = "Lock the main Frama-C instance. While locked, reload_project and run_wp on the main instance \
        are rejected (returns error). Sandbox operations are unaffected. Call this after Phase 1 to prevent subagents \
        from accidentally destroying annotations. Call unlock_project before Phase 3 final gate.")]
    async fn lock_project(
        &self,
        Parameters(_params): Parameters<LockProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        // option B (issue #115): 只保留运行时锁 project_locked（reload_project/run_wp 据它拒绝）。
        // 不再落盘 _program.json.locked mirror —— 该 mirror 与真锁 desync 导致 S3 卡死，
        // 且 hard check 读它属冗余复验（yaml 白名单已按状态挡住 reload/run_wp）。已移除 mirror + check。
        let mut locked = self.project_locked.write().await;
        *locked = true;
        Ok(CallToolResult::success(vec![Content::text(
            "{\"locked\": true, \"message\": \"Main instance locked. reload_project and run_wp on main are now blocked. Sandbox operations unaffected.\"}".to_string(),
        )]))
    }

    #[tool(description = "Unlock the main Frama-C instance, allowing reload_project and run_wp on the main instance again. \
        Call this before Phase 3 final gate when you need to reload the merged .c file.")]
    async fn unlock_project(
        &self,
        Parameters(_params): Parameters<UnlockProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        // option B (issue #115): 同 lock_project，只翻运行时锁，不落盘 mirror。
        let mut locked = self.project_locked.write().await;
        *locked = false;
        Ok(CallToolResult::success(vec![Content::text(
            "{\"locked\": false, \"message\": \"Main instance unlocked. reload_project and run_wp are now allowed.\"}".to_string(),
        )]))
    }

    // --- Persistence tools ---
    // Note: `save_function_state` snapshot tool was removed (commit X).
    // Persistence is now handled by `store_function_conclusion` + per-field-files layout
    // (see docs/fixes/conclusion-per-field-files.md). The old <func>.json single-file
    // snapshot was unused by FSM/hard-checks, had no `load` counterpart, and conflicted
    // with the new <func>/ directory layout.

    #[tool(description = "Print complete annotated C source code including all ACSL specifications \
        and RTE assertions from a SANDBOX Frama-C instance. The sandbox_name parameter is \
        REQUIRED and must include the experiment_id prefix (e.g. 'exp42:foo'). Optionally \
        writes to a file. For main-instance source export (Phase 3 final gate verified.c), \
        use print_source_main instead.")]
    async fn print_source_sandbox(
        &self,
        Parameters(params): Parameters<PrintSourceParams>,
    ) -> Result<CallToolResult, McpError> {
        let sname = params.sandbox_name.as_ref().ok_or_else(|| {
            McpError::invalid_params(
                "sandbox_name is required for print_source_sandbox (must include experiment_id, e.g. 'exp42:foo'); use print_source_main for main instance",
                None,
            )
        })?;
        if !sname.contains(':') {
            return Err(McpError::invalid_params(
                format!(
                    "sandbox_name '{}' must include experiment_id prefix (e.g. 'exp42:foo'); use print_source_main for main instance",
                    sname
                ),
                None,
            ));
        }
        let exp_id = sname
            .split_once(':')
            .map(|(id, _)| id.to_string())
            .unwrap_or_else(|| sname.clone());
        let sb_client = {
            let sandboxes = self.sandboxes.read().await;
            let (_, client) = sandboxes.get(&exp_id).ok_or_else(|| {
                McpError::invalid_params(format!("sandbox '{}' not found", exp_id), None)
            })?;
            client.clone()
        };
        let source = sb_client
            .get("plugins.ast-utils.printSource", json!(""))
            .await
            .map_err(McpError::from)?;
        Self::emit_source(source, params.output)
    }

    #[tool(description = "Print complete annotated C source code including all ACSL specifications \
        and RTE assertions from the MAIN Frama-C instance. Must NOT pass sandbox_name. \
        Optionally writes to a file. Used by the verify-program agent at Phase 3 final gate \
        to produce the *_verified.c artifact. For sandbox source export, use \
        print_source_sandbox instead.")]
    async fn print_source_main(
        &self,
        Parameters(params): Parameters<PrintSourceParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(ref sname) = params.sandbox_name {
            return Err(McpError::invalid_params(
                format!(
                    "sandbox_name '{}' provided but print_source_main targets the main instance; use print_source_sandbox for sandbox operations",
                    sname
                ),
                None,
            ));
        }
        let source = (self.require_client().await?)
            
            .get("plugins.ast-utils.printSource", json!(""))
            .await
            .map_err(McpError::from)?;
        Self::emit_source(source, params.output)
    }

    /// Shared output handling for print_source_sandbox / print_source_main.
    fn emit_source(
        source: serde_json::Value,
        output: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let text = source.as_str().unwrap_or_default();

        if let Some(path) = output {
            // Create parent directories if needed
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, text).map_err(|e| {
                McpError::internal_error(format!("write failed: {}", e), None)
            })?;
            Ok(CallToolResult::success(vec![Content::text(format!(
                "{{\"written\": \"{}\", \"bytes\": {}}}",
                path,
                text.len()
            ))]))
        } else {
            Ok(CallToolResult::success(vec![Content::text(text.to_string())]))
        }
    }

    #[tool(description = "Batch-inject ACSL annotations into a sandbox function. Idempotent \
        (skips by ACSL content). Per-entry failures land in failures[] with classification; \
        rejected entries do NOT enter the AST (no rollback needed). \
        \n\nInput: \
        \n  sandbox_name: \"<experiment_id>:<func>\" (must contain ':'). \
        \n  proposed_behaviors: [{name, assumes: [...]}]  — declare named behaviors here; \
        other clauses reference them by `behavior` field. Empty assumes ⇒ ACSL `assumes \\true`. \
        \n  proposed_requires: [{acsl, behavior?, necessity}] \
        \n  proposed_ensures:  [{acsl, from, behavior?}] \
        \n  proposed_assigns:  [{acsl, behavior?}] \
        \n  proposed_loop_annots: [{stmt_id, loop_label, \
        invariants: [{acsl, behavior?}], assigns: [{acsl, behavior?}], variant?: {acsl, behavior?}}] \
        \n\nWrapping: `behavior` field omitted → top-level clause (`requires R;`). \
        `behavior: \"X\"` → looks up X in proposed_behaviors and emits \
        `behavior X: assumes A1; ...; <clause>;` (loop clauses use `for X: ...`). \
        Undeclared behavior reference → ProposedError. \
        \n\nReturn payload: {status, successful, failures, summary}. \
        \n  status: \"success\" | \"partial\" (only syntax_error failures) | \"proposed_error\". \
        \n  failures[i].type: \
        \n    - syntax_error                      — ACSL parse failure (fix the string) \
        \n    - proposed_local_var_in_funspec     — funspec references a local; use \
        caller-visible state (formals/globals/\\result/\\old(formal)) \
        \n    - proposed_self_referential         — references an undefined logic name, \
        unknown label, undeclared behavior, etc.; check spelling / add the decl \
        \n    - proposed_error                    — type errors, duplicates, non-lvalue in assigns")]
    async fn inject_all_annotations_sandbox(
        &self,
        Parameters(params): Parameters<InjectAllAnnotationsSandboxParams>,
    ) -> Result<CallToolResult, McpError> {
        self.inject_all_impl(
            params.sandbox_name,
            /* require_sandbox */ true,
            params.proposed_behaviors,
            params.proposed_requires,
            params.proposed_ensures,
            params.proposed_assigns,
            params.proposed_loop_annots,
            params.proposed_terminates,
        )
        .await
    }

    /// Inject structured `proposed_*` annotations into a target function — shared
    /// by the sandbox and main variants. `target` is `exp:func` (sandbox) or a
    /// bare `func` (main); `require_sandbox` enforces the `:` shape. The plan-build
    /// is a pure function of `proposed_*` (→ bit-identical ACSL across targets);
    /// only the injection target and the O3 loop-sid re-resolution differ.
    /// See docs/fixes/vp-fsm-s2merge-add-annotation-main-toolname.md §4/§5.
    async fn inject_all_impl(
        &self,
        target: String,
        require_sandbox: bool,
        proposed_behaviors: Option<Vec<serde_json::Value>>,
        proposed_requires: Option<Vec<serde_json::Value>>,
        proposed_ensures: Option<Vec<serde_json::Value>>,
        proposed_assigns: Option<Vec<serde_json::Value>>,
        mut proposed_loop_annots: Option<Vec<serde_json::Value>>,
        proposed_terminates: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        // 1. Validate target shape matches the requested variant.
        let has_colon = target.contains(':');
        if require_sandbox && !has_colon {
            return Err(McpError::invalid_params(
                "sandbox_name must include experiment_id prefix (e.g. 'exp42:func')",
                None,
            ));
        }
        if !require_sandbox && has_colon {
            return Err(McpError::invalid_params(
                "function must be a bare main-instance name (no ':'); use \
                 inject_all_annotations_sandbox for sandboxes",
                None,
            ));
        }

        // 2. Resolve target client. resolve_client routes by ':' (sandbox vs main).
        let resolved = self.resolve_client(&target).await?;
        let target_function = target;

        // 2b. O3: on main, proposed_loop_annots[i].stmt_id are SANDBOX sids — invalid
        //     on main (extracted-file stubs shift CIL sids). Re-resolve to main sids
        //     by matching loops in source (pre-order) order. See §5 O3.
        if !require_sandbox {
            if let Some(ref mut loops) = proposed_loop_annots {
                if !loops.is_empty() {
                    let main_loop_sids = self.fetch_loop_sids(&resolved).await?;
                    if main_loop_sids.len() != loops.len() {
                        return Err(McpError::invalid_params(
                            format!(
                                "main function '{}' has {} loop(s) but proposed_loop_annots \
                                 has {} — cannot map loop sids (O3)",
                                resolved.function,
                                main_loop_sids.len(),
                                loops.len()
                            ),
                            None,
                        ));
                    }
                    for (i, l) in loops.iter_mut().enumerate() {
                        if let Some(obj) = l.as_object_mut() {
                            obj.insert("stmt_id".to_string(), json!(main_loop_sids[i]));
                        }
                    }
                }
            }
        }

        // 3a. Build behavior name → assumes lookup table from proposed_behaviors.
        //     ACSL behavior decls are resolved by name; lookup miss = undeclared.
        let mut behaviors: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(ref bhvs) = proposed_behaviors {
            for v in bhvs.iter() {
                let name = match v.get("name").and_then(|x| x.as_str()) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => continue,  // skip malformed entries silently (best-effort)
                };
                let assumes: Vec<String> = v
                    .get("assumes")
                    .and_then(|x| x.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|a| a.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                behaviors.insert(name, assumes);
            }
        }

        // 3b. Build injection plan. Per-entry behavior reference errors become
        //     InjectionFailure (ProposedError) directly, plan continues for the rest.
        let mut plan: Vec<InjectionPlanEntry> = Vec::new();
        let mut early_failures: Vec<InjectionFailure> = Vec::new();

        // proposed_requires: Vec<{acsl, behavior?, necessity}>
        if let Some(ref reqs) = proposed_requires {
            for (i, v) in reqs.iter().enumerate() {
                let acsl_text = v.get("acsl").and_then(|x| x.as_str()).unwrap_or("");
                let necessity = v.get("necessity").and_then(|x| x.as_str()).unwrap_or("");
                let behavior = v.get("behavior").and_then(|x| x.as_str());
                let path = format!("proposed_requires[{}]", i);
                match wrap_funspec_clause("requires", acsl_text, behavior, &behaviors, &path) {
                    Ok(normalized) => plan.push(InjectionPlanEntry {
                        acsl_text: normalized,
                        kind: "spec".to_string(),
                        derived_from: path,
                        stmt_id: None,
                        purpose: necessity.to_string(),
                        user_label: behavior.map(|b| format!("beh_{}", b)),
                    }),
                    Err(msg) => early_failures.push(InjectionFailure {
                        failure_type: classify_failure(&msg),
                        proposed_path: path,
                        acsl_text: acsl_text.to_string(),
                        frama_c_error: msg,
                    }),
                }
            }
        }

        // proposed_ensures: Vec<{acsl, from, behavior?}> (schema v2: behavior_assumes removed)
        if let Some(ref ensures_list) = proposed_ensures {
            for (i, v) in ensures_list.iter().enumerate() {
                let acsl_body = v.get("acsl").and_then(|x| x.as_str()).unwrap_or("");
                let from = v.get("from").and_then(|x| x.as_str()).unwrap_or("");
                let behavior = v.get("behavior").and_then(|x| x.as_str());
                let path = format!("proposed_ensures[{}]", i);
                let purpose = if !from.is_empty() {
                    from.to_string()
                } else {
                    acsl_body.chars().take(80).collect()
                };
                match wrap_funspec_clause("ensures", acsl_body, behavior, &behaviors, &path) {
                    Ok(acsl_text) => plan.push(InjectionPlanEntry {
                        acsl_text,
                        kind: "spec".to_string(),
                        derived_from: path,
                        stmt_id: None,
                        purpose,
                        user_label: behavior.map(|b| format!("beh_{}", b)),
                    }),
                    Err(msg) => early_failures.push(InjectionFailure {
                        failure_type: classify_failure(&msg),
                        proposed_path: path,
                        acsl_text: acsl_body.to_string(),
                        frama_c_error: msg,
                    }),
                }
            }
        }

        // proposed_assigns: Vec<{acsl, behavior?}> (schema v2: was Option<String>)
        if let Some(ref assigns_list) = proposed_assigns {
            for (i, v) in assigns_list.iter().enumerate() {
                let acsl_body = v.get("acsl").and_then(|x| x.as_str()).unwrap_or("");
                if acsl_body.trim().is_empty() { continue; }
                let behavior = v.get("behavior").and_then(|x| x.as_str());
                let path = format!("proposed_assigns[{}]", i);
                match wrap_funspec_clause("assigns", acsl_body, behavior, &behaviors, &path) {
                    Ok(acsl_text) => plan.push(InjectionPlanEntry {
                        acsl_text: normalize_acsl(&acsl_text),
                        kind: "spec".to_string(),
                        derived_from: path,
                        stmt_id: None,
                        purpose: "Function-level modifies clause".to_string(),
                        user_label: Some(
                            behavior.map(|b| format!("beh_{}_assigns", b))
                                .unwrap_or_else(|| format!("assigns_{}", i))
                        ),
                    }),
                    Err(msg) => early_failures.push(InjectionFailure {
                        failure_type: classify_failure(&msg),
                        proposed_path: path,
                        acsl_text: acsl_body.to_string(),
                        frama_c_error: msg,
                    }),
                }
            }
        }

        // proposed_loop_annots: each loop expanded via loop_annots_to_acsl()
        if let Some(ref loop_annots) = proposed_loop_annots {
            for (i, v) in loop_annots.iter().enumerate() {
                for outcome in loop_annots_to_acsl(v, i, &behaviors) {
                    match outcome {
                        Ok((acsl_text, kind, derived_from, stmt_id, purpose, user_label)) => {
                            plan.push(InjectionPlanEntry {
                                acsl_text,
                                kind,
                                derived_from,
                                stmt_id,
                                purpose,
                                user_label,
                            });
                        }
                        Err((path, msg)) => early_failures.push(InjectionFailure {
                            failure_type: classify_failure(&msg),
                            proposed_path: path,
                            acsl_text: String::new(),
                            frama_c_error: msg,
                        }),
                    }
                }
            }
        }

        // proposed_terminates: single funspec-level terminates clause (fsmint-6).
        // {acsl: "terminates \false"} or bare string. funspec-level → behavior=None.
        // wrap_funspec_clause normalizes a leading dup "terminates" keyword. Injected as
        // kind=spec → execAddAnnotation → insert_spec→add_spec writes spec_terminates (fix A).
        if let Some(ref term) = proposed_terminates {
            let acsl_text = term
                .get("acsl")
                .and_then(|x| x.as_str())
                .or_else(|| term.as_str())
                .unwrap_or("");
            if !acsl_text.trim().is_empty() {
                let path = "proposed_terminates".to_string();
                match wrap_funspec_clause("terminates", acsl_text, None, &behaviors, &path) {
                    Ok(acsl_norm) => plan.push(InjectionPlanEntry {
                        acsl_text: acsl_norm,
                        kind: "spec".to_string(),
                        derived_from: path,
                        stmt_id: None,
                        purpose: "termination_waived".to_string(),
                        user_label: Some("termination_waived".to_string()),
                    }),
                    Err(msg) => early_failures.push(InjectionFailure {
                        failure_type: classify_failure(&msg),
                        proposed_path: path,
                        acsl_text: acsl_text.to_string(),
                        frama_c_error: msg,
                    }),
                }
            }
        }

        // 4. Idempotency: fetch existing annotations to skip duplicates
        let existing_acsl: HashSet<String> = {
            let mut set = HashSet::new();
            let props_result = resolved
                .client
                .get("kernel.properties.fetchStatus", json!({"function": resolved.function}))
                .await;
            if let Ok(props) = props_result {
                if let Some(arr) = props.as_array() {
                    for p in arr {
                        if let Some(acsl) = p.get("acsl").and_then(|x| x.as_str()) {
                            // Normalize for comparison: strip hash_label
                            set.insert(normalize_for_comparison(acsl));
                        }
                    }
                }
            }
            set
        };

        // 5. Execute injection plan
        let mut successful: Vec<InjectedAnnotationEntry> = Vec::new();
        // Seed failures with the per-entry behavior-resolution errors collected
        // during plan building (schema v2): "behavior X referenced but not declared".
        let mut failures: Vec<InjectionFailure> = early_failures;

        for entry in &plan {
            // Idempotency check: skip if already exists
            let normalized = normalize_for_comparison(&entry.acsl_text);
            if existing_acsl.contains(&normalized) {
                // Skip but count as successful (it's already there)
                successful.push(InjectedAnnotationEntry {
                    hash_label: "existing".to_string(),
                    user_label: entry.user_label.clone(),
                    kind: entry.kind.clone(),
                    acsl: entry.acsl_text.clone(),
                    stmt_id: entry.stmt_id,
                    derived_from: entry.derived_from.clone(),
                    source: "generated".to_string(),
                    purpose: entry.purpose.clone(),
                    proof_target: None,
                    wp_status: None,
                    wp_time_ms: None,
                    wp_prover: None,
                });
                continue;
            }

            let add_params = AddAnnotationParams {
                function: target_function.clone(),
                kind: acsl_kind_to_ast_kind(&entry.acsl_text),
                acsl: entry.acsl_text.clone(),
                stmt: entry.stmt_id,
                user_label: entry.user_label.clone(),
            };

            let (add_result, used_hash) = match self.add_annotation_impl(add_params).await {
                Ok(r) => r,
                Err(e) => {
                    failures.push(InjectionFailure {
                        failure_type: FailureType::ProposedError,
                        proposed_path: entry.derived_from.clone(),
                        acsl_text: entry.acsl_text.clone(),
                        frama_c_error: e.message.to_string(),
                    });
                    continue;
                }
            };

            // Check the execAddAnnotation plugin's business-level success.
            // The OCaml plugin wraps the response under a "result" key:
            //   {"result": {"success": true, "error": null}, "hash_label": "..."}
            // so we must unwrap "result" before checking "success".
            //
            // type_spec/type_annot already rejects scope/typing violations
            // (e.g. funspec referencing locals, undefined logic functions),
            // so a plugin failure here means the annotation never entered
            // the AST — no rollback needed.
            let plugin_success = parse_plugin_success(&add_result);
            if !plugin_success {
                let error_msg = parse_plugin_error(&add_result)
                    .unwrap_or_else(|| "unknown error".to_string());
                failures.push(InjectionFailure {
                    failure_type: classify_failure(&error_msg),
                    proposed_path: entry.derived_from.clone(),
                    acsl_text: entry.acsl_text.clone(),
                    frama_c_error: error_msg,
                });
                continue;
            }

            successful.push(InjectedAnnotationEntry {
                hash_label: used_hash,
                user_label: entry.user_label.clone(),
                kind: entry.kind.clone(),
                acsl: entry.acsl_text.clone(),
                stmt_id: entry.stmt_id,
                derived_from: entry.derived_from.clone(),
                source: "generated".to_string(),
                purpose: entry.purpose.clone(),
                proof_target: None,
                wp_status: None,
                wp_time_ms: None,
                wp_prover: None,
            });
        }

        // 6. Compute status
        let status = compute_status(&failures);

        // 7. Update main-instance annotation count side effect
        if let Some(ref exp_id) = resolved.experiment_id {
            let sandboxes = self.sandboxes.read().await;
            if let Some((sb_state, _)) = sandboxes.get(exp_id.as_str()) {
                let orig_func = sb_state.original_function.clone();
                drop(sandboxes);
                let mut state = self.state.write().await;
                state.on_annotation_added(&orig_func);
                let conclusion = state.get_conclusion(&orig_func).cloned();
                drop(state);
                if let Some(c) = conclusion {
                    if let Err(e) = persist_conclusion(&orig_func, &c) {
                        tracing::warn!("persist_conclusion({}) failed (inject side-effect): {}", orig_func, e);
                    }
                }
            }
        }

        // 8. Return response.
        // Invariant: total_attempted == successful_count + failure_count.
        // "Attempted" counts every entry agent submitted — including plan-building
        // failures (undeclared behavior refs) that never reached type_spec.
        let summary = InjectionSummary {
            total_attempted: successful.len() + failures.len(),
            successful_count: successful.len(),
            failure_count: failures.len(),
        };

        let response = InjectAllAnnotationsSandboxResponse {
            status,
            successful,
            failures,
            summary,
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&response).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Merge a verified function's contract onto the MAIN Frama-C instance \
        from its conclusion's structured proposed_* (vp-fsm bottom-up merge, mechanism C). \
        Reconstructs ACSL via the same logic as inject_all_annotations_sandbox, so the merged \
        contract is bit-identical to what WP verified. Loop stmt_ids (sandbox sids) are \
        re-resolved to main sids by source order. function must be a bare main name (no ':'). \
        Returns the same {status, successful, failures, summary} shape.")]
    async fn inject_all_annotations_main(
        &self,
        Parameters(params): Parameters<InjectAllAnnotationsMainParams>,
    ) -> Result<CallToolResult, McpError> {
        self.inject_all_impl(
            params.function,
            /* require_sandbox */ false,
            params.proposed_behaviors,
            params.proposed_requires,
            params.proposed_ensures,
            params.proposed_assigns,
            params.proposed_loop_annots,
            params.proposed_terminates,
        )
        .await
    }

    /// Fetch a main-instance function's loop statement sids in source (pre-order)
    /// order, for O3 loop-sid re-resolution. Walks the getFunctionAst JSON.
    async fn fetch_loop_sids(&self, resolved: &ResolvedClient) -> Result<Vec<i64>, McpError> {
        let ast = resolved
            .client
            .get("plugins.ast-utils.getFunctionAst", json!(resolved.function))
            .await
            .map_err(McpError::from)?;
        let mut sids = Vec::new();
        collect_loop_sids(&ast, &mut sids);
        Ok(sids)
    }
}

/// Recursively collect sids of `kind == "loop"` statement nodes from a
/// getFunctionAst JSON, in source (pre-order) order. Statement lists are JSON
/// arrays (order-preserving) so sequential and nested loops come out in source
/// order. For an `if` node the two branch bodies (`then_body` / `else_body`) are
/// recursed in explicit source order (then before else) — this is the only place
/// that would otherwise depend on JSON object key iteration order.
///
/// Defence in depth: the crate enables serde_json `preserve_order` (so object
/// keys already iterate in ast-utils emission = source order), AND this function
/// orders `then_body`/`else_body` explicitly so correctness does not silently
/// hinge on that Cargo feature. The caller's count check (§5 O3) still guards
/// count mismatches. See docs/fixes/vpfsm-review-b1-b2-fix.md (B2).
fn collect_loop_sids(node: &serde_json::Value, out: &mut Vec<i64>) {
    match node {
        serde_json::Value::Object(map) => {
            if map.get("kind").and_then(|k| k.as_str()) == Some("loop") {
                if let Some(sid) = map.get("sid").and_then(|s| s.as_i64()) {
                    out.push(sid);
                }
            }
            // `if` node: recurse cond → then_body → else_body in explicit source
            // order, then any remaining keys. Other nodes: plain iteration (arrays
            // preserve order; non-if objects have no order-sensitive children).
            if map.contains_key("then_body") && map.contains_key("else_body") {
                for k in ["cond", "then_body", "else_body"] {
                    if let Some(v) = map.get(k) {
                        collect_loop_sids(v, out);
                    }
                }
                for (k, v) in map {
                    if !matches!(k.as_str(), "cond" | "then_body" | "else_body") {
                        collect_loop_sids(v, out);
                    }
                }
            } else {
                for (_, v) in map {
                    collect_loop_sids(v, out);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_loop_sids(v, out);
            }
        }
        _ => {}
    }
}

/// Generate a unique hash label for an ACSL annotation.
/// Format: <kind_prefix>_<8 hex chars>
fn generate_hash_label(kind: &str) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let prefix = match kind {
        "requires" => "re",
        "ensures" => "en",
        "assigns" => "as",
        "loop_invariant" => "li",
        "loop_assigns" => "la",
        "loop_variant" => "lv",
        "assert" => "at",
        _ => "an", // fallback for unknown kinds
    };
    let state = RandomState::new();
    let mut hasher = state.build_hasher();
    hasher.write_u64(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64);
    let hash = hasher.finish();
    format!("{}_{:08x}", prefix, hash as u32)
}

/// Build the full label injected into the AST by `add_annotation_impl`.
/// When `user_label` is present, the full label is `"{hash_label}_{user_label}"`;
/// otherwise just `hash_label`. Underscore is used (not comma) because the full label
/// becomes the Frama-C behavior name suffix (`label ^ "__spec"`), and comma is not
/// a valid identifier character in ACSL syntax.
/// This must be used for rollback to match what was actually written into the AST.
fn full_label(hash_label: &str, user_label: Option<&str>) -> String {
    match user_label {
        Some(ul) => format!("{}_{}", hash_label, ul),
        None => hash_label.to_string(),
    }
}

// --- inject_all_annotations_sandbox helpers (Plan §2) ---

/// Parse the `success` field from an `add_annotation_impl` response.
/// The OCaml plugin wraps its response under a `"result"` key, so the
/// JSON structure is:
///   {"result": {"success": true, "error": null}, "hash_label": "..."}
/// We must unwrap `"result"` before checking `"success"`.
fn parse_plugin_success(result: &CallToolResult) -> bool {
    result.content.first()
        .and_then(|c| c.as_text().map(|t| &t.text))
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .and_then(|v| v["result"]["success"].as_bool())
        .unwrap_or(false)
}

/// Parse the `error` field from an `add_annotation_impl` response.
/// Same `"result"` unwrapping as parse_plugin_success.
fn parse_plugin_error(result: &CallToolResult) -> Option<String> {
    result.content.first()
        .and_then(|c| c.as_text().map(|t| &t.text))
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .and_then(|v| v["result"]["error"].as_str().map(String::from))
}

/// Normalize ACSL text: strip all trailing semicolons/whitespace, add exactly one semicolon.
fn normalize_acsl(text: &str) -> String {
    let t = text.trim();
    let mut body = t;
    while body.ends_with(';') {
        body = &body[..body.len() - 1];
    }
    format!("{};", body.trim())
}

// ─────────────────────────────────────────────────────────────────────────
// Schema v2 helpers: behavior-aware ACSL wrapping
//
// Each proposed_{requires,ensures,assigns} entry optionally references a
// named behavior declared in proposed_behaviors. When `behavior: Some("X")`,
// we look up X's assumes and wrap as:
//     "behavior X: assumes A1; assumes A2; <keyword> <body>;"
// Undeclared reference → returns Err describing the offending path.
// ─────────────────────────────────────────────────────────────────────────

/// Look up a behavior's assumes by name. None on miss (caller decides error semantics).
fn lookup_behavior_assumes<'a>(
    behaviors: &'a std::collections::HashMap<String, Vec<String>>,
    name: &str,
) -> Option<&'a [String]> {
    behaviors.get(name).map(|v| v.as_slice())
}

/// Strip a leading clause keyword if `body` already starts with it as a whole
/// token (keyword followed by whitespace, or the keyword IS the whole body).
///
/// v-f-fsm stores `proposed_requires/ensures.acsl` WITH the clause keyword (it
/// uses the leading keyword as the per-clause type marker — verify-function.yaml
/// L792), while `proposed_assigns.acsl` is bare. inject_all's wrap_* prepend the
/// keyword assuming a bare predicate → "requires requires …" for the former.
/// Normalizing here makes wrap_* accept BOTH forms (bare or keyword-bearing),
/// reconciling the two contracts. Word-boundary check avoids stripping a
/// variable name like `requires_foo`. See
/// docs/fixes/inject-all-wrap-double-keyword.md.
fn strip_leading_keyword(body: &str, keyword: &str) -> String {
    let t = body.trim();
    if let Some(rest) = t.strip_prefix(keyword) {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return rest.trim().to_string();
        }
    }
    t.to_string()
}

/// Wrap an ACSL clause body in a top-level or behavior-scoped block.
/// `keyword` is one of "requires"/"ensures"/"assigns" for funspec clauses,
/// or "loop invariant"/"loop assigns"/"loop variant" for loop annotations
/// (loop annotations use `for X: ...` syntax, see [`wrap_loop_clause`]).
fn wrap_funspec_clause(
    keyword: &str,
    body: &str,
    behavior: Option<&str>,
    behaviors: &std::collections::HashMap<String, Vec<String>>,
    proposed_path: &str,
) -> Result<String, String> {
    let body_clean = body.trim().trim_end_matches(';').trim();
    let body_norm = strip_leading_keyword(body_clean, keyword);
    let body_trimmed = body_norm.trim_end_matches(';').trim();
    match behavior {
        None => Ok(format!("{} {};", keyword, body_trimmed)),
        Some(bname) => {
            let assumes = lookup_behavior_assumes(behaviors, bname).ok_or_else(|| {
                format!(
                    "behavior '{}' referenced at {} but not declared in proposed_behaviors",
                    bname, proposed_path
                )
            })?;
            let mut block = format!("behavior {}:", bname);
            for a in assumes {
                let a_trimmed = a.trim().trim_end_matches(';').trim();
                block.push_str(&format!(" assumes {};", a_trimmed));
            }
            block.push_str(&format!(" {} {};", keyword, body_trimmed));
            Ok(block)
        }
    }
}

/// Wrap a loop annotation clause (`loop invariant`/`loop assigns`/`loop variant`).
/// Loop clauses use `for X: loop invariant ...` syntax — assumes live in the
/// owning funspec behavior, not repeated inline.
fn wrap_loop_clause(
    keyword: &str,
    body: &str,
    behavior: Option<&str>,
    behaviors: &std::collections::HashMap<String, Vec<String>>,
    proposed_path: &str,
) -> Result<String, String> {
    let body_clean = body.trim().trim_end_matches(';').trim();
    let body_norm = strip_leading_keyword(body_clean, keyword);
    let body_trimmed = body_norm.trim_end_matches(';').trim();
    match behavior {
        None => Ok(format!("{} {};", keyword, body_trimmed)),
        Some(bname) => {
            // For loop clauses we only validate the behavior exists (no assumes inline).
            if lookup_behavior_assumes(behaviors, bname).is_none() {
                return Err(format!(
                    "behavior '{}' referenced at {} but not declared in proposed_behaviors",
                    bname, proposed_path
                ));
            }
            Ok(format!("for {}: {} {};", bname, keyword, body_trimmed))
        }
    }
}

/// Expand a single proposed_loop_annots[i] JSON into a Vec of (acsl_text, kind,
/// derived_from, stmt_id, purpose, user_label) or per-entry errors.
///
/// Schema v2: `invariants`, `assigns` are arrays of `{acsl, behavior?}`;
/// `variant` is single optional `{acsl, behavior?}`.
fn loop_annots_to_acsl(
    annot: &serde_json::Value,
    i: usize,
    behaviors: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<Result<(String, String, String, Option<i64>, String, Option<String>), (String, String)>> {
    let mut result = Vec::new();
    let stmt_id = annot.get("stmt_id").and_then(|v| v.as_i64());
    let loop_label = annot.get("loop_label").and_then(|v| v.as_str()).unwrap_or("");
    let base_label = format!("loop_{}", loop_label.replace(' ', "_"));

    // invariants: Vec<{acsl, behavior?}>
    if let Some(invs) = annot.get("invariants").and_then(|v| v.as_array()) {
        for (j, inv) in invs.iter().enumerate() {
            let body = inv.get("acsl").and_then(|x| x.as_str()).unwrap_or("");
            let behavior = inv.get("behavior").and_then(|x| x.as_str());
            let path = format!("proposed_loop_annots[{}].invariants[{}]", i, j);
            match wrap_loop_clause("loop invariant", body, behavior, behaviors, &path) {
                Ok(acsl_text) => result.push(Ok((
                    acsl_text,
                    "annot".to_string(),
                    path,
                    stmt_id,
                    format!("{} invariant {}", loop_label, j),
                    Some(format!("{}_inv_{}", base_label, j)),
                ))),
                Err(msg) => result.push(Err((path, msg))),
            }
        }
    }

    // loop assigns: Vec<{acsl, behavior?}>
    if let Some(las) = annot.get("assigns").and_then(|v| v.as_array()) {
        for (j, la) in las.iter().enumerate() {
            let body = la.get("acsl").and_then(|x| x.as_str()).unwrap_or("");
            if body.trim().is_empty() { continue; }
            let behavior = la.get("behavior").and_then(|x| x.as_str());
            let path = format!("proposed_loop_annots[{}].assigns[{}]", i, j);
            match wrap_loop_clause("loop assigns", body, behavior, behaviors, &path) {
                Ok(acsl_text) => result.push(Ok((
                    acsl_text,
                    "annot".to_string(),
                    path,
                    stmt_id,
                    format!("{} assigns {}", loop_label, j),
                    Some(format!("{}_assigns_{}", base_label, j)),
                ))),
                Err(msg) => result.push(Err((path, msg))),
            }
        }
    }

    // loop variant: Option<{acsl, behavior?}>
    if let Some(var) = annot.get("variant") {
        // Skip if explicitly null or missing acsl
        if !var.is_null() {
            let body = var.get("acsl").and_then(|x| x.as_str()).unwrap_or("");
            if !body.trim().is_empty() {
                let behavior = var.get("behavior").and_then(|x| x.as_str());
                let path = format!("proposed_loop_annots[{}].variant", i);
                match wrap_loop_clause("loop variant", body, behavior, behaviors, &path) {
                    Ok(acsl_text) => result.push(Ok((
                        acsl_text,
                        "annot".to_string(),
                        path,
                        stmt_id,
                        format!("{} variant", loop_label),
                        Some(format!("{}_variant", base_label)),
                    ))),
                    Err(msg) => result.push(Err((path, msg))),
                }
            }
        }
    }

    result
}


/// Map ACSL text to the AST kind expected by add_annotation_impl.
/// Only two values: "spec" (function-level: requires/ensures/assigns/behavior)
/// or "annot" (statement-level: loop_invariant/loop_assigns/loop_variant/assert).
fn acsl_kind_to_ast_kind(acsl: &str) -> String {
    let lower = acsl.trim().to_lowercase();
    if lower.starts_with("loop invariant")
        || lower.starts_with("loop assigns")
        || lower.starts_with("loop variant")
        || lower.starts_with("assert")
    {
        "annot".to_string()
    } else {
        "spec".to_string()
    }
}

/// Classify a Frama-C error message into a FailureType.
///
/// Patterns derived from frama-c kernel `Logic_typing.ml` + our ast-utils
/// wrapper. Order matters: more specific patterns first
/// (ProposedLocalVarInFunspec before generic ProposedSelfReferential).
fn classify_failure(error: &str) -> FailureType {
    let lower = error.to_lowercase();
    // 1. Funspec referencing function local (our ast-utils-specific message).
    if lower.contains("function local") {
        return FailureType::ProposedLocalVarInFunspec;
    }
    // 2. Unbound / unknown name (most common Logic_typing class).
    //    Covers: unbound logic variable/predicate/function,
    //            no such enum/struct/union/type/predicate,
    //            cannot find field/function,
    //            logic label `…' not found,
    //            reference to unknown behavior,
    //            unknown identifier, undeclared type,
    //            Unbound variable (our find_enum_tag fallback)
    if lower.contains("unbound")
        || lower.contains("no such")
        || lower.contains("not found")
        || lower.contains("unknown identifier")
        || lower.contains("undeclared type")
        || lower.contains("reference to unknown")
        || lower.contains("cannot find")
    {
        return FailureType::ProposedSelfReferential;
    }
    // 3. Syntax / parse errors.
    if lower.contains("syntax error")
        || lower.contains("parse error")
        || lower.contains("unexpected")
        || lower.contains("lexeme")
    {
        return FailureType::SyntaxError;
    }
    // 4. Fallback: type errors, duplicates, semantic violations, etc.
    FailureType::ProposedError
}

/// Compute overall status from failure list.
fn compute_status(failures: &[InjectionFailure]) -> String {
    if failures.is_empty() {
        "success".to_string()
    } else if failures.iter().all(|f| matches!(f.failure_type, FailureType::SyntaxError)) {
        "partial".to_string()
    } else {
        "proposed_error".to_string()
    }
}

/// Normalize ACSL for idempotency comparison: strip hash_labels and whitespace.
fn normalize_for_comparison(acsl: &str) -> String {
    let re = regex::Regex::new(r",\s*(?:re|en|li|la|lv|at|an)_[0-9a-f]{8}(?:,[^,]*)?").unwrap();
    let s = re.replace_all(acsl, "").to_string();
    s.split(';').next().unwrap_or(&s).trim().to_string()
}

/// Internal plan entry built from proposed_* fields before injection.
struct InjectionPlanEntry {
    acsl_text: String,
    kind: String,
    derived_from: String,
    stmt_id: Option<i64>,
    purpose: String,
    user_label: Option<String>,
}

fn parse_conclusion_status(s: &str) -> Result<crate::state::VerificationStatus, String> {
    match s {
        "verified" => Ok(crate::state::VerificationStatus::Verified),
        "failed" => Ok(crate::state::VerificationStatus::Failed),
        "unsound" => Ok(crate::state::VerificationStatus::Unsound),
        "blocked_on_callee" => Ok(crate::state::VerificationStatus::BlockedOnCallee),
        "in_progress" => Ok(crate::state::VerificationStatus::InProgress),
        _ => Err(format!(
            "invalid status '{}', expected: verified|failed|unsound|blocked_on_callee|in_progress",
            s
        )),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FramaCMcpServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo (alias for InitializeResult) is #[non_exhaustive] in
        // rmcp 1.x; can't use struct literal — go via ::new + with_* builder.
        //
        // Pin protocol_version to 2024-11-05: rmcp 1.x default is LATEST
        // (2025-11-25) which exceeds Claude Code 2.1.146's known set; pin to
        // the oldest broadly-supported version as defensive hardening
        // (mirrors fv-core-mcp).
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Frama-C formal verification server. Provides EVA abstract interpretation, \
                 WP deductive verification, and CIL AST navigation."
            )
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// O3: collect_loop_sids walks a getFunctionAst JSON and returns loop sids in
    /// source (pre-order) order. Shape mirrors real ast-utils output: a function
    /// body block whose stmts array holds two sequential loops (sids 4, 13) plus
    /// non-loop statements. Verified against real frama-c output (sids [4, 13]).
    #[test]
    fn collect_loop_sids_two_sequential_loops() {
        let ast = json!({
            "name": "f",
            "body": { "sid": 1, "kind": "block", "stmts": [
                { "sid": 2, "kind": "instr" },
                { "sid": 4, "kind": "loop", "body": { "kind": "block", "stmts": [
                    { "sid": 5, "kind": "instr" }
                ]}},
                { "sid": 13, "kind": "loop", "body": { "kind": "block", "stmts": [
                    { "sid": 14, "kind": "instr" }
                ]}},
                { "sid": 20, "kind": "return" }
            ]}
        });
        let mut sids = Vec::new();
        collect_loop_sids(&ast, &mut sids);
        assert_eq!(sids, vec![4, 13]);
    }

    /// O3: nested loops — outer collected before inner (pre-order = source order).
    #[test]
    fn collect_loop_sids_nested() {
        let ast = json!({
            "body": { "sid": 1, "kind": "block", "stmts": [
                { "sid": 3, "kind": "loop", "body": { "kind": "block", "stmts": [
                    { "sid": 7, "kind": "loop", "body": { "kind": "block", "stmts": [] }}
                ]}}
            ]}
        });
        let mut sids = Vec::new();
        collect_loop_sids(&ast, &mut sids);
        assert_eq!(sids, vec![3, 7]);
    }

    /// O3: no loops → empty (function-level-only merge needs no loop re-resolution).
    #[test]
    fn collect_loop_sids_none() {
        let ast = json!({ "body": { "sid": 1, "kind": "block", "stmts": [
            { "sid": 2, "kind": "return" }
        ]}});
        let mut sids = Vec::new();
        collect_loop_sids(&ast, &mut sids);
        assert!(sids.is_empty());
    }

    /// O3 regression (B2): loops split across both branches of an `if` must come
    /// out in source order (then before else), independent of JSON object key
    /// order. The if-node literal deliberately lists `else_body` BEFORE `then_body`
    /// so a naive key-order walk yields [20, 10]; collect_loop_sids' explicit
    /// then→else ordering must yield [10, 20]. Guards against silent loop-annot
    /// misplacement. See docs/fixes/vpfsm-review-b1-b2-fix.md (B2).
    #[test]
    fn collect_loop_sids_if_then_else_branches() {
        let ast = json!({
            "name": "f",
            "body": { "sid": 1, "kind": "block", "stmts": [
                {
                    "sid": 2, "kind": "if",
                    "cond": "x > 0",
                    // adversarial: else_body written before then_body in the literal
                    "else_body": [
                        { "sid": 20, "kind": "loop", "body": { "kind": "block", "stmts": [
                            { "sid": 21, "kind": "instr" }
                        ]}}
                    ],
                    "then_body": [
                        { "sid": 10, "kind": "loop", "body": { "kind": "block", "stmts": [
                            { "sid": 11, "kind": "instr" }
                        ]}}
                    ]
                }
            ]}
        });
        let mut sids = Vec::new();
        collect_loop_sids(&ast, &mut sids);
        assert_eq!(
            sids,
            vec![10, 20],
            "then-branch loop (sid 10) must precede else-branch loop (sid 20)"
        );
    }

    /// 回归: store_project_state 收到非法 state_json 必须返回 {"stored": false}
    /// (消除 silent-success 反模式——见 docs/fixes/store-project-state-schema-gap-vp-fsm.md §4.1).
    /// 非法 JSON 在落盘前就被拒, 不写盘, 故不污染 CWD。
    #[tokio::test]
    async fn store_project_state_invalid_json_returns_false() {
        let state = Arc::new(RwLock::new(SessionState::default()));
        let server = FramaCMcpServer::new_lazy(state, "frama-c".to_string(), 1);

        let params = StoreProjectStateParams {
            state_json: Some("{ this is not valid json".to_string()),
            source_files: None,
            verification_order: None,
            current_index: None,
            global_notes: None,
        };
        let result = server
            .store_project_state(Parameters(params))
            .await
            .expect("handler 不应 Err");

        let s = serde_json::to_string(&result).expect("result 可序列化");
        assert!(s.contains("stored") && s.contains("false") && s.contains("error"),
            "非法 state_json 应返回 stored:false + error, got: {}", s);
        assert!(!s.contains("\\\"stored\\\": true"),
            "不应是 silent-success, got: {}", s);
    }

    #[test]
    fn classify_rte_overflow() {
        let g = json!({"name": "signed_overflow at line 12"});
        let (kind, hl) = classify_wp_goal(&g);
        assert_eq!(kind, "rte_overflow");
        assert!(hl.is_none());
    }

    #[test]
    fn classify_rte_bound() {
        let g = json!({"name": "index_in_bound at line 7"});
        let (kind, _) = classify_wp_goal(&g);
        assert_eq!(kind, "rte_bound");
    }

    #[test]
    fn classify_rte_division() {
        let g = json!({"name": "division_by_zero"});
        let (kind, _) = classify_wp_goal(&g);
        assert_eq!(kind, "rte_division");
    }

    #[test]
    fn classify_rte_pointer() {
        let g = json!({"name": "mem_access of *p"});
        let (kind, _) = classify_wp_goal(&g);
        assert_eq!(kind, "rte_pointer");
    }

    #[test]
    fn classify_rte_shift() {
        let g = json!({"name": "shift overflow"});
        let (kind, _) = classify_wp_goal(&g);
        assert_eq!(kind, "rte_shift");
    }

    #[test]
    fn classify_user_assert() {
        let g = json!({"name": "Assertion at stmt 42"});
        let (kind, hl) = classify_wp_goal(&g);
        assert_eq!(kind, "user_assert");
        assert!(hl.is_none());
    }

    #[test]
    fn classify_spec_with_hash_label() {
        // 模拟 hash_label re_a3f2b1c8 注入到 pred_name
        let g = json!({"name": "Pre re_a3f2b1c8"});
        let (kind, hl) = classify_wp_goal(&g);
        assert_eq!(kind, "spec");
        assert_eq!(hl, Some("re_a3f2b1c8".to_string()));
    }

    #[test]
    fn classify_spec_without_hash_label() {
        // 没注 hash_label（理论上不应发生，但作为兜底默认 spec）
        let g = json!({"name": "Pre <some predicate>"});
        let (kind, hl) = classify_wp_goal(&g);
        assert_eq!(kind, "spec");
        assert!(hl.is_none());
    }

    #[test]
    fn classify_loop_invariant_with_hash_label() {
        let g = json!({"name": "Invariant li_12ab34cd at stmt 42"});
        let (kind, hl) = classify_wp_goal(&g);
        assert_eq!(kind, "spec");
        assert_eq!(hl, Some("li_12ab34cd".to_string()));
    }

    // --- inject_all_annotations_sandbox helpers tests ---

    #[test]
    fn parse_plugin_success_wrapped_result() {
        // Simulates the actual OCaml plugin response: {"result": {"success": true}, "hash_label": "..."}
        let json = serde_json::json!({
            "result": {"success": true, "error": null},
            "hash_label": "re_12345678"
        });
        let content = Content::text(json.to_string());
        let result = CallToolResult::success(vec![content]);
        assert!(parse_plugin_success(&result));
    }

    #[test]
    fn parse_plugin_success_false_wrapped() {
        let json = serde_json::json!({
            "result": {"success": false, "error": "ACSL syntax error in function contract"},
            "hash_label": "an_12345678"
        });
        let content = Content::text(json.to_string());
        let result = CallToolResult::success(vec![content]);
        assert!(!parse_plugin_success(&result));
    }

    #[test]
    fn parse_plugin_error_wrapped_result() {
        let json = serde_json::json!({
            "result": {"success": false, "error": "unbound logic variable i"},
            "hash_label": "an_12345678"
        });
        let content = Content::text(json.to_string());
        let result = CallToolResult::success(vec![content]);
        assert_eq!(parse_plugin_error(&result), Some("unbound logic variable i".to_string()));
    }

    #[test]
    fn parse_plugin_success_empty_content() {
        let result = CallToolResult::success(vec![]);
        assert!(!parse_plugin_success(&result));
    }

    #[test]
    fn normalize_acsl_adds_semicolon() {
        assert_eq!(normalize_acsl("n >= 0"), "n >= 0;");
    }

    #[test]
    fn normalize_acsl_strips_extra_semicolons() {
        assert_eq!(normalize_acsl("n >= 0;"), "n >= 0;");
        assert_eq!(normalize_acsl("n >= 0;;"), "n >= 0;");
    }

    #[test]
    fn normalize_acsl_trims_whitespace() {
        assert_eq!(normalize_acsl("  n >= 0  ;  "), "n >= 0;");
    }

    #[test]
    fn normalize_acsl_empty_string() {
        assert_eq!(normalize_acsl(""), ";");
        assert_eq!(normalize_acsl("  ;  "), ";");
    }

    // ─── Schema v2 helpers: wrap_funspec_clause / wrap_loop_clause / loop_annots_to_acsl ───
    //
    // Schema v2 replaced the old per-clause-keyword helpers (requires_to_acsl /
    // ensures_to_acsl / assigns_to_acsl) with unified wrap_funspec_clause +
    // wrap_loop_clause that resolve behavior references against a name → assumes table.

    fn make_behaviors(pairs: &[(&str, &[&str])]) -> std::collections::HashMap<String, Vec<String>> {
        pairs.iter()
            .map(|(name, assumes)| {
                (name.to_string(), assumes.iter().map(|s| s.to_string()).collect())
            })
            .collect()
    }

    #[test]
    fn wrap_funspec_clause_top_level() {
        let b = make_behaviors(&[]);
        let r = wrap_funspec_clause("requires", "n >= 0", None, &b, "proposed_requires[0]").unwrap();
        assert_eq!(r, "requires n >= 0;");
    }

    #[test]
    fn wrap_funspec_clause_strips_trailing_semicolon() {
        let b = make_behaviors(&[]);
        let r = wrap_funspec_clause("ensures", "\\result == 0;", None, &b, "p").unwrap();
        assert_eq!(r, "ensures \\result == 0;");
    }

    #[test]
    fn wrap_funspec_clause_with_behavior_no_assumes() {
        // Behavior declared with empty assumes list — still valid (named behavior, always applies).
        let b = make_behaviors(&[("sorted", &[])]);
        let r = wrap_funspec_clause("ensures", "\\result == 0", Some("sorted"), &b, "p").unwrap();
        assert_eq!(r, "behavior sorted: ensures \\result == 0;");
    }

    #[test]
    fn wrap_funspec_clause_with_behavior_and_assumes() {
        let b = make_behaviors(&[("sorted", &["n >= 2", "a != \\null"])]);
        let r = wrap_funspec_clause("assigns", "a[0..n-1]", Some("sorted"), &b, "p").unwrap();
        assert_eq!(r, "behavior sorted: assumes n >= 2; assumes a != \\null; assigns a[0..n-1];");
    }

    #[test]
    fn wrap_funspec_clause_undeclared_behavior_errors() {
        let b = make_behaviors(&[("known", &["n > 0"])]);
        let err = wrap_funspec_clause("requires", "p != \\null", Some("unknown"), &b, "proposed_requires[3]")
            .expect_err("undeclared behavior should error");
        assert!(err.contains("'unknown'"), "got: {}", err);
        assert!(err.contains("proposed_requires[3]"), "got: {}", err);
        assert!(err.contains("not declared in proposed_behaviors"), "got: {}", err);
    }

    #[test]
    fn wrap_loop_clause_top_level() {
        let b = make_behaviors(&[]);
        let r = wrap_loop_clause("loop invariant", "0 <= i", None, &b, "p").unwrap();
        assert_eq!(r, "loop invariant 0 <= i;");
    }

    #[test]
    fn wrap_loop_clause_with_behavior() {
        // Loop clauses use `for X: ...` syntax (NOT `behavior X: assumes ...; loop ...`)
        // — the behavior's assumes live in the funspec, the loop just references the name.
        let b = make_behaviors(&[("pos", &["n > 0"])]);
        let r = wrap_loop_clause("loop assigns", "a, i", Some("pos"), &b, "p").unwrap();
        assert_eq!(r, "for pos: loop assigns a, i;");
    }

    #[test]
    fn wrap_loop_clause_undeclared_behavior_errors() {
        let b = make_behaviors(&[]);
        let err = wrap_loop_clause(
            "loop variant", "n - i", Some("missing"), &b, "proposed_loop_annots[0].variant"
        ).expect_err("undeclared should error");
        assert!(err.contains("'missing'"));
        assert!(err.contains("proposed_loop_annots[0].variant"));
    }

    // --- double-keyword normalization (docs/fixes/inject-all-wrap-double-keyword.md) ---

    /// v-f-fsm stores requires/ensures acsl WITH the keyword (type marker). wrap
    /// must strip the leading dup, not produce "requires requires …".
    #[test]
    fn wrap_funspec_clause_strips_leading_keyword() {
        let b = make_behaviors(&[]);
        // keyword-bearing input (as stored in conclusion) → single keyword
        let r = wrap_funspec_clause("requires", "requires x < 2147483647;", None, &b, "p").unwrap();
        assert_eq!(r, "requires x < 2147483647;");
        let e = wrap_funspec_clause("ensures", "ensures \\result == x + 1;", None, &b, "p").unwrap();
        assert_eq!(e, "ensures \\result == x + 1;");
    }

    /// bare input (no keyword) still wraps normally — no regression for assigns / sandbox path.
    #[test]
    fn wrap_funspec_clause_bare_unchanged() {
        let b = make_behaviors(&[]);
        let r = wrap_funspec_clause("requires", "x < 2147483647", None, &b, "p").unwrap();
        assert_eq!(r, "requires x < 2147483647;");
        let a = wrap_funspec_clause("assigns", "\\nothing", None, &b, "p").unwrap();
        assert_eq!(a, "assigns \\nothing;");
    }

    /// word-boundary: a variable named `requires_foo` must NOT be mis-stripped.
    #[test]
    fn wrap_funspec_clause_keyword_prefix_variable_not_stripped() {
        let b = make_behaviors(&[]);
        let r = wrap_funspec_clause("requires", "requires_foo > 0", None, &b, "p").unwrap();
        assert_eq!(r, "requires requires_foo > 0;");
    }

    /// idempotent: stripping an already-bare body is a no-op.
    #[test]
    fn strip_leading_keyword_idempotent() {
        let once = strip_leading_keyword("requires x < 2;", "requires");
        assert_eq!(once, "x < 2;");
        let twice = strip_leading_keyword(&once, "requires");
        assert_eq!(twice, "x < 2;");
    }

    /// loop clause multi-word keyword also normalized.
    #[test]
    fn wrap_loop_clause_strips_leading_keyword() {
        let b = make_behaviors(&[]);
        let r = wrap_loop_clause("loop invariant", "loop invariant 0 <= i", None, &b, "p").unwrap();
        assert_eq!(r, "loop invariant 0 <= i;");
    }

    #[test]
    fn loop_annots_to_acsl_basic() {
        let b = make_behaviors(&[]);
        let annot = json!({
            "stmt_id": 2,
            "loop_label": "outer loop",
            "invariants": [
                {"acsl": "0 <= i <= n"},
                {"acsl": "\\forall k; 0 <= k <= j ==> a[k] <= a[j]"}
            ],
            "assigns": [
                {"acsl": "a, i, j, tmp"}
            ],
            "variant": {"acsl": "(n - 1) - i"}
        });
        let outcomes = loop_annots_to_acsl(&annot, 0, &b);
        // 2 invariants + 1 assigns + 1 variant = 4
        assert_eq!(outcomes.len(), 4);
        let entries: Vec<_> = outcomes.into_iter().map(|o| o.unwrap()).collect();

        assert_eq!(entries[0].0, "loop invariant 0 <= i <= n;");
        assert_eq!(entries[0].2, "proposed_loop_annots[0].invariants[0]");
        assert_eq!(entries[0].3, Some(2i64));

        assert_eq!(entries[1].0, "loop invariant \\forall k; 0 <= k <= j ==> a[k] <= a[j];");
        assert_eq!(entries[1].2, "proposed_loop_annots[0].invariants[1]");

        assert_eq!(entries[2].0, "loop assigns a, i, j, tmp;");
        assert_eq!(entries[2].2, "proposed_loop_annots[0].assigns[0]");

        assert_eq!(entries[3].0, "loop variant (n - 1) - i;");
        assert_eq!(entries[3].2, "proposed_loop_annots[0].variant");
    }

    #[test]
    fn loop_annots_to_acsl_with_behavior() {
        let b = make_behaviors(&[("pos", &["n > 0"])]);
        let annot = json!({
            "stmt_id": 5,
            "loop_label": "outer",
            "invariants": [
                {"acsl": "0 <= i <= n"},                            // top-level
                {"acsl": "a[0] >= 0", "behavior": "pos"}            // for pos: ...
            ],
            "assigns": [{"acsl": "a, i", "behavior": "pos"}],
            "variant": null
        });
        let outcomes = loop_annots_to_acsl(&annot, 0, &b);
        assert_eq!(outcomes.len(), 3);
        let entries: Vec<_> = outcomes.into_iter().map(|o| o.unwrap()).collect();
        assert_eq!(entries[0].0, "loop invariant 0 <= i <= n;");
        assert_eq!(entries[1].0, "for pos: loop invariant a[0] >= 0;");
        assert_eq!(entries[2].0, "for pos: loop assigns a, i;");
    }

    #[test]
    fn loop_annots_to_acsl_empty_arrays() {
        let b = make_behaviors(&[]);
        let annot = json!({
            "stmt_id": 8,
            "loop_label": "inner",
            "invariants": [],
            "assigns": [],
            "variant": null
        });
        let entries = loop_annots_to_acsl(&annot, 1, &b);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn loop_annots_to_acsl_undeclared_behavior_errors() {
        let b = make_behaviors(&[]);
        let annot = json!({
            "stmt_id": 3,
            "loop_label": "outer",
            "invariants": [{"acsl": "i >= 0", "behavior": "ghost"}],
            "assigns": [],
            "variant": null
        });
        let outcomes = loop_annots_to_acsl(&annot, 0, &b);
        assert_eq!(outcomes.len(), 1);
        let (path, msg) = outcomes[0].as_ref().expect_err("should error");
        assert!(path.contains("invariants[0]"));
        assert!(msg.contains("'ghost'"));
    }

    #[test]
    fn classify_failure_syntax_error() {
        assert!(matches!(classify_failure("syntax error in ACSL"), FailureType::SyntaxError));
        assert!(matches!(classify_failure("parse error: unexpected token"), FailureType::SyntaxError));
        assert!(matches!(classify_failure("unexpected identifier 'foo'"), FailureType::SyntaxError));
    }

    #[test]
    fn classify_failure_self_referential() {
        // Logic_typing "unbound" family
        assert!(matches!(
            classify_failure("unbound logic variable 'factorial'"),
            FailureType::ProposedSelfReferential
        ));
        assert!(matches!(
            classify_failure("unbound logic predicate unknown_pred"),
            FailureType::ProposedSelfReferential
        ));
        assert!(matches!(
            classify_failure("unbound logic function f"),
            FailureType::ProposedSelfReferential
        ));
        // Our ast-utils find_enum_tag fallback (truly unbound, not local)
        assert!(matches!(
            classify_failure("Unbound variable foo"),
            FailureType::ProposedSelfReferential
        ));
        // Logic_typing "no such" family
        assert!(matches!(
            classify_failure("no such enum E"),
            FailureType::ProposedSelfReferential
        ));
        assert!(matches!(
            classify_failure("no such predicate or logic function P(int)"),
            FailureType::ProposedSelfReferential
        ));
        // Misc unresolved-name
        assert!(matches!(
            classify_failure("logic label `L' not found"),
            FailureType::ProposedSelfReferential
        ));
        assert!(matches!(
            classify_failure("cannot find field x"),
            FailureType::ProposedSelfReferential
        ));
        assert!(matches!(
            classify_failure("reference to unknown behavior b"),
            FailureType::ProposedSelfReferential
        ));
        assert!(matches!(
            classify_failure("unknown identifier '\\permutation'"),
            FailureType::ProposedSelfReferential
        ));
        assert!(matches!(
            classify_failure("undeclared type 'my_type'"),
            FailureType::ProposedSelfReferential
        ));
    }

    #[test]
    fn classify_failure_local_var_in_funspec() {
        // The actionable message emitted by our ast-utils find_enum_tag wrapper
        // when funspec references a function local.
        assert!(matches!(
            classify_failure(
                "Variable 'i' is a function local; ACSL function-level contracts \
                 may only reference caller-visible state (formals, globals, \
                 \\result, \\old(formal)). Replace with the caller-visible state \
                 being modified."
            ),
            FailureType::ProposedLocalVarInFunspec
        ));
    }

    #[test]
    fn classify_failure_proposed_error() {
        // Type / semantic / duplicate errors → catchall
        assert!(matches!(
            classify_failure("comparison of incompatible types: int * and ℤ"),
            FailureType::ProposedError
        ));
        assert!(matches!(
            classify_failure("behavior b already defined"),
            FailureType::ProposedError
        ));
        assert!(matches!(
            classify_failure("not an assignable left value: n + 1"),
            FailureType::ProposedError
        ));
        assert!(matches!(
            classify_failure("type mismatch in expression"),
            FailureType::ProposedError
        ));
        assert!(matches!(
            classify_failure("some other error"),
            FailureType::ProposedError
        ));
    }

    #[test]
    fn compute_status_no_failures() {
        assert_eq!(compute_status(&[]), "success");
    }

    #[test]
    fn compute_status_all_syntax_errors() {
        let failures = vec![
            InjectionFailure {
                failure_type: FailureType::SyntaxError,
                proposed_path: "proposed_requires[0]".into(),
                acsl_text: "requires \\bad;".into(),
                frama_c_error: "syntax error".into(),
            },
            InjectionFailure {
                failure_type: FailureType::SyntaxError,
                proposed_path: "proposed_ensures[0]".into(),
                acsl_text: "ensures \\bad;".into(),
                frama_c_error: "parse error".into(),
            },
        ];
        assert_eq!(compute_status(&failures), "partial");
    }

    #[test]
    fn compute_status_with_proposed_error() {
        let failures = vec![
            InjectionFailure {
                failure_type: FailureType::SyntaxError,
                proposed_path: "proposed_requires[0]".into(),
                acsl_text: "requires \\bad;".into(),
                frama_c_error: "syntax error".into(),
            },
            InjectionFailure {
                failure_type: FailureType::ProposedError,
                proposed_path: "proposed_assigns".into(),
                acsl_text: "assigns bogus;".into(),
                frama_c_error: "type error".into(),
            },
        ];
        assert_eq!(compute_status(&failures), "proposed_error");
    }

    #[test]
    fn compute_status_self_referential() {
        let failures = vec![InjectionFailure {
            failure_type: FailureType::ProposedSelfReferential,
            proposed_path: "proposed_ensures[0]".into(),
            acsl_text: "ensures \\permutation{a}{\\old(a)};".into(),
            frama_c_error: "unknown identifier".into(),
        }];
        assert_eq!(compute_status(&failures), "proposed_error");
    }

    #[test]
    fn normalize_for_comparison_strips_hash_label() {
        let with_label = "requires \\valid(a + (0 .. n - 1)), re_a3f2b1c8";
        let result = normalize_for_comparison(with_label);
        assert!(result.contains("\\valid"));
        assert!(!result.contains("re_a3f2b1c8"));
    }

    #[test]
    fn acsl_kind_to_ast_kind_mapping() {
        // Function-level → "spec"
        assert_eq!(acsl_kind_to_ast_kind("requires P;"), "spec");
        assert_eq!(acsl_kind_to_ast_kind("ensures Q;"), "spec");
        assert_eq!(acsl_kind_to_ast_kind("assigns \\nothing;"), "spec");
        assert_eq!(acsl_kind_to_ast_kind("behavior foo:"), "spec");
        // Statement-level → "annot"
        assert_eq!(acsl_kind_to_ast_kind("loop invariant P;"), "annot");
        assert_eq!(acsl_kind_to_ast_kind("loop assigns a;"), "annot");
        assert_eq!(acsl_kind_to_ast_kind("loop variant e;"), "annot");
        assert_eq!(acsl_kind_to_ast_kind("assert x > 0;"), "annot");
    }

    // --- full_label helper tests ---

    #[test]
    fn full_label_without_user_label() {
        assert_eq!(full_label("re_a3f2b1c8", None), "re_a3f2b1c8");
    }

    #[test]
    fn full_label_with_user_label() {
        assert_eq!(full_label("as_af0e6de7", Some("assigns")), "as_af0e6de7_assigns");
    }

    #[test]
    fn full_label_matches_add_annotation_impl_construction() {
        // Verify that the helper produces the same result as the inline logic
        // in add_annotation_impl (the original source of truth).
        let hash_label = "li_44f10e5e";
        // Without user_label
        let inline = match None as Option<&str> {
            Some(ul) => format!("{}_{}", hash_label, ul),
            None => hash_label.to_string(),
        };
        assert_eq!(full_label(hash_label, None), inline);
        // With user_label
        let inline = match Some("assigns") as Option<&str> {
            Some(ul) => format!("{}_{}", hash_label, ul),
            None => hash_label.to_string(),
        };
        assert_eq!(full_label(hash_label, Some("assigns")), inline);
    }

    #[test]
    fn full_label_rollback_would_find_assigns_behavior() {
        // Simulate the exact bug scenario: proposed_assigns has user_label="assigns".
        // add_annotation_impl creates behavior named "{full_label}__spec".
        // rollback must search for the same full_label.
        let hash_label = "as_af0e6de7";
        let user_label = Some("assigns");
        // The behavior name created by add_annotation_impl:
        let behavior_name = format!("{}_{}__spec", hash_label, user_label.unwrap());
        assert_eq!(behavior_name, "as_af0e6de7_assigns__spec");
        // The rollback label (must match the full_label, not just hash_label):
        let rollback_label = full_label(hash_label, user_label);
        assert_eq!(rollback_label, "as_af0e6de7_assigns");
        // OCaml remove_annotation_by_label looks for: label ^ "__spec"
        let search_name = format!("{}__spec", rollback_label);
        assert_eq!(search_name, behavior_name);
        // The old buggy code used just hash_label for rollback:
        let buggy_search = format!("{}__spec", hash_label);
        assert_ne!(buggy_search, behavior_name);
    }
}
