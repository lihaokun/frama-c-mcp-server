use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct SessionState {
    pub project_loaded: bool,
    pub eva_completed: bool,
    pub wp_completed: bool,
    pub functions: HashMap<String, FunctionInfo>,
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

    pub fn invalidate_all(&mut self) {
        self.project_loaded = false;
        self.eva_completed = false;
        self.wp_completed = false;
        self.functions.clear();
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
        state.invalidate_all();
        assert!(!state.project_loaded);
        assert!(!state.eva_completed);
        assert!(!state.wp_completed);
        assert!(state.functions.is_empty());
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
}
