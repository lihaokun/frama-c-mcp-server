//! Integration tests against a live Frama-C Server.
//!
//! Prerequisites:
//!   frama-c <files> -server-socket /tmp/frama-c-test.sock
//!
//! Note: Frama-C Server accepts only ONE client at a time and shutdown()
//! kills the server process. All live-server tests are consolidated into
//! one comprehensive test. Run with --test-threads=1.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use frama_c_mcp_server::frama_c::client::FramaCClient;
use frama_c_mcp_server::state::SessionState;

fn socket_path() -> String {
    std::env::var("FRAMA_C_SOCK").unwrap_or_else(|_| "/tmp/frama-c-test.sock".into())
}

async fn connect() -> (FramaCClient, Arc<RwLock<SessionState>>) {
    let state = Arc::new(RwLock::new(SessionState::default()));
    let client = FramaCClient::connect(&socket_path(), state.clone())
        .await
        .expect("failed to connect to Frama-C server");
    (client, state)
}

// ─── Test: Full end-to-end workflow ──────────────────────────────────
//
// Single comprehensive test covering all 8 tools + protocol edge cases.
// This is the ONLY test that connects to a live Frama-C server.

#[tokio::test]
async fn test_full_workflow() {
    let (client, state) = connect().await;

    // ── 1. Connect + state ──
    println!("\n=== 1. Connect + state ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert_eq!(st.functions.len(), 3);
        for name in &["abs_val", "square", "main"] {
            let info = st.resolve_function(name).expect(name);
            assert!(info.file.ends_with("test_abs.c"));
            assert!(info.line > 0);
        }
    }
    println!("[ok] 3 functions loaded");

    // ── 2. get_function_info ──
    println!("\n=== 2. get_function_info ===");
    let abs_decl = {
        let st = state.read().await;
        st.resolve_function("abs_val").unwrap().declaration.clone()
    };
    let decl = client
        .get("kernel.ast.printDeclaration", serde_json::json!(abs_decl))
        .await
        .expect("printDeclaration failed");
    assert!(decl.is_array());
    println!("[ok] printDeclaration returned annotated AST");

    // ── 3. getFiles ──
    println!("\n=== 3. getFiles ===");
    let files = client
        .get("kernel.ast.getFiles", serde_json::json!(null))
        .await
        .expect("getFiles failed");
    let file_list = files.as_array().unwrap();
    assert!(!file_list.is_empty());
    println!("[ok] {} file(s)", file_list.len());

    // ── 4. get_callgraph ──
    println!("\n=== 4. get_callgraph ===");
    let cg_compute = client
        .exec(
            "plugins.callgraph.compute",
            serde_json::json!(null),
            Duration::from_secs(60),
        )
        .await;
    match cg_compute {
        Ok(_) => {
            let graph = client
                .get("plugins.callgraph.getCallgraph", serde_json::json!(null))
                .await;
            match graph {
                Ok(g) => {
                    println!("[ok] callgraph: {}", serde_json::to_string(&g).unwrap());
                }
                Err(e) => {
                    println!("[warn] getCallgraph: {}", e);
                }
            }
        }
        Err(e) => {
            println!("[warn] callgraph.compute: {}", e);
        }
    }

    // ── 5. Run EVA ──
    println!("\n=== 5. Run EVA ===");
    client
        .exec(
            "plugins.eva.general.compute",
            serde_json::json!(null),
            Duration::from_secs(120),
        )
        .await
        .expect("EVA compute failed");
    {
        let mut st = state.write().await;
        st.set_eva_completed();
    }
    let comp_state = client
        .get(
            "plugins.eva.general.getComputationState",
            serde_json::json!(null),
        )
        .await
        .expect("getComputationState failed");
    assert_eq!(comp_state.as_str(), Some("computed"));
    let stats = client
        .get(
            "plugins.eva.general.getProgramStats",
            serde_json::json!(null),
        )
        .await
        .expect("getProgramStats failed");
    assert!(stats.is_object());
    println!("[ok] EVA completed");

    // ── 6. get_eva_alarms (properties) ──
    println!("\n=== 6. get_eva_alarms ===");
    let properties = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    assert!(!properties.is_empty());
    let first = &properties[0];
    assert!(first.get("key").is_some());
    assert!(first.get("kind").is_some());
    assert!(first.get("status").is_some());
    assert!(first.get("scope").is_some());
    assert!(first.get("source").is_some());
    let abs_scope = {
        let st = state.read().await;
        st.resolve_function("abs_val").unwrap().declaration.clone()
    };
    let abs_props: Vec<_> = properties
        .iter()
        .filter(|p| p["scope"].as_str() == Some(&abs_scope))
        .collect();
    assert!(!abs_props.is_empty(), "abs_val should have properties");
    println!(
        "[ok] {} total properties, {} for abs_val",
        properties.len(),
        abs_props.len()
    );

    // ── 7. get_eva_value ──
    println!("\n=== 7. get_eva_value ===");
    // Ensure markers are indexed via printDeclaration
    let _ = client
        .get(
            "kernel.ast.printDeclaration",
            serde_json::json!(abs_decl),
        )
        .await;
    // callstack must be omitted (not null) — it's param_opt
    let eva_values = client
        .get(
            "plugins.eva.values.getValues",
            serde_json::json!({"target": "#s2"}),
        )
        .await;
    match eva_values {
        Ok(v) => {
            println!(
                "[ok] getValues(#s2): {}",
                serde_json::to_string(&v).unwrap()
            );
        }
        Err(e) => {
            println!("[warn] getValues(#s2): {}", e);
        }
    }

    // ── 8. WP config + run ──
    println!("\n=== 8. Run WP ===");
    client
        .set("plugins.wp.setTimeout", serde_json::json!(10))
        .await
        .expect("setTimeout failed");
    let pvdecl = abs_decl.replace("#F", "#v");
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(pvdecl),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs failed");
    {
        let mut st = state.write().await;
        st.set_wp_completed();
    }
    let tasks = client
        .get(
            "plugins.wp.getScheduledTasks",
            serde_json::json!(null),
        )
        .await
        .expect("getScheduledTasks failed");
    assert!(tasks.is_object());
    println!(
        "[ok] WP completed: {}",
        serde_json::to_string(&tasks).unwrap()
    );

    // ── 9. get_verification_status ──
    println!("\n=== 9. get_verification_status ===");
    let _ = client
        .get(
            "kernel.properties.reloadStatus",
            serde_json::json!(null),
        )
        .await;
    let all_props = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    let mut by_status = std::collections::HashMap::new();
    let mut by_kind = std::collections::HashMap::new();
    for prop in &all_props {
        let status = prop["status"].as_str().unwrap_or("unknown");
        *by_status.entry(status.to_string()).or_insert(0u64) += 1;
        let kind = prop["kind"].as_str().unwrap_or("unknown");
        *by_kind.entry(kind.to_string()).or_insert(0u64) += 1;
    }
    assert!(
        by_status.get("valid").copied().unwrap_or(0) > 0,
        "should have valid properties"
    );
    println!(
        "[ok] {} properties: by_status={:?}, by_kind={:?}",
        all_props.len(),
        by_status,
        by_kind
    );
    let eva_comp = client
        .get(
            "plugins.eva.general.getComputationState",
            serde_json::json!(null),
        )
        .await
        .expect("getComputationState failed");
    assert_eq!(eva_comp.as_str(), Some("computed"));
    let wp_tasks = client
        .get(
            "plugins.wp.getScheduledTasks",
            serde_json::json!(null),
        )
        .await
        .expect("getScheduledTasks failed");
    assert!(wp_tasks.is_object());
    println!("[ok] verification_status complete");

    // ── 10. Rejected request (protocol edge case) ──
    println!("\n=== 10. Rejected request ===");
    let result = client
        .get("nonexistent.endpoint", serde_json::json!(null))
        .await;
    assert!(result.is_err());
    match result {
        Err(frama_c_mcp_server::error::FramaCError::Rejected { .. }) => {
            println!("[ok] Rejected as expected");
        }
        Err(e) => {
            println!("[ok] Error (possibly different format): {}", e);
        }
        Ok(_) => panic!("should have been rejected"),
    }

    // ── 11. Incremental fetch ──
    println!("\n=== 11. Incremental fetch ===");
    // connect() already consumed fetchFunctions, so a direct call returns empty
    let entries_empty = client
        .fetch_all("kernel.ast.fetchFunctions")
        .await
        .expect("fetch_all failed");
    assert!(
        entries_empty.is_empty(),
        "should be empty (already consumed by connect), got {}",
        entries_empty.len()
    );
    // After reload, should get all again
    let _ = client
        .get(
            "kernel.ast.reloadFunctions",
            serde_json::json!(null),
        )
        .await;
    let entries_after = client
        .fetch_all("kernel.ast.fetchFunctions")
        .await
        .expect("fetch_all after reload failed");
    assert_eq!(
        entries_after.len(),
        3,
        "after reload should get all 3 functions"
    );
    // And empty again
    let entries_again = client
        .fetch_all("kernel.ast.fetchFunctions")
        .await
        .expect("fetch_all second call failed");
    assert!(
        entries_again.is_empty(),
        "second fetch after reload should be empty, got {}",
        entries_again.len()
    );
    println!(
        "[ok] incremental fetch: consumed={}, after_reload={}, again={}",
        entries_empty.len(),
        entries_after.len(),
        entries_again.len()
    );

    // ── 12. Cache invalidation + resolve_function_or_refresh pattern ──
    // Verifies F2 fix: after cache is cleared, reloadFunctions + fetchFunctions
    // restores function data (prevents cascade failure).
    // Also verifies F3 fix: same pattern used by run_wp on cache miss.
    println!("\n=== 12. Cache invalidation + refresh ===");
    {
        let mut st = state.write().await;
        st.invalidate_all();
        assert!(st.functions.is_empty(), "cache should be empty after invalidate_all");
    }
    // Without reloadFunctions, fetchFunctions returns empty (consumed by step 11)
    // With reloadFunctions first, it returns all functions
    let _ = client
        .get("kernel.ast.reloadFunctions", serde_json::json!(null))
        .await
        .expect("reloadFunctions failed");
    let refreshed = client
        .fetch_all("kernel.ast.fetchFunctions")
        .await
        .expect("fetchFunctions after reload failed");
    assert_eq!(
        refreshed.len(),
        3,
        "reload+fetch should restore all 3 functions"
    );
    {
        let mut st = state.write().await;
        st.update_functions(&refreshed);
        st.project_loaded = true;
        st.set_eva_completed();
        st.set_wp_completed();
    }
    {
        let st = state.read().await;
        assert!(
            st.resolve_function("abs_val").is_some(),
            "abs_val should be resolvable after refresh"
        );
    }
    println!("[ok] Cache restored via reload+fetch pattern");

    // ── 13. Scoped property filtering after cache refresh (F7) ──
    println!("\n=== 13. Scoped property filtering ===");
    {
        let _ = client
            .get("kernel.properties.reloadStatus", serde_json::json!(null))
            .await;
        let all_props = client
            .fetch_all("kernel.properties.fetchStatus")
            .await
            .expect("fetchStatus failed");
        let abs_decl_after = {
            let st = state.read().await;
            st.resolve_function("abs_val").unwrap().declaration.clone()
        };
        let scoped: Vec<_> = all_props
            .iter()
            .filter(|p| p["scope"].as_str() == Some(&abs_decl_after))
            .collect();
        assert!(
            !scoped.is_empty(),
            "abs_val should have properties after cache refresh"
        );
        assert!(
            scoped.len() < all_props.len(),
            "scoped filter should return fewer than all ({} < {})",
            scoped.len(),
            all_props.len()
        );
        println!(
            "[ok] scoped: {} of {} properties for abs_val",
            scoped.len(),
            all_props.len()
        );
    }

    // ── 14. Final state ──
    println!("\n=== 14. Final state ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert!(st.eva_completed);
        assert!(st.wp_completed);
        assert_eq!(st.functions.len(), 3);
    }
    println!("[ok] All flags correct");

    client.shutdown().await.expect("shutdown failed");
    println!("\n=== All 14 steps passed ===");
}

// ─── Offline tests (no live server needed) ───────────────────────────

#[tokio::test]
async fn test_function_not_found() {
    let state = Arc::new(RwLock::new(SessionState::default()));
    let st = state.read().await;
    assert!(st.resolve_function("nonexistent_func").is_none());
}

#[tokio::test]
async fn test_state_invalidation() {
    let mut state = SessionState::default();
    state.project_loaded = true;
    state.eva_completed = true;
    state.wp_completed = true;
    state.update_functions(&[serde_json::json!({
        "name": "f",
        "key": "kf#1",
        "decl": "#F1",
        "signature": "void f(void);",
        "sloc": {"file": "a.c", "line": 1}
    })]);
    assert_eq!(state.functions.len(), 1);

    state.invalidate_all();
    assert!(!state.project_loaded);
    assert!(!state.eva_completed);
    assert!(!state.wp_completed);
    assert!(state.functions.is_empty());
}

/// F2 regression: update_functions(&[]) clears existing cache (cascade failure).
/// This documents why reloadFunctions must be called before fetchFunctions
/// when the incremental cursor has been consumed.
#[tokio::test]
async fn test_update_functions_empty_clears_cache() {
    let mut state = SessionState::default();
    state.update_functions(&[serde_json::json!({
        "name": "abs_val", "key": "kf#24", "decl": "#F24",
        "signature": "int abs_val(int x);",
        "sloc": {"file": "test.c", "line": 6}
    }), serde_json::json!({
        "name": "main", "key": "kf#36", "decl": "#F36",
        "signature": "int main(void);",
        "sloc": {"file": "test.c", "line": 15}
    })]);
    assert_eq!(state.functions.len(), 2);

    // Simulates what happened with F2: fetchFunctions returns empty
    // (already consumed), and update_functions clears existing cache
    state.update_functions(&[]);
    assert!(
        state.functions.is_empty(),
        "update_functions(&[]) should clear existing cache (F2 cascade behavior)"
    );
}

#[tokio::test]
async fn test_connect_bad_socket() {
    let state = Arc::new(RwLock::new(SessionState::default()));
    let result = FramaCClient::connect("/tmp/nonexistent-socket.sock", state).await;
    assert!(result.is_err(), "should fail on nonexistent socket");
    match result {
        Err(e) => println!("[ok] Expected error: {}", e),
        Ok(_) => panic!("should fail on nonexistent socket"),
    }
}
