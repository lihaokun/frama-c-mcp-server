use std::collections::HashMap;

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
}

#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub name: String,
    pub marker: String,
    pub declaration: String,
    pub signature: String,
    pub file: String,
    pub line: u32,
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
}
