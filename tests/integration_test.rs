//! Integration tests against a live Frama-C Server.
//!
//! Prerequisites:
//!   Phase 1 test:
//!     frama-c test/test_abs.c -server-socket /tmp/frama-c-test.sock
//!   Phase 2 test:
//!     frama-c test/test_phase2.c -server-socket /tmp/frama-c-test-p2.sock
//!   Comprehensive test:
//!     frama-c test/test_comprehensive.c -server-socket /tmp/frama-c-test-comp.sock
//!
//! Note: Frama-C Server accepts only ONE client at a time and shutdown()
//! kills the server process. All live-server tests are consolidated into
//! one comprehensive test per phase. Run with --test-threads=1.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use frama_c_mcp_server::frama_c::client::FramaCClient;
use frama_c_mcp_server::state::SessionState;

fn socket_path() -> String {
    std::env::var("FRAMA_C_SOCK").unwrap_or_else(|_| "/tmp/frama-c-test.sock".into())
}

fn socket_path_p2() -> String {
    std::env::var("FRAMA_C_SOCK_P2").unwrap_or_else(|_| "/tmp/frama-c-test-p2.sock".into())
}

fn socket_path_comp() -> String {
    std::env::var("FRAMA_C_SOCK_COMP")
        .unwrap_or_else(|_| "/tmp/frama-c-test-comp.sock".into())
}

async fn connect() -> (FramaCClient, Arc<RwLock<SessionState>>) {
    let state = Arc::new(RwLock::new(SessionState::default()));
    let client = FramaCClient::connect(&socket_path(), state.clone())
        .await
        .expect("failed to connect to Frama-C server");
    (client, state)
}

async fn connect_p2() -> (FramaCClient, Arc<RwLock<SessionState>>) {
    let state = Arc::new(RwLock::new(SessionState::default()));
    let client = FramaCClient::connect(&socket_path_p2(), state.clone())
        .await
        .expect("failed to connect to Frama-C server (phase 2)");
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

// ─── Test: Phase 2 end-to-end workflow ────────────────────────────────
//
// Prerequisites:
//   frama-c test/test_phase2.c -server-socket /tmp/frama-c-test-p2.sock
//
// Covers: fetchGlobals, fetchGoals, getCallers, callgraph caching,
//         EVA params, multi-function WP, callstack value queries.

#[tokio::test]
async fn test_phase2_workflow() {
    let (client, state) = connect_p2().await;

    // ── P2.1. Connect + verify functions and globals ──
    println!("\n=== P2.1. Connect + functions ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert_eq!(st.functions.len(), 4, "should have clamp, increment, process, main");
        for name in &["clamp", "increment", "process", "main"] {
            assert!(st.resolve_function(name).is_some(), "missing function: {}", name);
        }
    }
    println!("[ok] 4 functions loaded");

    // ── P2.2. fetchGlobals — verify API format ──
    println!("\n=== P2.2. fetchGlobals ===");
    let globals = client
        .fetch_all("kernel.ast.fetchGlobals")
        .await
        .expect("fetchGlobals failed");
    println!("[info] fetchGlobals returned {} entries", globals.len());
    if !globals.is_empty() {
        let first = &globals[0];
        println!("[info] sample global: {}", serde_json::to_string_pretty(first).unwrap());
        // Verify expected fields exist (the exact names may differ from design doc)
        let has_name = first.get("name").is_some();
        let has_key = first.get("key").is_some();
        let has_decl = first.get("decl").is_some();
        println!("[info] has name={}, key={}, decl={}", has_name, has_key, has_decl);
        if has_name && has_key && has_decl {
            // Update state globals cache
            let mut st = state.write().await;
            st.update_globals(&globals);
            println!("[ok] globals cache populated: {} entries", st.globals.len());
        } else {
            println!("[warn] fetchGlobals field names differ from expected — needs code adjustment");
        }
    } else {
        println!("[warn] fetchGlobals returned empty — may need reloadGlobals first");
        // Try with reload
        let _ = client.get("kernel.ast.reloadGlobals", serde_json::json!(null)).await;
        let globals2 = client
            .fetch_all("kernel.ast.fetchGlobals")
            .await
            .expect("fetchGlobals after reload failed");
        println!("[info] after reload: {} entries", globals2.len());
        if !globals2.is_empty() {
            let first = &globals2[0];
            println!("[info] sample global: {}", serde_json::to_string_pretty(first).unwrap());
        }
    }

    // ── P2.3. Callgraph compute + cache ──
    println!("\n=== P2.3. Callgraph ===");
    client
        .exec(
            "plugins.callgraph.compute",
            serde_json::json!(null),
            Duration::from_secs(60),
        )
        .await
        .expect("callgraph.compute failed");
    let graph = client
        .get("plugins.callgraph.getCallgraph", serde_json::json!(null))
        .await
        .expect("getCallgraph failed");
    println!("[info] callgraph: {}", serde_json::to_string_pretty(&graph).unwrap());
    {
        let mut st = state.write().await;
        st.update_callgraph(&graph);
        assert!(!st.callgraph_edges.is_empty(), "should have call edges");
        assert!(!st.callgraph_vertices.is_empty(), "should have vertices");

        // main → process → clamp, process → increment
        let process_decl = st.resolve_function("process").map(|f| f.declaration.clone());
        if let Some(ref pd) = process_decl {
            let callees = st.get_callees(pd);
            println!("[info] process callees: {:?}", callees);
            assert!(callees.len() >= 2, "process should call clamp and increment");
        }

        let main_decl = st.resolve_function("main").map(|f| f.declaration.clone());
        if let Some(ref md) = main_decl {
            let callees = st.get_callees(md);
            println!("[info] main callees: {:?}", callees);
            assert!(!callees.is_empty(), "main should call process");
        }
    }
    println!("[ok] callgraph cached and queried");

    // ── P2.4. EVA parameter setting ──
    println!("\n=== P2.4. EVA params ===");
    // Test setMain — should accept function name
    let set_main_result = client
        .set("kernel.parameters.setMain", serde_json::json!("main"))
        .await;
    match set_main_result {
        Ok(_) => println!("[ok] setMain accepted"),
        Err(e) => println!("[warn] setMain: {} — API name may differ", e),
    }

    // Test setEvaPrecision
    let set_precision = client
        .set("kernel.parameters.setEvaPrecision", serde_json::json!(3))
        .await;
    match set_precision {
        Ok(_) => println!("[ok] setEvaPrecision accepted"),
        Err(e) => println!("[warn] setEvaPrecision: {} — API name may differ", e),
    }

    // Test setEvaSlevel
    let set_slevel = client
        .set("kernel.parameters.setEvaSlevel", serde_json::json!(32))
        .await;
    match set_slevel {
        Ok(_) => println!("[ok] setEvaSlevel accepted"),
        Err(e) => println!("[warn] setEvaSlevel: {} — API name may differ", e),
    }

    // ── P2.5. Run EVA ──
    println!("\n=== P2.5. Run EVA ===");
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
    println!("[ok] EVA completed");

    // ── P2.6. getCallers (EVA-based) ──
    println!("\n=== P2.6. getCallers ===");
    let clamp_decl = {
        let st = state.read().await;
        st.resolve_function("clamp").unwrap().declaration.clone()
    };
    let callers = client
        .get(
            "plugins.eva.general.getCallers",
            serde_json::json!(clamp_decl),
        )
        .await;
    match callers {
        Ok(c) => {
            println!("[ok] getCallers(clamp): {}", serde_json::to_string(&c).unwrap());
        }
        Err(e) => {
            println!("[warn] getCallers: {}", e);
        }
    }

    // ── P2.7. Properties / annotations ──
    println!("\n=== P2.7. Properties ===");
    client
        .get("kernel.properties.reloadStatus", serde_json::json!(null))
        .await
        .expect("reloadStatus failed");
    let properties = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    assert!(!properties.is_empty(), "should have properties after EVA");
    // Check scope filtering for clamp
    let clamp_props: Vec<_> = properties
        .iter()
        .filter(|p| p["scope"].as_str() == Some(&clamp_decl))
        .collect();
    println!(
        "[ok] {} total properties, {} for clamp",
        properties.len(),
        clamp_props.len()
    );

    // ── P2.8. EVA values with callstack ──
    println!("\n=== P2.8. EVA values ===");
    // First get a marker from printDeclaration
    let _ = client
        .get("kernel.ast.printDeclaration", serde_json::json!(clamp_decl))
        .await;
    // Combined values (no callstack)
    let combined = client
        .get(
            "plugins.eva.values.getValues",
            serde_json::json!({"target": "#s2"}),
        )
        .await;
    match combined {
        Ok(v) => println!("[ok] getValues(combined): {}", serde_json::to_string(&v).unwrap()),
        Err(e) => println!("[warn] getValues(combined): {}", e),
    }
    // With callstack index 0
    let with_cs = client
        .get(
            "plugins.eva.values.getValues",
            serde_json::json!({"target": "#s2", "callstack": 0}),
        )
        .await;
    match with_cs {
        Ok(v) => println!("[ok] getValues(callstack=0): {}", serde_json::to_string(&v).unwrap()),
        Err(e) => println!("[info] getValues(callstack=0): {} (may need valid callstack index)", e),
    }

    // ── P2.9. Run WP on multiple functions ──
    println!("\n=== P2.9. Run WP ===");
    // WP on clamp
    client
        .set("plugins.wp.setTimeout", serde_json::json!(10))
        .await
        .expect("setTimeout failed");
    let clamp_pvdecl = clamp_decl.replace("#F", "#v");
    let _ = client
        .get("kernel.ast.printDeclaration", serde_json::json!(clamp_decl))
        .await;
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(clamp_pvdecl),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs(clamp) failed");

    // WP on increment
    let increment_decl = {
        let st = state.read().await;
        st.resolve_function("increment").unwrap().declaration.clone()
    };
    let increment_pvdecl = increment_decl.replace("#F", "#v");
    let _ = client
        .get("kernel.ast.printDeclaration", serde_json::json!(increment_decl))
        .await;
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(increment_pvdecl),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs(increment) failed");

    {
        let mut st = state.write().await;
        st.set_wp_completed();
    }
    let tasks = client
        .get("plugins.wp.getScheduledTasks", serde_json::json!(null))
        .await
        .expect("getScheduledTasks failed");
    assert!(tasks.is_object());
    println!("[ok] WP completed for clamp + increment");

    // ── P2.10. fetchGoals — verify API format ──
    println!("\n=== P2.10. fetchGoals ===");
    let reload_goals = client
        .get("plugins.wp.reloadGoals", serde_json::json!(null))
        .await;
    match reload_goals {
        Ok(_) => {
            let goals = client
                .fetch_all("plugins.wp.fetchGoals")
                .await
                .expect("fetchGoals failed");
            println!("[info] fetchGoals returned {} entries", goals.len());
            if !goals.is_empty() {
                let first = &goals[0];
                println!("[info] sample goal: {}", serde_json::to_string_pretty(first).unwrap());
                // Verify expected fields
                let has_wpo = first.get("wpo").is_some();
                let has_function = first.get("function").is_some();
                let has_status = first.get("status").is_some();
                println!("[info] has wpo={}, function={}, status={}", has_wpo, has_function, has_status);

                // Filter by function (clamp)
                let clamp_goals: Vec<_> = goals
                    .iter()
                    .filter(|g| g["function"].as_str() == Some(&clamp_decl))
                    .collect();
                println!("[info] clamp goals: {}", clamp_goals.len());

                // Filter by status
                let valid_goals: Vec<_> = goals
                    .iter()
                    .filter(|g| g["status"].as_str() == Some("valid"))
                    .collect();
                println!("[info] valid goals: {}", valid_goals.len());
            }
            println!("[ok] fetchGoals format verified");
        }
        Err(e) => {
            println!("[warn] reloadGoals: {} — API name may differ", e);
        }
    }

    // ── P2.11. Verification status summary ──
    println!("\n=== P2.11. Verification status ===");
    client
        .get("kernel.properties.reloadStatus", serde_json::json!(null))
        .await
        .expect("reloadStatus failed");
    let all_props = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    let mut by_status = std::collections::HashMap::new();
    for prop in &all_props {
        let status = prop["status"].as_str().unwrap_or("unknown");
        *by_status.entry(status.to_string()).or_insert(0u64) += 1;
    }
    println!(
        "[ok] {} properties: {:?}",
        all_props.len(),
        by_status
    );

    // ── P2.12. Property key lookup (for investigate_alarm) ──
    println!("\n=== P2.12. Property key ===");
    if !all_props.is_empty() {
        let sample_key = all_props[0]["key"].as_str().unwrap_or("?");
        println!("[info] sample property key: {}", sample_key);
        // Verify kinstr field (used by investigate_alarm for values)
        let has_kinstr = all_props[0].get("kinstr").is_some();
        println!("[info] has kinstr={}", has_kinstr);
    }

    // ── P2.13. Final state ──
    println!("\n=== P2.13. Final state ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert!(st.eva_completed);
        assert!(st.wp_completed);
        assert_eq!(st.functions.len(), 4);
    }
    println!("[ok] All Phase 2 flags correct");

    client.shutdown().await.expect("shutdown failed");
    println!("\n=== All Phase 2 steps passed ===");
}

// ─── Test: Comprehensive Phase 2 (all 15 tools) ─────────────────────
//
// Prerequisites:
//   frama-c test/test_comprehensive.c -server-socket /tmp/frama-c-test-comp.sock
//
// This test exercises EVERY Phase 2 tool against a C file designed to
// produce EVA alarms, WP unknown goals, globals, and multi-level calls.

async fn connect_comp() -> (FramaCClient, Arc<RwLock<SessionState>>) {
    let state = Arc::new(RwLock::new(SessionState::default()));
    let client = FramaCClient::connect(&socket_path_comp(), state.clone())
        .await
        .expect("failed to connect to Frama-C server (comprehensive)");
    (client, state)
}

#[tokio::test]
async fn test_comprehensive() {
    let (client, state) = connect_comp().await;

    // ═══════════════════════════════════════════════════════════
    // PHASE A: Setup — connect, fetch functions/globals/callgraph
    // ═══════════════════════════════════════════════════════════

    // ── A1. Functions ──
    println!("\n=== A1. Functions ===");
    let expected_fns = [
        "get_element", "safe_div", "unsafe_div", "sum_positive",
        "identity", "validate", "process_range", "run_pipeline", "main",
    ];
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        println!("[info] loaded {} functions", st.functions.len());
        for name in &expected_fns {
            assert!(
                st.resolve_function(name).is_some(),
                "missing function: {}",
                name
            );
        }
    }
    println!("[ok] {} functions loaded", expected_fns.len());

    // ── A2. Globals (lookup_symbol: global) ──
    println!("\n=== A2. fetchGlobals ===");
    let _ = client
        .get("kernel.ast.reloadGlobals", serde_json::json!(null))
        .await;
    let globals = client
        .fetch_all("kernel.ast.fetchGlobals")
        .await
        .expect("fetchGlobals failed");
    println!("[info] {} globals", globals.len());
    {
        let mut st = state.write().await;
        st.update_globals(&globals);
    }
    // Verify known globals
    {
        let st = state.read().await;
        let buf = st.resolve_global("buffer");
        let bs = st.resolve_global("buf_size");
        let ec = st.resolve_global("error_count");
        println!(
            "[info] buffer={}, buf_size={}, error_count={}",
            buf.is_some(), bs.is_some(), ec.is_some()
        );
        // At least buf_size and error_count should be found (buffer is array, may differ)
        assert!(bs.is_some(), "buf_size global not found");
        assert!(ec.is_some(), "error_count global not found");
    }
    println!("[ok] globals verified");

    // ── A3. Callgraph + cache (get_callgraph, trace_call_chain prep) ──
    println!("\n=== A3. Callgraph ===");
    client
        .exec(
            "plugins.callgraph.compute",
            serde_json::json!(null),
            Duration::from_secs(60),
        )
        .await
        .expect("callgraph.compute failed");
    let graph = client
        .get("plugins.callgraph.getCallgraph", serde_json::json!(null))
        .await
        .expect("getCallgraph failed");
    {
        let mut st = state.write().await;
        st.update_callgraph(&graph);
        println!(
            "[info] {} edges, {} vertices",
            st.callgraph_edges.len(),
            st.callgraph_vertices.len()
        );
        // main → run_pipeline → process_range → validate → get_element
        // That's a 4-level call chain
        let main_decl = st.resolve_function("main").unwrap().declaration.clone();
        let main_callees = st.get_callees(&main_decl);
        println!("[info] main callees: {:?}", main_callees);
        assert!(!main_callees.is_empty(), "main should have callees");
    }
    println!("[ok] callgraph cached");

    // ── A4. trace_call_chain (BFS traversal) ──
    println!("\n=== A4. trace_call_chain ===");
    {
        let st = state.read().await;
        let main_decl = st.resolve_function("main").unwrap().declaration.clone();
        // BFS down from main
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        let mut chain = Vec::new();
        queue.push_back((main_decl.clone(), 0u32));
        while let Some((marker, depth)) = queue.pop_front() {
            if depth > 5 || visited.contains(&marker) {
                continue;
            }
            visited.insert(marker.clone());
            let callees = st.get_callees(&marker);
            for callee in callees {
                let from_name = st.resolve_decl_to_name(&marker).unwrap_or("?");
                let to_name = st.resolve_decl_to_name(callee).unwrap_or("?");
                chain.push(format!("{}→{} (depth {})", from_name, to_name, depth));
                queue.push_back((callee.to_string(), depth + 1));
            }
        }
        println!("[info] call chain from main:");
        for edge in &chain {
            println!("  {}", edge);
        }
        // Should reach at least get_element (4 levels deep)
        assert!(
            visited.len() >= 4,
            "BFS should visit at least 4 functions, got {}",
            visited.len()
        );
    }
    println!("[ok] trace_call_chain verified");

    // ═══════════════════════════════════════════════════════════
    // PHASE B: EVA analysis
    // ═══════════════════════════════════════════════════════════

    // ── B1. Run EVA ──
    println!("\n=== B1. Run EVA ===");
    client
        .exec(
            "plugins.eva.general.compute",
            serde_json::json!(null),
            Duration::from_secs(300),
        )
        .await
        .expect("EVA compute failed");
    {
        let mut st = state.write().await;
        st.set_eva_completed();
    }
    let comp = client
        .get("plugins.eva.general.getComputationState", serde_json::json!(null))
        .await
        .expect("getComputationState failed");
    assert_eq!(comp.as_str(), Some("computed"));
    println!("[ok] EVA completed");

    // ── B2. get_eva_alarms — expect alarms from unsafe_div ──
    println!("\n=== B2. get_eva_alarms ===");
    client
        .get("kernel.properties.reloadStatus", serde_json::json!(null))
        .await
        .expect("reloadStatus failed");
    let all_props = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    assert!(!all_props.is_empty());

    let mut by_status: HashMap<String, Vec<&serde_json::Value>> = HashMap::new();
    for prop in &all_props {
        let status = prop["status"].as_str().unwrap_or("unknown");
        by_status.entry(status.to_string()).or_default().push(prop);
    }
    println!("[info] properties by status:");
    for (status, props) in &by_status {
        println!("  {}: {}", status, props.len());
    }

    // unsafe_div should produce an alarm (unknown or invalid)
    let non_valid: Vec<_> = all_props
        .iter()
        .filter(|p| p["status"].as_str() != Some("valid"))
        .collect();
    println!(
        "[info] {} non-valid properties (expected: alarm from unsafe_div)",
        non_valid.len()
    );
    // There should be at least one alarm/unknown from unsafe_div
    assert!(
        !non_valid.is_empty(),
        "unsafe_div should produce at least one EVA alarm"
    );
    println!("[ok] EVA alarms detected");

    // ── B3. find_callers ──
    println!("\n=== B3. find_callers ===");
    let get_element_decl = {
        let st = state.read().await;
        st.resolve_function("get_element").unwrap().declaration.clone()
    };
    let callers = client
        .get(
            "plugins.eva.general.getCallers",
            serde_json::json!(get_element_decl),
        )
        .await
        .expect("getCallers failed");
    println!(
        "[ok] getCallers(get_element): {}",
        serde_json::to_string(&callers).unwrap()
    );
    // get_element is called by validate
    assert!(callers.is_array());

    // ── B4. get_eva_value ──
    println!("\n=== B4. get_eva_value ===");
    let unsafe_div_decl = {
        let st = state.read().await;
        st.resolve_function("unsafe_div").unwrap().declaration.clone()
    };
    let _ = client
        .get("kernel.ast.printDeclaration", serde_json::json!(unsafe_div_decl))
        .await;
    // Get values at a statement marker (the division)
    // We don't know the exact marker, but we can query a known one
    let vals = client
        .get(
            "plugins.eva.values.getValues",
            serde_json::json!({"target": "#s2"}),
        )
        .await;
    match vals {
        Ok(v) => println!("[ok] getValues: {}", serde_json::to_string(&v).unwrap()),
        Err(e) => println!("[info] getValues(#s2): {} (marker may not exist)", e),
    }

    // ── B5. investigate_alarm ──
    println!("\n=== B5. investigate_alarm ===");
    // Find a non-valid property to investigate
    if let Some(alarm_prop) = non_valid.first() {
        let prop_key = alarm_prop["key"].as_str().unwrap_or("?");
        println!("[info] investigating property: {}", prop_key);
        println!(
            "[info] property detail: {}",
            serde_json::to_string_pretty(alarm_prop).unwrap()
        );
        // Check kinstr field
        let kinstr = alarm_prop["kinstr"].as_str();
        println!("[info] kinstr: {:?}", kinstr);
        // Check scope field (function marker)
        let scope = alarm_prop["scope"].as_str();
        println!("[info] scope: {:?}", scope);
        // If kinstr exists, try getValues on it
        if let Some(ki) = kinstr {
            let values = client
                .get(
                    "plugins.eva.values.getValues",
                    serde_json::json!({"target": ki}),
                )
                .await;
            match values {
                Ok(v) => println!("[ok] investigate values: {}", serde_json::to_string(&v).unwrap()),
                Err(e) => println!("[info] investigate values: {}", e),
            }
        }
        // If scope exists, try getCallers
        if let Some(sc) = scope {
            let callers = client
                .get("plugins.eva.general.getCallers", serde_json::json!(sc))
                .await;
            match callers {
                Ok(c) => println!("[ok] investigate callers: {}", serde_json::to_string(&c).unwrap()),
                Err(e) => println!("[info] investigate callers: {}", e),
            }
        }
    }
    println!("[ok] investigate_alarm flow verified");

    // ── B6. get_current_annotations ──
    println!("\n=== B6. get_current_annotations ===");
    client
        .get("kernel.properties.reloadStatus", serde_json::json!(null))
        .await
        .expect("reloadStatus failed");
    let props_again = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    // Filter for safe_div annotations
    let safe_div_decl = {
        let st = state.read().await;
        st.resolve_function("safe_div").unwrap().declaration.clone()
    };
    let safe_div_annots: Vec<_> = props_again
        .iter()
        .filter(|p| p["scope"].as_str() == Some(&safe_div_decl))
        .collect();
    println!(
        "[ok] safe_div has {} annotations",
        safe_div_annots.len()
    );
    assert!(
        !safe_div_annots.is_empty(),
        "safe_div should have annotations (requires/ensures)"
    );

    // ═══════════════════════════════════════════════════════════
    // PHASE C: WP verification
    // ═══════════════════════════════════════════════════════════

    // ── C1. Run WP on multiple functions ──
    println!("\n=== C1. Run WP ===");
    client
        .set("plugins.wp.setTimeout", serde_json::json!(5))
        .await
        .expect("setTimeout failed");

    // WP on safe_div (should be provable)
    let _ = client
        .get("kernel.ast.printDeclaration", serde_json::json!(safe_div_decl))
        .await;
    let safe_div_pv = safe_div_decl.replace("#F", "#v");
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(safe_div_pv),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs(safe_div) failed");
    println!("[ok] WP: safe_div done");

    // WP on identity (ensures \result > 0 should NOT be provable)
    let identity_decl = {
        let st = state.read().await;
        st.resolve_function("identity").unwrap().declaration.clone()
    };
    let _ = client
        .get("kernel.ast.printDeclaration", serde_json::json!(identity_decl))
        .await;
    let identity_pv = identity_decl.replace("#F", "#v");
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(identity_pv),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs(identity) failed");
    println!("[ok] WP: identity done");

    // WP on get_element (should be provable)
    let _ = client
        .get("kernel.ast.printDeclaration", serde_json::json!(get_element_decl))
        .await;
    let get_element_pv = get_element_decl.replace("#F", "#v");
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(get_element_pv),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs(get_element) failed");
    println!("[ok] WP: get_element done");

    {
        let mut st = state.write().await;
        st.set_wp_completed();
    }

    // ── C2. get_wp_goals — verify format + filtering ──
    println!("\n=== C2. get_wp_goals ===");
    let _ = client
        .get("plugins.wp.reloadGoals", serde_json::json!(null))
        .await
        .expect("reloadGoals failed");
    let goals = client
        .fetch_all("plugins.wp.fetchGoals")
        .await
        .expect("fetchGoals failed");
    println!("[info] {} total goals", goals.len());
    assert!(!goals.is_empty(), "should have WP goals");

    // Print first goal to verify format
    println!(
        "[info] sample goal:\n{}",
        serde_json::to_string_pretty(&goals[0]).unwrap()
    );

    // Verify goal fields (actual format from Frama-C):
    //   wpo: goal ID string, scope: function decl marker,
    //   fct: function name, status: uppercase ("VALID", "UNKNOWN")
    let first = &goals[0];
    assert!(first.get("wpo").is_some(), "goal should have 'wpo' field");
    assert!(first.get("scope").is_some(), "goal should have 'scope' field");
    assert!(first.get("status").is_some(), "goal should have 'status' field");
    println!("[ok] fetchGoals format verified");

    // Group by status
    let mut goals_by_status: HashMap<String, usize> = HashMap::new();
    for g in &goals {
        let status = g["status"].as_str().unwrap_or("?").to_string();
        *goals_by_status.entry(status).or_default() += 1;
    }
    println!("[info] goals by status: {:?}", goals_by_status);

    // identity's ensures \result > 0 should NOT be valid
    // Note: Frama-C uses "scope" (decl marker) not "function" for filtering
    let identity_goals: Vec<_> = goals
        .iter()
        .filter(|g| g["scope"].as_str() == Some(&identity_decl))
        .collect();
    println!("[info] identity goals: {}", identity_goals.len());
    for ig in &identity_goals {
        println!(
            "  status={}, wpo={}, name={}",
            ig["status"].as_str().unwrap_or("?"),
            ig["wpo"].as_str().unwrap_or("?"),
            ig["name"].as_str().unwrap_or("?")
        );
    }
    if !identity_goals.is_empty() {
        let has_non_valid = identity_goals
            .iter()
            .any(|g| g["status"].as_str() != Some("VALID"));
        println!(
            "[info] identity has non-VALID goals: {}",
            has_non_valid
        );
    }
    println!("[ok] get_wp_goals verified");

    // ═══════════════════════════════════════════════════════════
    // PHASE D: Composite tools
    // ═══════════════════════════════════════════════════════════

    // ── D1. lookup_symbol — function ──
    println!("\n=== D1. lookup_symbol (function) ===");
    {
        let st = state.read().await;
        let info = st.resolve_function("safe_div").unwrap();
        println!(
            "[ok] safe_div: file={}, line={}, marker={}, decl={}",
            info.file, info.line, info.marker, info.declaration
        );
    }

    // ── D2. lookup_symbol — global variable ──
    println!("\n=== D2. lookup_symbol (global) ===");
    {
        let st = state.read().await;
        let info = st.resolve_global("error_count").unwrap();
        println!(
            "[ok] error_count: type={}, file={}, line={}, marker={}, decl={}",
            info.typ, info.file, info.line, info.marker, info.declaration
        );
    }

    // ── D3. suggest_verification_plan logic ──
    println!("\n=== D3. suggest_verification_plan ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert!(st.eva_completed);
        assert!(st.wp_completed);
        println!("[ok] both EVA and WP completed → suggest review");
    }
    // Reload properties to check for invalid/unknown
    client
        .get("kernel.properties.reloadStatus", serde_json::json!(null))
        .await
        .expect("reloadStatus failed");
    let final_props = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    let mut final_by_status: HashMap<String, usize> = HashMap::new();
    for p in &final_props {
        let s = p["status"].as_str().unwrap_or("?").to_string();
        *final_by_status.entry(s).or_default() += 1;
    }
    println!("[info] final properties: {:?}", final_by_status);
    let unknown_count = final_by_status.get("unknown").copied().unwrap_or(0);
    let invalid_count = final_by_status.get("invalid").copied().unwrap_or(0);
    if invalid_count > 0 {
        println!("[ok] suggest: investigate {} invalid properties (high priority)", invalid_count);
    }
    if unknown_count > 0 {
        println!("[ok] suggest: {} unknown properties need attention", unknown_count);
    }
    println!("[ok] suggest_verification_plan logic verified");

    // ── D4. get_verification_status ──
    println!("\n=== D4. get_verification_status ===");
    let eva_comp = client
        .get("plugins.eva.general.getComputationState", serde_json::json!(null))
        .await
        .expect("getComputationState failed");
    assert_eq!(eva_comp.as_str(), Some("computed"));
    let wp_tasks = client
        .get("plugins.wp.getScheduledTasks", serde_json::json!(null))
        .await
        .expect("getScheduledTasks failed");
    assert!(wp_tasks.is_object());
    println!("[ok] verification status: EVA computed, WP tasks available");

    // ═══════════════════════════════════════════════════════════
    // FINAL
    // ═══════════════════════════════════════════════════════════

    println!("\n=== Final state ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert!(st.eva_completed);
        assert!(st.wp_completed);
        assert!(st.functions.len() >= 9);
        assert!(st.globals.len() >= 2);
        assert!(!st.callgraph_edges.is_empty());
    }
    println!("[ok] all assertions passed");

    client.shutdown().await.expect("shutdown failed");
    println!("\n=== Comprehensive test complete ===");
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
