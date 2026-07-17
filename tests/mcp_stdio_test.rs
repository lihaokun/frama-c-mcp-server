//! End-to-end tests for the ACSL-injecting MCP tools, driven over the real
//! MCP wire protocol (stdio JSON-RPC).
//!
//! Why this exists
//! ---------------
//! Other integration tests in this crate talk directly to the Frama-C server
//! via `FramaCClient`; they bypass the MCP layer entirely. After PR #91 reorg
//! (find_var Kglobal fix + CLI pre-check removal + classify_failure
//! extension), we need to verify the **MCP-visible** behaviour of:
//!   - validate_acsl
//!   - add_annotation_sandbox
//!   - add_annotation_main
//!   - inject_all_annotations_sandbox
//!
//! These tests do NOT make any tool method `pub` — they invoke through
//! `rmcp` client over a real stdio JSON-RPC connection to the spawned server
//! binary (just like Claude Code does in production).
//!
//! Pre-requisites
//! --------------
//! - `cargo build --release` to produce `target/release/frama-c-mcp-server`
//! - `frama-c` on PATH (CI sets `export PATH="$(opam var bin):$PATH"`),
//!   or override via `FRAMA_C_BIN` env var
//! - ast_utils_plugin installed (`cd ast-utils && dune install`)
//!
//! Each test spawns its own MCP server (which in turn spawns its own
//! frama-c subprocess) on a unique socket path, so tests can run in
//! parallel without socket collisions.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, RawContent};
use rmcp::service::ServiceExt;
use rmcp::transport::TokioChildProcess;
use serde_json::{json, Value};
use tokio::process::Command;

// ──────────────────────────────────────────────────────────────────────────
// Harness
// ──────────────────────────────────────────────────────────────────────────

/// Workspace-relative path resolver.
fn workspace_path(rel: &str) -> PathBuf {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR not set");
    PathBuf::from(crate_dir).join(rel)
}

/// Spawn the MCP server directly (lazy mode, Issue #95) and connect over stdio.
///
/// 不再走 launch-mcp.sh wrapper —— 直接 exec binary。MCP server 启动时
/// **不连任何 frama-c**，第一次 reload_project tool 调用时才 spawn frama-c。
/// `c_file` 必须由 caller 之后 reload_project 进去（startup 时不预 load）。
async fn spawn_mcp_client(c_file: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let binary = workspace_path("target/release/frama-c-mcp-server");
    if !binary.exists() {
        panic!(
            "MCP binary missing: {}\nRun `cargo build --release` first.",
            binary.display()
        );
    }
    let frama_c = std::env::var("FRAMA_C_BIN").unwrap_or_else(|_| "frama-c".into());

    let mut cmd = Command::new(&binary);
    cmd.arg("--frama-c").arg(&frama_c);
    cmd.stderr(std::process::Stdio::inherit());

    let transport = TokioChildProcess::new(cmd)
        .expect("failed to spawn MCP server child process");
    let client = ()
        .serve(transport)
        .await
        .expect("failed to initialize MCP client handshake");

    // Lazy 模式：caller 期望 c_file 已 loaded，这里代为调 reload_project
    // 保持旧 spawn_mcp_client 语义（兼容 existing 11 个测试）。
    call_tool_json(
        &client,
        "reload_project",
        serde_json::json!({ "files": [c_file], "rte": false }),
    )
    .await
    .expect("reload_project failed in spawn helper");

    client
}

async fn raw_call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: Value,
) -> Result<CallToolResult, String> {
    let args_obj = match args {
        Value::Object(m) => m,
        _ => return Err(format!("tool args must be JSON object, got {:?}", args)),
    };
    client
        .call_tool(
            CallToolRequestParams::new(name.to_string())
                .with_arguments(args_obj),
        )
        .await
        .map_err(|e| format!("tool call '{}' failed: {}", name, e))
}

/// Call a tool returning a JSON payload (most tools).
async fn call_tool_json(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    let r = raw_call(client, name, args).await?;
    Ok(payload_json(&r))
}

/// Call a tool returning plain text (e.g. print_source_*).
async fn call_tool_text(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: Value,
) -> Result<String, String> {
    let r = raw_call(client, name, args).await?;
    Ok(payload_text(&r))
}

/// Concatenate all text content from a CallToolResult.
fn payload_text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the text payload as JSON. The OCaml plugin wraps its responses in a
/// `{"result": <inner>}` envelope; Rust handlers either pass through (e.g.
/// validate_acsl) or augment (e.g. add_annotation_* injects `hash_label` at
/// the outer level). Unwrap once when an outer `result` key is the only
/// non-augmented field so tests can write `r["valid"]` / `r["success"]`
/// uniformly. Otherwise return as-is.
fn payload_json(r: &CallToolResult) -> Value {
    let text = payload_text(r);
    let parsed: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("non-JSON payload: <<{}>> -- {}", text, e));
    unwrap_ocaml_result(parsed)
}

/// If the value is a top-level object containing a `result` key, return the
/// inner result merged with any sibling keys (e.g. `hash_label` added by
/// add_annotation_impl). Otherwise return value as-is.
fn unwrap_ocaml_result(v: Value) -> Value {
    if let Value::Object(mut top) = v {
        if let Some(Value::Object(inner)) = top.remove("result") {
            let mut merged = inner;
            for (k, val) in top {
                merged.entry(k).or_insert(val);
            }
            return Value::Object(merged);
        }
        return Value::Object(top);
    }
    v
}

fn bubble_sort_c() -> PathBuf {
    workspace_path("test/bubble_sort.c")
}

fn factorial_c() -> PathBuf {
    workspace_path("test/factorial.c")
}

fn binary_search_c() -> PathBuf {
    workspace_path("test/binary_search.c")
}

// ──────────────────────────────────────────────────────────────────────────
// Test 1: validate_acsl rejects broken funspec and accepts valid
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn validate_acsl_broken_local_and_behavior_wrap_and_valid() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    // Plain broken: funspec referencing locals
    let r = call_tool_json(&client, "validate_acsl", json!({
        "function": "bubble_sort",
        "kind": "spec",
        "acsl": "assigns *(a+(0..n-1)), i, tmp;",
    })).await.unwrap();
    assert_eq!(r["valid"], Value::Bool(false), "plain broken: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("function local"),
        "plain broken error: {:?}", r["error"]);

    // Behavior-wrapped broken
    let r = call_tool_json(&client, "validate_acsl", json!({
        "function": "bubble_sort",
        "kind": "spec",
        "acsl": "behavior b1: assumes n > 0; assigns *(a+(0..n-1)), i, tmp;",
    })).await.unwrap();
    assert_eq!(r["valid"], Value::Bool(false), "behavior broken: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("function local"));

    // Valid: should pass
    let r = call_tool_json(&client, "validate_acsl", json!({
        "function": "bubble_sort",
        "kind": "spec",
        "acsl": "requires n >= 0; assigns *(a+(0..n-1));",
    })).await.unwrap();
    assert_eq!(r["valid"], Value::Bool(true), "valid case: {:?}", r);

    // Undef logic predicate
    let r = call_tool_json(&client, "validate_acsl", json!({
        "function": "bubble_sort",
        "kind": "spec",
        "acsl": "requires unknown_pred(n);",
    })).await.unwrap();
    assert_eq!(r["valid"], Value::Bool(false), "undef pred: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("unbound logic predicate"));

    let _ = client.cancel().await;
}

#[tokio::test]
async fn create_sandbox_keeps_unused_static_target_function() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let c_file = tmp.path().join("static_helper.c");
    std::fs::write(
        &c_file,
        r#"
#include <stdint.h>

typedef struct {
    uint8_t *buf;
    int count;
} ring_buffer_t;

static int rb_is_empty(const ring_buffer_t *rb)
{
    return rb->count == 0;
}

int dev_read(ring_buffer_t *rb)
{
    return rb_is_empty(rb);
}
"#,
    )
    .expect("write fixture");

    let client = spawn_mcp_client(c_file.to_str().unwrap()).await;

    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "rb_is_empty",
        "experiment_id": "test_static_keep",
    }))
    .await
    .unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();
    assert_eq!(sb_name, "test_static_keep:rb_is_empty");

    let ast = call_tool_json(&client, "get_function_ast", json!({
        "function": &sb_name,
    }))
    .await
    .unwrap();
    assert!(
        ast.get("error").is_none(),
        "static target disappeared from sandbox AST: {:?}",
        ast
    );
    assert!(
        ast.get("body").and_then(|v| v.as_array()).is_some(),
        "expected function body in sandbox AST: {:?}",
        ast
    );

    let src = call_tool_text(&client, "print_source_sandbox", json!({
        "sandbox_name": &sb_name,
    }))
    .await
    .unwrap();
    assert!(src.contains("rb_is_empty"), "sandbox source lost target: {}", src);
    assert!(src.contains("ring_buffer_t"), "sandbox source lost target type: {}", src);

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// Test 2: add_annotation_sandbox rejects broken; AST stays clean
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn add_annotation_sandbox_payload_and_ast_consistency() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    // Create sandbox
    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "bubble_sort",
        "experiment_id": "test_aas",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();
    assert_eq!(sb_name, "test_aas:bubble_sort");

    // Plain broken — payload success=false
    let r = call_tool_json(&client, "add_annotation_sandbox", json!({
        "function": &sb_name,
        "kind": "spec",
        "acsl": "assigns *(a+(0..n-1)), i, tmp;",
        "user_label": "plain_broken",
    })).await.unwrap();
    assert_eq!(r["success"], Value::Bool(false), "plain broken: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("function local"));

    // Behavior-wrapped broken
    let r = call_tool_json(&client, "add_annotation_sandbox", json!({
        "function": &sb_name,
        "kind": "spec",
        "acsl": "behavior bad: assumes n > 0; assigns *(a+(0..n-1)), i, tmp;",
        "user_label": "bhv_broken",
    })).await.unwrap();
    assert_eq!(r["success"], Value::Bool(false), "bhv broken: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("function local"));

    // Valid spec — should land
    let r = call_tool_json(&client, "add_annotation_sandbox", json!({
        "function": &sb_name,
        "kind": "spec",
        "acsl": "requires n >= 0; assigns *(a+(0..n-1));",
        "user_label": "valid",
    })).await.unwrap();
    assert_eq!(r["success"], Value::Bool(true), "valid case: {:?}", r);

    // Inspect AST: the broken labels / behavior name MUST NOT appear; valid SHOULD.
    let src = call_tool_text(&client, "print_source_sandbox", json!({
        "sandbox_name": &sb_name,
    })).await.unwrap();
    assert!(!src.contains("plain_broken"), "plain_broken label leaked into AST: src={}", src);
    assert!(!src.contains("bhv_broken"), "bhv_broken label leaked into AST");
    assert!(!src.contains("behavior bad"), "broken behavior leaked into AST");
    assert!(src.contains("valid"), "valid label missing from AST");

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// Test 3: add_annotation_main — same rejection path on main instance
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn add_annotation_main_rejects_broken() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    // Note: function name MUST NOT include ':' for main-instance path.
    let r = call_tool_json(&client, "add_annotation_main", json!({
        "function": "bubble_sort",
        "kind": "spec",
        "acsl": "assigns *(a+(0..n-1)), i, tmp;",
        "user_label": "main_broken",
    })).await.unwrap();
    assert_eq!(r["success"], Value::Bool(false));
    assert!(r["error"].as_str().unwrap_or("").contains("function local"),
        "main broken: {:?}", r);

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// Test 4: inject_all_annotations_sandbox — mixed batch classification
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inject_all_classifies_failures_correctly() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    // Create sandbox
    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "bubble_sort",
        "experiment_id": "test_iall",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();

    // Mixed input: valid require + undef predicate require + valid ensure + broken assigns
    // Schema v2: proposed_assigns is now Vec<{acsl, behavior?}>.
    let r = call_tool_json(&client, "inject_all_annotations_sandbox", json!({
        "sandbox_name": &sb_name,
        "proposed_requires": [
            {"acsl": "n >= 0", "necessity": "valid"},
            {"acsl": "unknown_pred(n)", "necessity": "undef pred"},
        ],
        "proposed_ensures": [
            {"acsl": "\\forall integer k; 0 <= k < n ==> a[k] == a[k]", "from": "trivial"},
        ],
        "proposed_assigns": [
            {"acsl": "*(a+(0..n-1)), i, tmp"}
        ],
    })).await.unwrap();

    assert_eq!(r["status"].as_str().unwrap_or(""), "proposed_error",
        "status: {:?}", r);

    let summary = &r["summary"];
    assert_eq!(summary["total_attempted"].as_u64().unwrap(), 4);
    assert_eq!(summary["successful_count"].as_u64().unwrap(), 2,
        "expected 2 successful (valid req + valid ens); got {:?}", r);
    assert_eq!(summary["failure_count"].as_u64().unwrap(), 2);

    let failures = r["failures"].as_array().expect("failures not array");
    let types: Vec<String> = failures
        .iter()
        .map(|f| f["type"].as_str().unwrap_or("?").to_string())
        .collect();

    assert!(types.contains(&"proposed_self_referential".to_string()),
        "missing proposed_self_referential in {:?}", types);
    assert!(types.contains(&"proposed_local_var_in_funspec".to_string()),
        "missing proposed_local_var_in_funspec in {:?}", types);

    // Verify each failure carries the correct proposed_path → type mapping
    for f in failures {
        let path = f["proposed_path"].as_str().unwrap_or("");
        let ftype = f["type"].as_str().unwrap_or("");
        match path {
            "proposed_requires[1]" =>
                assert_eq!(ftype, "proposed_self_referential",
                    "undef pred at path 'proposed_requires[1]': got {}", ftype),
            "proposed_assigns[0]" =>
                assert_eq!(ftype, "proposed_local_var_in_funspec",
                    "broken assigns at 'proposed_assigns[0]': got {}", ftype),
            other => panic!("unexpected failure path: {}", other),
        }
    }

    // Verify successful entries actually landed in AST.
    // frama-c renders `n >= 0` as `n ≥ 0` (unicode); check either.
    let src = call_tool_text(&client, "print_source_sandbox", json!({
        "sandbox_name": &sb_name,
    })).await.unwrap();
    assert!(
        src.contains("requires") && (src.contains("n ≥ 0") || src.contains("n >= 0")),
        "valid requires missing from sandbox AST; src={}",
        src
    );
    // Broken specs MUST NOT appear
    assert!(!src.contains("unknown_pred"), "undef pred leaked into AST");
    assert!(!src.contains(", i, tmp"), "broken assigns leaked into AST");

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// Test 5: invalid sandbox_name format → MCP error
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inject_all_rejects_missing_experiment_id_prefix() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    // Direct call_tool — expect Err from the MCP layer (invalid_params).
    let res = client
        .call_tool(
            CallToolRequestParams::new("inject_all_annotations_sandbox")
                .with_arguments(
                    json!({
                        "sandbox_name": "bubble_sort",  // missing prefix
                        "proposed_requires": [],
                    }).as_object().unwrap().clone(),
                ),
        )
        .await;

    // The tool returns Err(McpError) for invalid_params.
    let err = res.expect_err("expected error for missing experiment_id prefix");
    let msg = format!("{}", err);
    assert!(msg.contains("experiment_id") || msg.contains("prefix"),
        "error msg should mention experiment_id/prefix; got: {}", msg);

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// Test 6: empty input → status=success, 0 attempted
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inject_all_empty_input_is_no_op_success() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "bubble_sort",
        "experiment_id": "test_empty",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();

    let r = call_tool_json(&client, "inject_all_annotations_sandbox", json!({
        "sandbox_name": &sb_name,
        "proposed_requires": null,
        "proposed_ensures": null,
        "proposed_assigns": null,
        "proposed_loop_annots": null,
    })).await.unwrap();

    assert_eq!(r["status"].as_str().unwrap(), "success");
    assert_eq!(r["summary"]["total_attempted"].as_u64().unwrap(), 0);
    assert_eq!(r["summary"]["successful_count"].as_u64().unwrap(), 0);
    assert_eq!(r["summary"]["failure_count"].as_u64().unwrap(), 0);

    // Belt-and-braces: arrays should be present and empty.
    assert_eq!(r["successful"].as_array().unwrap().len(), 0);
    assert_eq!(r["failures"].as_array().unwrap().len(), 0);

    let _ = tokio::time::timeout(Duration::from_secs(2), client.cancel()).await;
}

// ──────────────────────────────────────────────────────────────────────────
// Test 7: PR #91 original case — `assigns i, j, tmp, a[0..n-1];` on
// bubble_sort, exercised through all 4 ACSL-injecting MCP tools to prove
// the find_var Kglobal fix lands at every entrypoint.
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pr91_original_case_across_all_four_tools() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    // The exact ACSL string from PR #91 description's problem A repro.
    const BROKEN: &str = "assigns i, j, tmp, a[0..n-1];";

    // ── 1. validate_acsl ──
    let r = call_tool_json(&client, "validate_acsl", json!({
        "function": "bubble_sort",
        "kind": "spec",
        "acsl": BROKEN,
    })).await.unwrap();
    assert_eq!(r["valid"], Value::Bool(false), "validate_acsl: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("function local"));

    // ── 2. add_annotation_main ──
    let r = call_tool_json(&client, "add_annotation_main", json!({
        "function": "bubble_sort",
        "kind": "spec",
        "acsl": BROKEN,
        "user_label": "pr91_main",
    })).await.unwrap();
    assert_eq!(r["success"], Value::Bool(false), "add_annotation_main: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("function local"));

    // ── 3. add_annotation_sandbox ──
    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "bubble_sort",
        "experiment_id": "pr91_orig",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();

    let r = call_tool_json(&client, "add_annotation_sandbox", json!({
        "function": &sb_name,
        "kind": "spec",
        "acsl": BROKEN,
        "user_label": "pr91_sandbox",
    })).await.unwrap();
    assert_eq!(r["success"], Value::Bool(false), "add_annotation_sandbox: {:?}", r);
    assert!(r["error"].as_str().unwrap_or("").contains("function local"));

    // ── 4. inject_all_annotations_sandbox ──
    // Schema v2: proposed_assigns is Vec<{acsl, behavior?}>.
    let r = call_tool_json(&client, "inject_all_annotations_sandbox", json!({
        "sandbox_name": &sb_name,
        // The broken assigns is the ONLY entry; tests that the single-entry
        // failure path classifies correctly.
        "proposed_assigns": [
            {"acsl": "i, j, tmp, a[0..n-1]"}
        ],
    })).await.unwrap();
    assert_eq!(r["status"].as_str().unwrap(), "proposed_error");
    let failures = r["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["type"].as_str().unwrap(),
        "proposed_local_var_in_funspec",
        "inject_all classification: {:?}", failures[0]);
    assert!(failures[0]["frama_c_error"].as_str().unwrap_or("")
        .contains("function local"));

    // ── 5. AST cleanliness on BOTH instances ──
    // Main instance source — neither label nor broken assigns should appear
    let main_src = call_tool_text(&client, "print_source_main", json!({})).await.unwrap();
    assert!(!main_src.contains("pr91_main"),
        "main AST polluted by add_annotation_main attmpt");
    assert!(!main_src.contains(", i, j, tmp"),
        "main AST contains broken assigns content");

    // Sandbox source
    let sb_src = call_tool_text(&client, "print_source_sandbox", json!({
        "sandbox_name": &sb_name,
    })).await.unwrap();
    assert!(!sb_src.contains("pr91_sandbox"),
        "sandbox AST polluted by add_annotation_sandbox attmpt");
    assert!(!sb_src.contains(", i, j, tmp"),
        "sandbox AST contains broken assigns content (inject_all leak)");

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// Test 8: Schema v2 — proposed_behaviors + behavior references across
// requires / ensures / assigns. Verifies the assumes-once declaration is
// shared, undeclared references error gracefully, and printSource shows
// the merged behavior block.
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inject_all_schema_v2_behaviors_and_undeclared_reference() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "bubble_sort",
        "experiment_id": "v2_bhv",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();

    let r = call_tool_json(&client, "inject_all_annotations_sandbox", json!({
        "sandbox_name": &sb_name,
        // Declare 1 behavior; reference it from 2 clauses; reference an
        // undeclared behavior from 1 clause (should fail with ProposedError).
        "proposed_behaviors": [
            {"name": "v2nonneg", "assumes": ["n >= 0"]}
        ],
        "proposed_requires": [
            {"acsl": "n >= 0", "necessity": "always required"},
            {"acsl": "n <= 1000", "behavior": "v2nonneg", "necessity": "for v2nonneg only"}
        ],
        "proposed_ensures": [
            // Reference declared behavior — should land in AST.
            // bubble_sort returns void, so we can't use \result; use \old(n).
            {"acsl": "n == \\old(n)", "from": "v2_test", "behavior": "v2nonneg"},
            // Reference undeclared behavior — should fail at plan building
            {"acsl": "\\true", "from": "bug_test", "behavior": "undeclared_bhv"}
        ],
        "proposed_assigns": [
            // Plain assigns at top level
            {"acsl": "a[0..n-1]"}
        ]
    })).await.unwrap();

    assert_eq!(r["status"].as_str().unwrap(), "proposed_error",
        "expected proposed_error due to undeclared bhv ref; got: {:?}", r);

    let summary = &r["summary"];
    assert_eq!(summary["total_attempted"].as_u64().unwrap(), 5,
        "5 entries: 2 req + 2 ens + 1 assigns");
    assert_eq!(summary["failure_count"].as_u64().unwrap(), 1,
        "exactly 1 failure (the undeclared bhv ref)");
    assert_eq!(summary["successful_count"].as_u64().unwrap(), 4);

    // Locate the undeclared behavior failure
    let failures = r["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    let f = &failures[0];
    assert_eq!(f["proposed_path"].as_str().unwrap(), "proposed_ensures[1]");
    let err = f["frama_c_error"].as_str().unwrap_or("");
    assert!(err.contains("'undeclared_bhv'"),
        "error should mention undeclared name: {}", err);
    assert!(err.contains("not declared in proposed_behaviors"),
        "error should explain the rule: {}", err);

    // AST verification: declared behavior clauses landed; undeclared did not
    let src = call_tool_text(&client, "print_source_sandbox", json!({
        "sandbox_name": &sb_name,
    })).await.unwrap();
    assert!(src.contains("v2nonneg"),
        "v2nonneg behavior should appear in AST; src={}", src);
    assert!(src.contains("n ≥ 0") || src.contains("n >= 0"),
        "valid requires missing");
    assert!(!src.contains("undeclared_bhv"),
        "undeclared behavior must not leak into AST");

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// Benchmark E2E (schema v2): exercise inject_all on real benchmark functions
// (factorial / binary_search / bubble_sort) with realistic proposed_* shaped
// like what S2.5 would emit. Verifies:
//   - schema v2 input round-trip (typed Vec<ProposedX> deserialization)
//   - all clauses including loop annots land in AST
//   - sandbox sids correctly referenced for loop annotations
//   - status==success when all entries valid
// ──────────────────────────────────────────────────────────────────────────

/// Helper: discover loop stmt_ids in a sandbox function via get_function_ast.
async fn find_loop_sids(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    sandbox_name: &str,
) -> Vec<i64> {
    let r = call_tool_json(client, "get_function_ast", json!({
        "function": sandbox_name,
    })).await.expect("get_function_ast failed");
    let body = r.get("body").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut sids = Vec::new();
    fn walk(arr: &[Value], sids: &mut Vec<i64>) {
        for s in arr {
            if !s.is_object() { continue; }
            if s.get("kind").and_then(|x| x.as_str()) == Some("loop") {
                if let Some(sid) = s.get("sid").and_then(|x| x.as_i64()) {
                    sids.push(sid);
                }
            }
            for k in ["body", "stmts", "then_body", "else_body"] {
                if let Some(arr2) = s.get(k).and_then(|x| x.as_array()) {
                    walk(arr2, sids);
                }
            }
        }
    }
    walk(&body, &mut sids);
    sids
}

#[tokio::test]
async fn benchmark_factorial_full_spec_via_inject_all() {
    let client = spawn_mcp_client(factorial_c().to_str().unwrap()).await;

    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "factorial",
        "experiment_id": "bench_fact",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();

    let sids = find_loop_sids(&client, &sb_name).await;
    assert_eq!(sids.len(), 1, "factorial should have 1 loop; got {:?}", sids);
    let loop_sid = sids[0];

    let r = call_tool_json(&client, "inject_all_annotations_sandbox", json!({
        "sandbox_name": &sb_name,
        // schema v2 — all fields use Vec<typed>
        "proposed_behaviors": [],
        "proposed_requires": [
            {"acsl": "n >= 0", "necessity": "factorial undefined for negative n"}
        ],
        "proposed_ensures": [
            {"acsl": "\\result >= 1", "from": "loop invariant f >= 1 carried to exit"}
        ],
        "proposed_assigns": [
            {"acsl": "\\nothing"}
        ],
        "proposed_loop_annots": [
            {
                "stmt_id": loop_sid,
                "loop_label": "main loop",
                "invariants": [
                    {"acsl": "1 <= i <= n + 1"},
                    {"acsl": "f >= 1"}
                ],
                "assigns": [{"acsl": "f, i"}],
                "variant": {"acsl": "n + 1 - i"}
            }
        ]
    })).await.unwrap();

    let status = r["status"].as_str().unwrap_or("");
    let summary = &r["summary"];
    // Expected entries: 1 req + 1 ens + 1 assigns + 2 inv + 1 lassigns + 1 lvariant = 7
    assert_eq!(summary["total_attempted"].as_u64().unwrap(), 7,
        "expected 7 entries; got summary={:?}", summary);
    assert_eq!(status, "success",
        "expected status=success; got status={} failures={:?}",
        status, r["failures"]);
    assert_eq!(summary["successful_count"].as_u64().unwrap(), 7);
    assert_eq!(summary["failure_count"].as_u64().unwrap(), 0);

    // AST sanity: requires/ensures/loop invariants all present
    let src = call_tool_text(&client, "print_source_sandbox", json!({
        "sandbox_name": &sb_name,
    })).await.unwrap();
    assert!(src.contains("requires"), "no requires in AST; src={}", src);
    assert!(src.contains("ensures"), "no ensures in AST");
    assert!(src.contains("loop invariant"), "no loop invariant in AST");
    assert!(src.contains("loop variant"), "no loop variant in AST");

    let _ = client.cancel().await;
}

#[tokio::test]
async fn benchmark_binary_search_full_spec_via_inject_all() {
    let client = spawn_mcp_client(binary_search_c().to_str().unwrap()).await;

    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "binary_search",
        "experiment_id": "bench_bs",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();

    let sids = find_loop_sids(&client, &sb_name).await;
    assert_eq!(sids.len(), 1, "binary_search should have 1 loop; got {:?}", sids);
    let loop_sid = sids[0];

    let r = call_tool_json(&client, "inject_all_annotations_sandbox", json!({
        "sandbox_name": &sb_name,
        "proposed_behaviors": [],
        "proposed_requires": [
            {"acsl": "n >= 0", "necessity": "array length non-negative"},
            {"acsl": "\\valid_read(a + (0..n-1))", "necessity": "read-only array bounds"}
        ],
        "proposed_ensures": [
            {"acsl": "\\result == -1 || (0 <= \\result < n && a[\\result] == x)",
             "from": "binary search postcondition"}
        ],
        "proposed_assigns": [
            {"acsl": "\\nothing"}
        ],
        "proposed_loop_annots": [
            {
                "stmt_id": loop_sid,
                "loop_label": "binary search loop",
                "invariants": [
                    {"acsl": "-1 <= low"},
                    {"acsl": "high <= n"}
                ],
                "assigns": [{"acsl": "low, high"}],
                "variant": {"acsl": "high - low"}
            }
        ]
    })).await.unwrap();

    let status = r["status"].as_str().unwrap_or("");
    let summary = &r["summary"];
    // 2 req + 1 ens + 1 assigns + 2 inv + 1 lassigns + 1 lvariant = 8
    assert_eq!(summary["total_attempted"].as_u64().unwrap(), 8,
        "expected 8 entries; got summary={:?}", summary);
    assert_eq!(status, "success",
        "expected status=success; got status={} failures={:?}",
        status, r["failures"]);

    let _ = client.cancel().await;
}

#[tokio::test]
async fn benchmark_bubble_sort_with_named_behavior_via_inject_all() {
    let client = spawn_mcp_client(bubble_sort_c().to_str().unwrap()).await;

    let r = call_tool_json(&client, "create_sandbox", json!({
        "function": "bubble_sort",
        "experiment_id": "bench_bb",
    })).await.unwrap();
    let sb_name = r["sandbox_name"].as_str().unwrap().to_string();

    let sids = find_loop_sids(&client, &sb_name).await;
    assert_eq!(sids.len(), 2, "bubble_sort should have 2 loops; got {:?}", sids);
    let outer_sid = sids[0];
    let inner_sid = sids[1];

    let r = call_tool_json(&client, "inject_all_annotations_sandbox", json!({
        "sandbox_name": &sb_name,
        // Exercise named behavior: nonempty case requires valid pointer.
        // n <= 0 → function early-returns (no behavior precondition).
        "proposed_behaviors": [
            {"name": "nonempty", "assumes": ["n > 0"]}
        ],
        "proposed_requires": [
            // Top-level requires: array bounds (always)
            {"acsl": "n >= 0", "necessity": "non-negative size"},
            // Behavior-scoped requires: valid pointer only when n > 0
            {"acsl": "\\valid(a + (0..n-1))", "behavior": "nonempty",
             "necessity": "pointer must be valid when array non-empty"}
        ],
        "proposed_ensures": [
            {"acsl": "\\true", "from": "trivial top-level postcondition"}
        ],
        "proposed_assigns": [
            // Behavior-scoped assigns: only modifies array when non-empty
            {"acsl": "a[0..n-1]", "behavior": "nonempty"}
        ],
        "proposed_loop_annots": [
            {
                "stmt_id": outer_sid,
                "loop_label": "outer loop",
                "invariants": [{"acsl": "0 <= i <= n - 1"}],
                "assigns": [{"acsl": "a[0..n-1], i, j, tmp"}],
                "variant": {"acsl": "i"}
            },
            {
                "stmt_id": inner_sid,
                "loop_label": "inner loop",
                "invariants": [{"acsl": "0 <= j <= i"}],
                "assigns": [{"acsl": "a[0..n-1], j, tmp"}],
                "variant": {"acsl": "i - j"}
            }
        ]
    })).await.unwrap();

    let status = r["status"].as_str().unwrap_or("");
    let summary = &r["summary"];
    // Entries: 2 req + 1 ens + 1 assigns + (1 inv + 1 lassigns + 1 lvariant) × 2 = 10
    assert_eq!(summary["total_attempted"].as_u64().unwrap(), 10,
        "expected 10 entries; got summary={:?}", summary);
    assert_eq!(status, "success",
        "expected status=success; got status={} failures={:?}",
        status, r["failures"]);

    // Verify the named behavior surfaced in AST.
    let src = call_tool_text(&client, "print_source_sandbox", json!({
        "sandbox_name": &sb_name,
    })).await.unwrap();
    assert!(src.contains("behavior nonempty"),
        "named behavior should appear in AST; src={}", src);
    assert!(src.contains("assumes"), "behavior assumes missing from AST");

    let _ = client.cancel().await;
}

// ──────────────────────────────────────────────────────────────────────────
// fsmint-3: get_ready_functions 功能测试（端到端 MCP wire，验工具接线 +
// callgraph 取数 + 序列化；纯函数算法层另由 topo.rs 单测覆盖 INV2/3/4）
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_ready_functions_chain_and_inprogress() {
    // mini_no_recursion: a → b → c（c 叶子，无 callee）
    let c_file = workspace_path("test/mini_no_recursion.c");
    let client = spawn_mcp_client(c_file.to_str().unwrap()).await;

    // ready 函数名集合（从返回的 [ReadyFunc] JSON array 抽 function 字段，排序）
    fn ready_names(v: &Value) -> Vec<String> {
        let mut ns: Vec<String> = v
            .as_array()
            .unwrap_or_else(|| panic!("get_ready_functions should return JSON array, got {:?}", v))
            .iter()
            .map(|f| f["function"].as_str().unwrap().to_string())
            .collect();
        ns.sort();
        ns
    }

    // done={} → ready={c}（唯一叶子）
    let r = call_tool_json(&client, "get_ready_functions",
        json!({"done": [], "in_progress": []})).await.unwrap();
    assert_eq!(ready_names(&r), vec!["c"], "empty done → only leaf c: {:?}", r);

    // done={c} → ready={b}（b 的唯一 callee c 已 merge）
    let r = call_tool_json(&client, "get_ready_functions",
        json!({"done": ["c"], "in_progress": []})).await.unwrap();
    assert_eq!(ready_names(&r), vec!["b"], "done={{c}} → b: {:?}", r);

    // done={b,c} → ready={a}
    let r = call_tool_json(&client, "get_ready_functions",
        json!({"done": ["b", "c"], "in_progress": []})).await.unwrap();
    assert_eq!(ready_names(&r), vec!["a"], "done={{b,c}} → a: {:?}", r);

    // done={c}, in_progress={b} → ready={}（a 的 callee b 未 done；b 在跑被排除）
    let r = call_tool_json(&client, "get_ready_functions",
        json!({"done": ["c"], "in_progress": ["b"]})).await.unwrap();
    assert!(ready_names(&r).is_empty(), "in_progress excludes b, a not ready: {:?}", r);

    // 序列化形状：c 非 SCC → is_cycle=false, scc_id=null, scc_members=[c]
    let r = call_tool_json(&client, "get_ready_functions",
        json!({"done": [], "in_progress": []})).await.unwrap();
    let c_entry = &r.as_array().unwrap()[0];
    assert_eq!(c_entry["function"], "c");
    assert_eq!(c_entry["is_cycle"], Value::Bool(false));
    assert!(c_entry["scc_id"].is_null(), "non-SCC scc_id should be null: {:?}", c_entry);
    assert_eq!(c_entry["scc_members"], json!(["c"]));

    let _ = client.cancel().await;
}
