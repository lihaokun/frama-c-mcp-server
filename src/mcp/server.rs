use std::collections::HashMap;
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
        description = "Run EVA abstract interpretation analysis. Returns computation state and program statistics. This may take several minutes for large programs."
    )]
    async fn run_eva(&self) -> Result<CallToolResult, McpError> {
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
        // callstack is optional (param_opt). Omit it to get combined values
        // across all callstacks. If provided, it must be an integer index
        // obtained from getCallstacks.
        let values = self
            .client
            .get(
                "plugins.eva.values.getValues",
                json!({"target": params.marker}),
            )
            .await
            .map_err(McpError::from)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&values).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Run WP deductive verification on a specific function. Requires function_name to identify the target. Returns proof task statistics. This may take several minutes."
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

        let info = self
            .resolve_function_or_refresh(&params.function_name)
            .await?;
        let decl_marker = info.declaration;

        // printDeclaration indexes markers in the server's marker table.
        // Without this call, PVDecl markers (needed by startProofs) are not
        // registered and will be rejected as "invalid marker".
        self.client
            .get("kernel.ast.printDeclaration", json!(decl_marker))
            .await
            .map_err(McpError::from)?;

        // startProofs requires an AST.Marker of type PVDecl (variable decl).
        // Convert #F<vid> (function declaration) to #v<vid> (variable declaration).
        // Both use the same Cil varinfo.vid, so the numeric suffix is identical.
        let pvdecl_marker = decl_marker.replace("#F", "#v");

        self.client
            .exec(
                "plugins.wp.startProofs",
                json!(pvdecl_marker),
                Duration::from_secs(600),
            )
            .await
            .map_err(McpError::from)?;
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
