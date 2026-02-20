use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde_json::json;

use crate::error::FramaCError;
use crate::frama_c::client::FramaCClient;
use crate::mcp::types::*;
use crate::state::SessionState;

#[derive(Clone)]
pub struct FramaCMcpServer {
    client: Arc<FramaCClient>,
    state: Arc<RwLock<SessionState>>,
    tool_router: ToolRouter<Self>,
}

impl FramaCMcpServer {
    pub fn new(client: FramaCClient, state: Arc<RwLock<SessionState>>) -> Self {
        Self {
            client: Arc::new(client),
            state,
            tool_router: Self::tool_router(),
        }
    }

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
        self.client
            .get("kernel.ast.reloadFunctions", json!(null))
            .await
            .map_err(McpError::from)?;
        let entries = self
            .client
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
        self.client
            .get("kernel.ast.reloadGlobals", json!(null))
            .await
            .map_err(McpError::from)?;
        let entries = self
            .client
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
            self.client
                .exec(
                    "plugins.callgraph.compute",
                    json!(null),
                    Duration::from_secs(60),
                )
                .await
                .map_err(McpError::from)?;
            let graph = self
                .client
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
        description = "Reload C source files after modification. Reparses AST and refreshes all cached state. EVA/WP results are invalidated."
    )]
    async fn reload_project(
        &self,
        Parameters(params): Parameters<ReloadProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(files) = params.files {
            self.client
                .set("kernel.ast.setFiles", json!(files))
                .await
                .map_err(McpError::from)?;
        }
        self.client
            .exec("kernel.ast.compute", json!(null), Duration::from_secs(120))
            .await
            .map_err(McpError::from)?;
        // Reload to reset incremental cursor (defensive: compute may not
        // always trigger server-side signal propagation in edge cases)
        self.client
            .get("kernel.ast.reloadFunctions", json!(null))
            .await
            .map_err(McpError::from)?;
        let entries = self
            .client
            .fetch_all("kernel.ast.fetchFunctions")
            .await
            .map_err(McpError::from)?;
        let files_list = self
            .client
            .get("kernel.ast.getFiles", json!(null))
            .await
            .map_err(McpError::from)?;

        {
            let mut state = self.state.write().await;
            state.invalidate_all();
            state.update_functions(&entries);
            state.project_loaded = true;
        }

        let result = json!({
            "functions": entries,
            "files": files_list,
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
        let info = self
            .resolve_function_or_refresh(&params.function_name)
            .await?;

        let decl_text = self
            .client
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
        self.client
            .exec(
                "plugins.callgraph.compute",
                json!(null),
                Duration::from_secs(60),
            )
            .await
            .map_err(McpError::from)?;
        let graph = self
            .client
            .get("plugins.callgraph.getCallgraph", json!(null))
            .await
            .map_err(McpError::from)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&graph).unwrap_or_default(),
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
            self.client
                .set("kernel.parameters.setEvaPrecision", json!(precision))
                .await
                .map_err(McpError::from)?;
        }
        if let Some(ref main_fn) = params.main_function {
            self.client
                .set("kernel.parameters.setMain", json!(main_fn))
                .await
                .map_err(McpError::from)?;
        }
        if let Some(slevel) = params.slevel {
            self.client
                .set("kernel.parameters.setEvaSlevel", json!(slevel))
                .await
                .map_err(McpError::from)?;
        }

        self.client
            .exec(
                "plugins.eva.general.compute",
                json!(null),
                Duration::from_secs(600),
            )
            .await
            .map_err(McpError::from)?;
        let comp_state = self
            .client
            .get("plugins.eva.general.getComputationState", json!(null))
            .await
            .map_err(McpError::from)?;
        let stats = self
            .client
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
        self.client
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let properties = self
            .client
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
        let values = self
            .client
            .get("plugins.eva.values.getValues", request_data)
            .await
            .map_err(McpError::from)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&values).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Run WP deductive verification. Specify function names to verify, or omit to verify all functions. Returns proof task statistics. This may take several minutes."
    )]
    async fn run_wp(
        &self,
        Parameters(params): Parameters<RunWpParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(prover) = params.prover {
            self.client
                .set("plugins.wp.setProvers", json!([prover]))
                .await
                .map_err(McpError::from)?;
        }
        if let Some(timeout) = params.timeout {
            self.client
                .set("plugins.wp.setTimeout", json!(timeout))
                .await
                .map_err(McpError::from)?;
        }

        // P2.9: Resolve target functions
        let targets = match params.functions {
            Some(names) => {
                let mut infos = Vec::new();
                for name in &names {
                    infos.push(self.resolve_function_or_refresh(name).await?);
                }
                infos
            }
            None => {
                // Verify all functions from cache
                let state = self.state.read().await;
                state.functions.values().cloned().collect()
            }
        };

        for info in &targets {
            let decl_marker = &info.declaration;

            // printDeclaration indexes markers in the server's marker table.
            // Without this call, PVDecl markers (needed by startProofs) are not
            // registered and will be rejected as "invalid marker".
            self.client
                .get("kernel.ast.printDeclaration", json!(decl_marker))
                .await
                .map_err(McpError::from)?;

            // startProofs requires an AST.Marker of type PVDecl (variable decl).
            // Convert #F<vid> (function declaration) to #v<vid> (variable declaration).
            let pvdecl_marker = decl_marker.replace("#F", "#v");

            self.client
                .exec(
                    "plugins.wp.startProofs",
                    json!(pvdecl_marker),
                    Duration::from_secs(600),
                )
                .await
                .map_err(McpError::from)?;
        }

        let tasks = self
            .client
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
        self.client
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let properties = self
            .client
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
            let comp = self
                .client
                .get("plugins.eva.general.getComputationState", json!(null))
                .await
                .unwrap_or(json!(null));
            result["eva"] = comp;
        }
        if wp_state {
            let tasks = self
                .client
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
        self.client
            .get("plugins.wp.reloadGoals", json!(null))
            .await
            .map_err(McpError::from)?;
        let goals = self
            .client
            .fetch_all("plugins.wp.fetchGoals")
            .await
            .map_err(McpError::from)?;

        let scope_marker = if let Some(ref func) = params.function {
            Some(self.resolve_function_or_refresh(func).await?.declaration)
        } else {
            None
        };

        let filtered: Vec<_> = goals
            .iter()
            .filter(|g| {
                if let Some(ref marker) = scope_marker {
                    if g["function"].as_str() != Some(marker) {
                        return false;
                    }
                }
                if let Some(ref status) = params.status {
                    if g["status"].as_str() != Some(status) {
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
        description = "List ACSL annotations on a function with their verification status."
    )]
    async fn get_current_annotations(
        &self,
        Parameters(params): Parameters<GetAnnotationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let info = self
            .resolve_function_or_refresh(&params.function)
            .await?;

        self.client
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let properties = self
            .client
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

        let callers = self
            .client
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
        self.client
            .get("kernel.properties.reloadStatus", json!(null))
            .await
            .map_err(McpError::from)?;
        let all_props = self
            .client
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
            if let Ok(values) = self
                .client
                .get("plugins.eva.values.getValues", json!({"target": kinstr}))
                .await
            {
                result["values"] = values;
            }
        }

        // Normal: callers of the enclosing function
        if let Some(scope) = prop["scope"].as_str() {
            if let Ok(callers) = self
                .client
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
            self.client
                .get("kernel.properties.reloadStatus", json!(null))
                .await
                .map_err(McpError::from)?;
            let props = self
                .client
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
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FramaCMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Frama-C formal verification server. Provides EVA abstract interpretation, \
                 WP deductive verification, and CIL AST navigation."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
