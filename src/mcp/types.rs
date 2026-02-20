use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReloadProjectParams {
    /// C source file paths to reload. If omitted, reloads currently loaded files.
    pub files: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFunctionInfoParams {
    /// Function name to query
    pub function_name: String,
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
    pub functions: Option<Vec<String>>,
    /// SMT prover name: "Alt-Ergo", "Why3:Z3", "Why3:CVC5" (default: current setting)
    pub prover: Option<String>,
    /// Prover timeout in seconds (default: current setting)
    pub timeout: Option<u32>,
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
