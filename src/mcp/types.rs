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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetEvaValueParams {
    /// Statement or expression marker (e.g. "#s2")
    pub marker: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunWpParams {
    /// Function name to verify with WP
    pub function_name: String,
    /// SMT prover name: "Alt-Ergo", "Why3:Z3", "Why3:CVC5" (default: current setting)
    pub prover: Option<String>,
    /// Prover timeout in seconds (default: current setting)
    pub timeout: Option<u32>,
}
