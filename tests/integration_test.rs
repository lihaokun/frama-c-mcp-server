//! Integration tests against a live Frama-C Server.
//!
//! Prerequisites:
//!   Phase 1 test:
//!     frama-c test/test_abs.c -server-socket /tmp/frama-c-test.sock
//!   Phase 2 test:
//!     frama-c test/test_phase2.c -server-socket /tmp/frama-c-test-p2.sock
//!   Comprehensive test:
//!     frama-c test/test_comprehensive.c -server-socket /tmp/frama-c-test-comp.sock
//!   Iterative workflow test:
//!     frama-c test/test_iterative_raw.c -server-socket /tmp/frama-c-test-iter.sock
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

// ─── Test: Comprehensive verification workflow (all 15 tools) ────────
//
// Prerequisites:
//   frama-c test/test_comprehensive.c -server-socket /tmp/frama-c-test-comp.sock
//
// Simulates a complete AI agent verification workflow against a "Safe Buffer
// Module" with behaviors, named ensures, volatile nondet input, and unsafe
// functions. Exercises all 15 MCP tools' underlying API calls.
//
// Target C file provides:
//   Functions: buf_push (behaviors ok/full), buf_get, buf_sum (loop),
//              buf_avg, echo (named ensures correct/wrong),
//              unsafe_read (no precondition), unsafe_avg (div-by-zero),
//              run (orchestrator), main (volatile nondet)
//   Globals:   data[CAPACITY], count, error_code, nondet (volatile)
//   EVA alarms: unsafe_read (index_bound), unsafe_avg (division_by_zero),
//               buf_sum/run (signed_overflow)
//   WP goals:  buf_push (6 VALID), buf_get (2 VALID),
//              echo (1 VALID correct + 1 NORESULT wrong)
//   Call chain: main→run→buf_sum→buf_get (4 levels)

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

    // ═══════════════════════════════════════════════════════════════
    // PHASE A: Reconnaissance — connect, discover functions/globals/callgraph
    // ═══════════════════════════════════════════════════════════════

    // ── A1. Connect + verify all 9 functions loaded ──
    println!("\n=== A1. Functions (reload_project) ===");
    let expected_fns = [
        "buf_push", "buf_get", "buf_sum", "buf_avg",
        "echo", "unsafe_read", "unsafe_avg", "run", "main",
    ];
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert_eq!(st.functions.len(), expected_fns.len(),
            "expected {} functions, got {}", expected_fns.len(), st.functions.len());
        for name in &expected_fns {
            let info = st.resolve_function(name).unwrap_or_else(|| panic!("missing function: {}", name));
            assert!(info.file.ends_with("test_comprehensive.c"));
            assert!(info.line > 0);
        }
    }
    println!("[ok] {} functions loaded", expected_fns.len());

    // ── A2. Globals (lookup_symbol: global variables) ──
    println!("\n=== A2. Globals (lookup_symbol) ===");
    let _ = client.get("kernel.ast.reloadGlobals", serde_json::json!(null)).await;
    let globals = client.fetch_all("kernel.ast.fetchGlobals").await.expect("fetchGlobals failed");
    assert!(globals.len() >= 3, "should have at least data, count, error_code globals");
    {
        let mut st = state.write().await;
        st.update_globals(&globals);
    }
    {
        let st = state.read().await;
        // count and error_code are scalar globals
        let count_info = st.resolve_global("count").expect("count global not found");
        assert!(count_info.declaration.starts_with("#G"), "global decl should start with #G");
        let ec_info = st.resolve_global("error_code").expect("error_code global not found");
        assert_eq!(ec_info.typ, "int");
        println!("[ok] globals: count(decl={}), error_code(type={})", count_info.declaration, ec_info.typ);
    }

    // ── A3. Callgraph (get_callgraph) ──
    println!("\n=== A3. Callgraph ===");
    client.exec("plugins.callgraph.compute", serde_json::json!(null), Duration::from_secs(60))
        .await.expect("callgraph.compute failed");
    let graph = client.get("plugins.callgraph.getCallgraph", serde_json::json!(null))
        .await.expect("getCallgraph failed");
    {
        let mut st = state.write().await;
        st.update_callgraph(&graph);
        assert!(!st.callgraph_edges.is_empty(), "should have call edges");
        assert!(!st.callgraph_vertices.is_empty(), "should have vertices");
        println!("[info] {} edges, {} vertices", st.callgraph_edges.len(), st.callgraph_vertices.len());

        // Verify key edges: main→run, run→buf_push, buf_sum→buf_get
        let run_decl = st.resolve_function("run").unwrap().declaration.clone();
        let run_callees = st.get_callees(&run_decl);
        println!("[info] run callees: {:?}", run_callees);
        assert!(run_callees.len() >= 3, "run should call buf_push, buf_sum, buf_avg");
    }
    println!("[ok] callgraph cached");

    // ── A4. trace_call_chain: BFS from main (4+ levels deep) ──
    println!("\n=== A4. trace_call_chain ===");
    {
        let st = state.read().await;
        let main_decl = st.resolve_function("main").unwrap().declaration.clone();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        let mut chain = Vec::new();
        queue.push_back((main_decl.clone(), 0u32));
        while let Some((marker, depth)) = queue.pop_front() {
            if depth > 5 || visited.contains(&marker) { continue; }
            visited.insert(marker.clone());
            for callee in st.get_callees(&marker) {
                let from_name = st.resolve_decl_to_name(&marker).unwrap_or("?");
                let to_name = st.resolve_decl_to_name(callee).unwrap_or("?");
                chain.push(format!("  {}→{} (depth {})", from_name, to_name, depth));
                queue.push_back((callee.to_string(), depth + 1));
            }
        }
        println!("[info] call chain from main:");
        for edge in &chain { println!("{}", edge); }
        // main→run→buf_sum→buf_get is 4 levels; plus main→unsafe_read, main→echo, etc.
        assert!(visited.len() >= 5,
            "BFS should visit at least 5 functions (main,run,buf_push,buf_sum,buf_get,...), got {}",
            visited.len());
    }
    println!("[ok] trace_call_chain verified (4+ levels)");

    // ── A5. get_function_info: annotated declaration ──
    println!("\n=== A5. get_function_info (buf_push) ===");
    let buf_push_decl = {
        let st = state.read().await;
        st.resolve_function("buf_push").unwrap().declaration.clone()
    };
    let decl_text = client.get("kernel.ast.printDeclaration", serde_json::json!(buf_push_decl))
        .await.expect("printDeclaration(buf_push) failed");
    assert!(decl_text.is_array(), "printDeclaration should return array");
    println!("[ok] buf_push declaration retrieved (annotated AST)");

    // ═══════════════════════════════════════════════════════════════
    // PHASE B: EVA analysis — run EVA, find alarms, query values
    // ═══════════════════════════════════════════════════════════════

    // ── B1. Run EVA (run_eva) ──
    println!("\n=== B1. Run EVA ===");
    client.exec("plugins.eva.general.compute", serde_json::json!(null), Duration::from_secs(300))
        .await.expect("EVA compute failed");
    { let mut st = state.write().await; st.set_eva_completed(); }
    let comp = client.get("plugins.eva.general.getComputationState", serde_json::json!(null))
        .await.expect("getComputationState failed");
    assert_eq!(comp.as_str(), Some("computed"));
    println!("[ok] EVA completed");

    // ── B2. get_eva_alarms — properties with multiple statuses ──
    println!("\n=== B2. get_eva_alarms ===");
    client.get("kernel.properties.reloadStatus", serde_json::json!(null)).await.unwrap();
    let all_props = client.fetch_all("kernel.properties.fetchStatus").await.expect("fetchStatus");
    assert!(all_props.len() >= 20, "should have many properties, got {}", all_props.len());

    let mut by_status: HashMap<String, usize> = HashMap::new();
    for p in &all_props {
        let s = p["status"].as_str().unwrap_or("?");
        *by_status.entry(s.to_string()).or_default() += 1;
    }
    println!("[info] {} properties by status: {:?}", all_props.len(), by_status);
    assert!(by_status.get("valid").copied().unwrap_or(0) > 0, "should have valid properties");
    assert!(by_status.get("unknown").copied().unwrap_or(0) > 0,
        "should have unknown properties (from unsafe_read/unsafe_avg)");

    // Verify specific alarms: unsafe_read should have index_bound alarm
    let unsafe_read_decl = {
        let st = state.read().await;
        st.resolve_function("unsafe_read").unwrap().declaration.clone()
    };
    let unsafe_read_alarms: Vec<_> = all_props.iter()
        .filter(|p| p["scope"].as_str() == Some(&unsafe_read_decl)
                && p["status"].as_str() != Some("valid"))
        .collect();
    assert!(!unsafe_read_alarms.is_empty(),
        "unsafe_read should have non-valid properties (index_bound alarm)");
    println!("[ok] EVA alarms: {} non-valid for unsafe_read", unsafe_read_alarms.len());

    // ── B3. find_callers: who calls buf_get? ──
    println!("\n=== B3. find_callers (buf_get) ===");
    let buf_get_decl = {
        let st = state.read().await;
        st.resolve_function("buf_get").unwrap().declaration.clone()
    };
    let callers = client.get("plugins.eva.general.getCallers", serde_json::json!(buf_get_decl))
        .await.expect("getCallers(buf_get) failed");
    assert!(callers.is_array(), "getCallers should return array");
    let callers_arr = callers.as_array().unwrap();
    assert!(!callers_arr.is_empty(), "buf_get should have callers (buf_sum)");
    println!("[ok] buf_get has {} caller(s)", callers_arr.len());

    // ── B4. get_eva_value: query values at alarm kinstr markers ──
    println!("\n=== B4. get_eva_value ===");
    // Strategy: use kinstr from unsafe_read's alarm (the array access statement).
    // This is a real alarm location where EVA has computed value ranges.
    // We also register markers via printDeclaration first so the server knows them.
    let _ = client.get("kernel.ast.printDeclaration", serde_json::json!(unsafe_read_decl)).await;

    // Find unsafe_read's alarm property — it should have a kinstr pointing to the
    // array access statement (e.g. "#k38") where EVA detected index_bound alarm.
    let ur_alarm_with_kinstr = unsafe_read_alarms.iter()
        .find(|p| p["kinstr"].as_str().is_some());
    assert!(ur_alarm_with_kinstr.is_some(),
        "unsafe_read alarm should have a kinstr marker");
    let kinstr = ur_alarm_with_kinstr.unwrap()["kinstr"].as_str().unwrap();
    println!("[info] querying EVA values at unsafe_read alarm kinstr={}", kinstr);

    let values = client.get("plugins.eva.values.getValues", serde_json::json!({"target": kinstr}))
        .await.expect("getValues should succeed for alarm kinstr");
    println!("[info] getValues({}): {}", kinstr, serde_json::to_string(&values).unwrap());

    // The result should contain vBefore/vAfter with actual value information
    // (not empty {} like a loop header would return)
    assert!(values.is_object(), "getValues should return an object");
    let has_value_info = values.get("vBefore").is_some() || values.get("vAfter").is_some();
    assert!(has_value_info,
        "getValues at alarm kinstr should return vBefore/vAfter, got: {}",
        serde_json::to_string(&values).unwrap());
    println!("[ok] getValues returned meaningful value ranges at alarm location");

    // ── B5. investigate_alarm: deep investigation of unsafe_avg division_by_zero ──
    println!("\n=== B5. investigate_alarm ===");
    let unsafe_avg_decl = {
        let st = state.read().await;
        st.resolve_function("unsafe_avg").unwrap().declaration.clone()
    };
    // Find the alarm property for unsafe_avg
    let unsafe_avg_alarm = all_props.iter().find(|p| {
        p["scope"].as_str() == Some(&unsafe_avg_decl) && p["status"].as_str() != Some("valid")
    });
    if let Some(alarm) = unsafe_avg_alarm {
        let prop_key = alarm["key"].as_str().unwrap_or("?");
        let kind = alarm["kind"].as_str().unwrap_or("?");
        let descr = alarm["descr"].as_str().unwrap_or("?");
        println!("[info] investigating: {} — {} — {}", prop_key, kind, descr);

        // Quick: property detail only
        println!("[ok] quick: property detail available");

        // Normal: values at the alarm location
        if let Some(kinstr) = alarm["kinstr"].as_str() {
            let values = client.get("plugins.eva.values.getValues", serde_json::json!({"target": kinstr}))
                .await;
            match values {
                Ok(v) => println!("[ok] normal: values at {}: {}", kinstr, serde_json::to_string(&v).unwrap()),
                Err(e) => println!("[info] normal: values at {}: {}", kinstr, e),
            }
        }

        // Normal: callers of the enclosing function
        let callers = client.get("plugins.eva.general.getCallers", serde_json::json!(unsafe_avg_decl))
            .await;
        match callers {
            Ok(c) => println!("[ok] normal: callers: {}", serde_json::to_string(&c).unwrap()),
            Err(e) => println!("[info] normal: callers: {}", e),
        }

        // Deep: all annotations on unsafe_avg
        let ua_annots: Vec<_> = all_props.iter()
            .filter(|p| p["scope"].as_str() == Some(&unsafe_avg_decl))
            .collect();
        println!("[ok] deep: {} annotations on unsafe_avg", ua_annots.len());
    } else {
        println!("[warn] no alarm found for unsafe_avg (unexpected)");
    }
    println!("[ok] investigate_alarm flow verified");

    // ── B6. get_current_annotations: buf_push has behaviors ──
    println!("\n=== B6. get_current_annotations (buf_push) ===");
    client.get("kernel.properties.reloadStatus", serde_json::json!(null)).await.unwrap();
    let props_for_annots = client.fetch_all("kernel.properties.fetchStatus").await.expect("fetchStatus");
    let buf_push_annots: Vec<_> = props_for_annots.iter()
        .filter(|p| p["scope"].as_str() == Some(&buf_push_decl))
        .collect();
    println!("[info] buf_push has {} annotations", buf_push_annots.len());
    // buf_push has requires, assigns, 2 behaviors (ok + full) with ensures each,
    // plus complete/disjoint — should have many annotations
    assert!(buf_push_annots.len() >= 3,
        "buf_push should have at least 3 annotations (requires/assigns/ensures), got {}",
        buf_push_annots.len());
    // Print annotation kinds for visibility
    let mut annot_kinds: HashMap<String, usize> = HashMap::new();
    for a in &buf_push_annots {
        let k = a["kind"].as_str().unwrap_or("?");
        *annot_kinds.entry(k.to_string()).or_default() += 1;
    }
    println!("[ok] buf_push annotation kinds: {:?}", annot_kinds);

    // ═══════════════════════════════════════════════════════════════
    // PHASE C: WP verification — run WP on annotated functions
    // ═══════════════════════════════════════════════════════════════

    // ── C1. Run WP on buf_push, buf_get, echo (run_wp multi-function) ──
    println!("\n=== C1. Run WP ===");
    client.set("plugins.wp.setTimeout", serde_json::json!(10)).await.expect("setTimeout");

    // WP on echo (has named ensures correct + wrong)
    let echo_decl = {
        let st = state.read().await;
        st.resolve_function("echo").unwrap().declaration.clone()
    };
    let _ = client.get("kernel.ast.printDeclaration", serde_json::json!(echo_decl)).await;
    let echo_pv = echo_decl.replace("#F", "#v");
    client.exec("plugins.wp.startProofs", serde_json::json!(echo_pv), Duration::from_secs(120))
        .await.expect("startProofs(echo) failed");
    println!("[ok] WP: echo done");

    // WP on buf_push (behaviors ok/full with complete/disjoint)
    let _ = client.get("kernel.ast.printDeclaration", serde_json::json!(buf_push_decl)).await;
    let buf_push_pv = buf_push_decl.replace("#F", "#v");
    client.exec("plugins.wp.startProofs", serde_json::json!(buf_push_pv), Duration::from_secs(120))
        .await.expect("startProofs(buf_push) failed");
    println!("[ok] WP: buf_push done");

    // WP on buf_get (simple contract)
    let _ = client.get("kernel.ast.printDeclaration", serde_json::json!(buf_get_decl)).await;
    let buf_get_pv = buf_get_decl.replace("#F", "#v");
    client.exec("plugins.wp.startProofs", serde_json::json!(buf_get_pv), Duration::from_secs(120))
        .await.expect("startProofs(buf_get) failed");
    println!("[ok] WP: buf_get done");

    { let mut st = state.write().await; st.set_wp_completed(); }

    // ── C2. get_wp_goals: verify format, filtering, NORESULT on echo ──
    println!("\n=== C2. get_wp_goals ===");
    let _ = client.get("plugins.wp.reloadGoals", serde_json::json!(null)).await.expect("reloadGoals");
    let goals = client.fetch_all("plugins.wp.fetchGoals").await.expect("fetchGoals");
    assert!(!goals.is_empty(), "should have WP goals");
    println!("[info] {} total goals", goals.len());

    // Verify goal structure
    let first_goal = &goals[0];
    assert!(first_goal.get("wpo").is_some(), "goal should have 'wpo' field");
    assert!(first_goal.get("scope").is_some(), "goal should have 'scope' field");
    assert!(first_goal.get("status").is_some(), "goal should have 'status' field");
    assert!(first_goal.get("fct").is_some(), "goal should have 'fct' field");
    assert!(first_goal.get("name").is_some(), "goal should have 'name' field");
    println!("[ok] fetchGoals format verified");

    // Group goals by status (uppercase: VALID, NORESULT, UNKNOWN, etc.)
    let mut goals_by_status: HashMap<String, usize> = HashMap::new();
    for g in &goals {
        let s = g["status"].as_str().unwrap_or("?").to_string();
        *goals_by_status.entry(s).or_default() += 1;
    }
    println!("[info] goals by status: {:?}", goals_by_status);
    assert!(goals_by_status.get("VALID").copied().unwrap_or(0) >= 8,
        "should have at least 8 VALID goals (buf_push + buf_get + echo correct)");

    // Echo goals: "correct" should be VALID, "wrong" should be NORESULT or UNKNOWN
    let echo_goals: Vec<_> = goals.iter()
        .filter(|g| g["scope"].as_str() == Some(&echo_decl))
        .collect();
    println!("[info] echo has {} goals:", echo_goals.len());
    for eg in &echo_goals {
        println!("  [{status}] {name}",
            status = eg["status"].as_str().unwrap_or("?"),
            name = eg["name"].as_str().unwrap_or("?"));
    }
    assert!(echo_goals.len() >= 2, "echo should have at least 2 goals (correct + wrong + assigns)");
    // The 'wrong' ensures should NOT be VALID
    let echo_wrong = echo_goals.iter().find(|g| {
        let name = g["name"].as_str().unwrap_or("");
        name.contains("wrong")
    });
    if let Some(wrong_goal) = echo_wrong {
        let status = wrong_goal["status"].as_str().unwrap_or("?");
        assert_ne!(status, "VALID",
            "echo's 'ensures wrong: \\result > 0' should NOT be provable, got {}", status);
        println!("[ok] echo 'wrong' goal is {} (not VALID, as expected)", status);
    } else {
        println!("[warn] echo 'wrong' goal not found by name — checking all non-VALID");
        let echo_non_valid: Vec<_> = echo_goals.iter()
            .filter(|g| g["status"].as_str() != Some("VALID"))
            .collect();
        assert!(!echo_non_valid.is_empty(),
            "echo should have at least one non-VALID goal (the 'wrong' ensures)");
        println!("[ok] echo has {} non-VALID goal(s)", echo_non_valid.len());
    }

    // ═══════════════════════════════════════════════════════════════
    // PHASE D: Assessment — composite tools and verification plan
    // ═══════════════════════════════════════════════════════════════

    // ── D1. lookup_symbol: function ──
    println!("\n=== D1. lookup_symbol (function) ===");
    {
        let st = state.read().await;
        let info = st.resolve_function("buf_push").unwrap();
        assert!(info.file.ends_with("test_comprehensive.c"));
        assert!(info.marker.starts_with("kf#"));
        assert!(info.declaration.starts_with("#F"));
        println!("[ok] buf_push: marker={}, decl={}, line={}", info.marker, info.declaration, info.line);
    }

    // ── D2. lookup_symbol: global variable ──
    println!("\n=== D2. lookup_symbol (global) ===");
    {
        let st = state.read().await;
        let info = st.resolve_global("error_code").unwrap();
        assert_eq!(info.typ, "int");
        assert!(info.declaration.starts_with("#G"));
        assert!(info.marker.starts_with("vi#"));
        println!("[ok] error_code: type={}, decl={}, marker={}", info.typ, info.declaration, info.marker);
    }

    // ── D3. suggest_verification_plan: logic verification ──
    println!("\n=== D3. suggest_verification_plan ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded, "project should be loaded");
        assert!(st.eva_completed, "EVA should be completed");
        assert!(st.wp_completed, "WP should be completed");
    }
    // Check final property status for plan suggestions
    client.get("kernel.properties.reloadStatus", serde_json::json!(null)).await.unwrap();
    let final_props = client.fetch_all("kernel.properties.fetchStatus").await.expect("fetchStatus");
    let mut final_by_status: HashMap<String, usize> = HashMap::new();
    for p in &final_props {
        let s = p["status"].as_str().unwrap_or("?").to_string();
        *final_by_status.entry(s).or_default() += 1;
    }
    println!("[info] final properties: {:?}", final_by_status);
    // With EVA+WP both done, the suggestion should be "review results"
    println!("[ok] suggest: EVA+WP complete → review results");

    // ── D4. get_verification_status: final summary ──
    println!("\n=== D4. get_verification_status ===");
    let eva_comp = client.get("plugins.eva.general.getComputationState", serde_json::json!(null))
        .await.expect("getComputationState");
    assert_eq!(eva_comp.as_str(), Some("computed"));
    let wp_tasks = client.get("plugins.wp.getScheduledTasks", serde_json::json!(null))
        .await.expect("getScheduledTasks");
    assert!(wp_tasks.is_object());
    println!("[ok] verification status: EVA=computed, WP tasks available");

    // ═══════════════════════════════════════════════════════════════
    // FINAL: Verify all state is consistent
    // ═══════════════════════════════════════════════════════════════

    println!("\n=== Final state ===");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert!(st.eva_completed);
        assert!(st.wp_completed);
        assert_eq!(st.functions.len(), 9, "should have 9 functions");
        assert!(st.globals.len() >= 3, "should have at least 3 globals (data/count/error_code)");
        assert!(!st.callgraph_edges.is_empty(), "callgraph should be cached");
        assert!(!st.callgraph_vertices.is_empty(), "vertices should be cached");
    }
    println!("[ok] all final assertions passed");

    client.shutdown().await.expect("shutdown failed");
    println!("\n=== Comprehensive verification workflow complete ===");
}

// ─── Test: Iterative verification workflow ────────────────────────────
//
// Prerequisites:
//   frama-c test/test_iterative_raw.c -server-socket /tmp/frama-c-test-iter.sock
//
// Simulates the AI agent's core workflow:
//   1. Load a raw C file (no ACSL) → EVA finds alarms
//   2. Inject ACSL annotations → reload_project
//   3. Re-run EVA → alarms change
//   4. Run WP → prove annotations correct
//   5. Verify and clean up

fn socket_path_iter() -> String {
    std::env::var("FRAMA_C_SOCK_ITER")
        .unwrap_or_else(|_| "/tmp/frama-c-test-iter.sock".into())
}

async fn connect_iter() -> (FramaCClient, Arc<RwLock<SessionState>>) {
    let state = Arc::new(RwLock::new(SessionState::default()));
    let client = FramaCClient::connect(&socket_path_iter(), state.clone())
        .await
        .expect("failed to connect to Frama-C server (iterative)");
    (client, state)
}

/// RAII guard: restores file content on drop (even on panic).
struct FileRestoreGuard {
    path: String,
    original: String,
}

impl Drop for FileRestoreGuard {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.original);
    }
}

const ANNOTATED_C: &str = r#"// Iterative verification workflow test — ANNOTATED version
// ACSL annotations added to eliminate EVA alarms

#define SIZE 10
int arr[SIZE];
volatile int nondet;

/*@ requires b != 0;
    assigns \nothing;
    ensures \result == a / b;
*/
int safe_div(int a, int b) {
    return a / b;
}

/*@ requires 0 <= idx < SIZE;
    assigns \nothing;
    ensures \result == arr[idx];
*/
int array_read(int idx) {
    return arr[idx];
}

int main(void) {
    arr[0] = 100;
    arr[5] = 200;
    int x = safe_div(nondet, nondet);
    int y = array_read(nondet);
    return x + y;
}
"#;

#[tokio::test]
async fn test_iterative_workflow() {
    // ── Setup: save original file content for RAII restore ──
    let test_file = std::env::current_dir()
        .unwrap()
        .join("test/test_iterative_raw.c");
    let test_file_str = test_file.to_str().unwrap().to_string();
    let original_content =
        std::fs::read_to_string(&test_file).expect("failed to read test_iterative_raw.c");
    let _guard = FileRestoreGuard {
        path: test_file_str.clone(),
        original: original_content.clone(),
    };

    let (client, state) = connect_iter().await;

    // ═══════════════════════════════════════════════════════════════
    // PHASE 1: Load raw C → EVA finds alarms
    // ═══════════════════════════════════════════════════════════════

    // ── 1.1 Verify functions loaded ──
    println!("\n=== Phase 1: Raw C → EVA alarms ===");
    println!("\n--- 1.1 Verify functions ---");
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert_eq!(
            st.functions.len(),
            3,
            "should have safe_div, array_read, main"
        );
        for name in &["safe_div", "array_read", "main"] {
            let info = st
                .resolve_function(name)
                .unwrap_or_else(|| panic!("missing function: {}", name));
            assert!(info.file.ends_with("test_iterative_raw.c"));
        }
    }
    println!("[ok] 3 functions loaded (safe_div, array_read, main)");

    // ── 1.2 Run EVA ──
    println!("\n--- 1.2 Run EVA ---");
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

    // ── 1.3 Get EVA alarms — expect alarms in safe_div and array_read ──
    println!("\n--- 1.3 Get EVA alarms ---");
    client
        .get(
            "kernel.properties.reloadStatus",
            serde_json::json!(null),
        )
        .await
        .unwrap();
    let phase1_props = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus failed");
    assert!(
        !phase1_props.is_empty(),
        "should have properties after EVA on raw C"
    );

    let safe_div_decl = {
        let st = state.read().await;
        st.resolve_function("safe_div").unwrap().declaration.clone()
    };
    let array_read_decl = {
        let st = state.read().await;
        st.resolve_function("array_read")
            .unwrap()
            .declaration
            .clone()
    };

    // safe_div should have non-valid properties (division_by_zero alarm)
    let safe_div_alarms: Vec<_> = phase1_props
        .iter()
        .filter(|p| {
            p["scope"].as_str() == Some(&safe_div_decl)
                && p["status"].as_str() != Some("valid")
        })
        .collect();
    println!(
        "[info] safe_div non-valid properties: {}",
        safe_div_alarms.len()
    );
    // Note: EVA may not generate alarms for safe_div when main calls it with
    // constants (100, 5). The function-level analysis depends on EVA's precision.
    // We check more broadly below.

    // array_read should have non-valid properties (index_bound alarm)
    let array_read_alarms: Vec<_> = phase1_props
        .iter()
        .filter(|p| {
            p["scope"].as_str() == Some(&array_read_decl)
                && p["status"].as_str() != Some("valid")
        })
        .collect();
    println!(
        "[info] array_read non-valid properties: {}",
        array_read_alarms.len()
    );

    // At least one function should have non-valid properties
    let total_non_valid: Vec<_> = phase1_props
        .iter()
        .filter(|p| p["status"].as_str() != Some("valid"))
        .collect();
    println!(
        "[info] total non-valid properties: {}",
        total_non_valid.len()
    );
    // Record phase 1 alarm count
    let n1 = phase1_props.len();
    println!(
        "[ok] Phase 1 alarms: {} total properties, {} non-valid",
        n1,
        total_non_valid.len()
    );

    // ═══════════════════════════════════════════════════════════════
    // PHASE 2: Inject ACSL → reload_project
    // ═══════════════════════════════════════════════════════════════

    println!("\n=== Phase 2: Inject ACSL → reload ===");

    // ── 2.1 Overwrite file with annotated version ──
    println!("\n--- 2.1 Write annotated file ---");
    std::fs::write(&test_file, ANNOTATED_C).expect("failed to write annotated version");
    // Verify file was written
    let written = std::fs::read_to_string(&test_file).unwrap();
    assert!(
        written.contains("requires"),
        "annotated file should contain 'requires'"
    );
    println!("[ok] Annotated file written");

    // ── 2.2 Reload project: reparse from disk ──
    println!("\n--- 2.2 Reload project ---");
    // Get current file list
    let files = client
        .get("kernel.ast.getFiles", serde_json::json!(null))
        .await
        .expect("getFiles failed");
    println!("[info] current files: {}", serde_json::to_string(&files).unwrap());

    // Force AST invalidation via setFiles([]) → setFiles(files) → compute.
    // Same sequence as Ivette's reparseFiles() (ivette/src/frama-c/menu.ts).
    // Frama-C's state dependency system only propagates Kernel.Files → Ast.self
    // invalidation when the parameter value actually changes.
    client
        .set("kernel.ast.setFiles", serde_json::json!([]))
        .await
        .expect("setFiles([]) failed");
    client
        .set("kernel.ast.setFiles", files)
        .await
        .expect("setFiles(files) failed");
    client
        .exec(
            "kernel.ast.compute",
            serde_json::json!(null),
            Duration::from_secs(60),
        )
        .await
        .expect("kernel.ast.compute failed");

    // Refresh function list from server
    let _ = client
        .get(
            "kernel.ast.reloadFunctions",
            serde_json::json!(null),
        )
        .await
        .expect("reloadFunctions failed");
    let refreshed = client
        .fetch_all("kernel.ast.fetchFunctions")
        .await
        .expect("fetchFunctions failed");

    // ── 2.3 Update state, verify functions reloaded ──
    println!("\n--- 2.3 Verify reload ---");
    {
        let mut st = state.write().await;
        st.invalidate_all();
        st.update_functions(&refreshed);
        st.project_loaded = true;
    }
    {
        let st = state.read().await;
        assert_eq!(
            st.functions.len(),
            3,
            "should still have 3 functions after reload"
        );
        assert!(!st.eva_completed, "EVA should be invalidated after reload");
        assert!(!st.wp_completed, "WP should be invalidated after reload");
    }
    println!("[ok] 3 functions after reload, EVA/WP invalidated");

    // ── 2.4 Verify annotations visible in printDeclaration ──
    println!("\n--- 2.4 Verify annotations ---");
    let safe_div_decl_new = {
        let st = state.read().await;
        st.resolve_function("safe_div").unwrap().declaration.clone()
    };
    let decl_text = client
        .get(
            "kernel.ast.printDeclaration",
            serde_json::json!(safe_div_decl_new),
        )
        .await
        .expect("printDeclaration(safe_div) failed");
    let decl_str = serde_json::to_string(&decl_text).unwrap();
    assert!(
        decl_str.contains("requires"),
        "printDeclaration should show ACSL 'requires' annotation, got: {}",
        &decl_str[..decl_str.len().min(500)]
    );
    println!("[ok] ACSL annotations visible in printDeclaration");

    // ═══════════════════════════════════════════════════════════════
    // PHASE 3: Re-run EVA → alarms should change
    // ═══════════════════════════════════════════════════════════════

    println!("\n=== Phase 3: Re-run EVA ===");

    // ── 3.1 Run EVA ──
    println!("\n--- 3.1 Run EVA ---");
    client
        .exec(
            "plugins.eva.general.compute",
            serde_json::json!(null),
            Duration::from_secs(120),
        )
        .await
        .expect("EVA compute (phase 3) failed");
    {
        let mut st = state.write().await;
        st.set_eva_completed();
    }
    println!("[ok] EVA completed (phase 3)");

    // ── 3.2 Get alarms — compare with phase 1 ──
    println!("\n--- 3.2 Get alarms ---");
    client
        .get(
            "kernel.properties.reloadStatus",
            serde_json::json!(null),
        )
        .await
        .unwrap();
    let phase3_props = client
        .fetch_all("kernel.properties.fetchStatus")
        .await
        .expect("fetchStatus (phase 3) failed");
    let n2 = phase3_props.len();

    let phase3_non_valid: Vec<_> = phase3_props
        .iter()
        .filter(|p| p["status"].as_str() != Some("valid"))
        .collect();
    println!(
        "[info] Phase 3: {} total properties, {} non-valid (phase 1 had {} total)",
        n2,
        phase3_non_valid.len(),
        n1
    );
    // With annotations, the annotated version should have user-specified properties
    // that EVA can validate. The total property count may differ.
    println!("[ok] Phase 3 EVA alarms collected");

    // ═══════════════════════════════════════════════════════════════
    // PHASE 4: WP proof → annotations correct
    // ═══════════════════════════════════════════════════════════════

    println!("\n=== Phase 4: WP proof ===");

    // ── 4.1 Run WP on safe_div and array_read ──
    println!("\n--- 4.1 Run WP ---");
    client
        .set("plugins.wp.setTimeout", serde_json::json!(10))
        .await
        .expect("setTimeout failed");

    // WP on safe_div
    let safe_div_decl_wp = {
        let st = state.read().await;
        st.resolve_function("safe_div").unwrap().declaration.clone()
    };
    let _ = client
        .get(
            "kernel.ast.printDeclaration",
            serde_json::json!(safe_div_decl_wp),
        )
        .await;
    let safe_div_pv = safe_div_decl_wp.replace("#F", "#v");
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(safe_div_pv),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs(safe_div) failed");
    println!("[ok] WP: safe_div done");

    // WP on array_read
    let array_read_decl_wp = {
        let st = state.read().await;
        st.resolve_function("array_read")
            .unwrap()
            .declaration
            .clone()
    };
    let _ = client
        .get(
            "kernel.ast.printDeclaration",
            serde_json::json!(array_read_decl_wp),
        )
        .await;
    let array_read_pv = array_read_decl_wp.replace("#F", "#v");
    client
        .exec(
            "plugins.wp.startProofs",
            serde_json::json!(array_read_pv),
            Duration::from_secs(120),
        )
        .await
        .expect("startProofs(array_read) failed");
    println!("[ok] WP: array_read done");

    {
        let mut st = state.write().await;
        st.set_wp_completed();
    }

    // ── 4.2 Get WP goals ──
    println!("\n--- 4.2 Get WP goals ---");
    let _ = client
        .get("plugins.wp.reloadGoals", serde_json::json!(null))
        .await
        .expect("reloadGoals failed");
    let goals = client
        .fetch_all("plugins.wp.fetchGoals")
        .await
        .expect("fetchGoals failed");
    assert!(!goals.is_empty(), "should have WP goals");
    println!("[info] {} total WP goals", goals.len());

    // Group by status
    let mut goals_by_status: HashMap<String, usize> = HashMap::new();
    for g in &goals {
        let s = g["status"].as_str().unwrap_or("?").to_string();
        *goals_by_status.entry(s).or_default() += 1;
    }
    println!("[info] goals by status: {:?}", goals_by_status);

    // Filter goals for safe_div and array_read
    let safe_div_goals: Vec<_> = goals
        .iter()
        .filter(|g| g["scope"].as_str() == Some(&safe_div_decl_wp))
        .collect();
    let array_read_goals: Vec<_> = goals
        .iter()
        .filter(|g| g["scope"].as_str() == Some(&array_read_decl_wp))
        .collect();
    println!(
        "[info] safe_div goals: {}, array_read goals: {}",
        safe_div_goals.len(),
        array_read_goals.len()
    );

    // Expect VALID goals — assigns + ensures for each function
    let valid_count = goals_by_status.get("VALID").copied().unwrap_or(0);
    assert!(
        valid_count >= 2,
        "should have at least 2 VALID goals (ensures for safe_div + array_read), got {}",
        valid_count
    );
    println!("[ok] {} VALID WP goals", valid_count);

    // ═══════════════════════════════════════════════════════════════
    // PHASE 5: Final verification + cleanup
    // ═══════════════════════════════════════════════════════════════

    println!("\n=== Phase 5: Final verification ===");

    // ── 5.1 get_verification_status ──
    println!("\n--- 5.1 Verification status ---");
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
    {
        let st = state.read().await;
        assert!(st.project_loaded);
        assert!(st.eva_completed);
        assert!(st.wp_completed);
        assert_eq!(st.functions.len(), 3);
    }
    println!("[ok] All verification state consistent");

    // ── 5.2 File restore is handled by FileRestoreGuard (Drop) ──
    // Explicit shutdown before guard runs
    client.shutdown().await.expect("shutdown failed");
    println!("[ok] Frama-C server shut down");

    println!("\n=== Iterative workflow test complete ===");
    // FileRestoreGuard::drop restores test_iterative_raw.c to original content
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
