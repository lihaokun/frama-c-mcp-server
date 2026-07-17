//! Integration tests for Plan A — long-text fields **only on disk**, not in state.
//! See docs/fixes/conclusion-per-field-files.md.
//!
//! Plan A architectural invariants tested here:
//! - 长文本字段（semantic_proof / semiformal_proof / program_summary）
//!   不在 FunctionVerificationState 内存结构里——struct 物理上不含这些字段
//! - persist_conclusion 只写 meta.json，不碰 .md 文件（不可能误删 LLM 的 .md）
//! - 长文本写文件由 write_long_text_field 显式驱动（MCP store handler 调用）
//! - 长文本读 = 直接 read_long_texts_as_json 从 disk 组装
//!
//! 注：`analysis_summary` 历史上是第 4 个长文本字段，2026-05-26 因撞 CC subagent
//! guard 删除，内容并入 semiformal_proof.md 的 `## function_summary` section
//! （见 docs/fixes/rename-analysis-summary-subagent-guard.md）。
//!
//! 测试覆盖 7 个 invariant：
//! 1. write_long_text_field round-trip (3 字段)
//! 2. persist_conclusion 不写长文本 .md
//! 3. write_long_text_field 空 content = no-op（保护直接 Write）
//! 4. 直接 fs::write → read_long_texts_as_json 看见；analysis_summary 已删（防回退）
//! 5. 旧 <func>.json 文件被 load_conclusions_from_disk 忽略
//! 6. meta.json 不含长文本 key（含 analysis_summary 负断言）；manifest 只列存在文件
//! 7. handler 工作流端到端：长文本 + 短字段

use frama_c_mcp_server::mcp::server::{
    load_conclusion_dir, load_conclusions_from_disk, persist_conclusion_at,
    read_long_texts_as_json, write_long_text_field,
};
use frama_c_mcp_server::state::{
    FunctionConclusionUpdate, SessionState, VerificationStatus,
};
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: write_long_text_field 写文件 + read_long_texts_as_json 读回
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t1_long_text_write_read_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("factorial");
    std::fs::create_dir_all(&dir).unwrap();

    write_long_text_field(&dir, "semantic_proof", "# SP\n## Section\nfacts").unwrap();
    write_long_text_field(&dir, "semiformal_proof", "# Semiformal\n## 1.").unwrap();
    write_long_text_field(&dir, "program_summary", "nonneg int factorial").unwrap();

    assert!(dir.join("semantic_proof.md").is_file());
    assert!(dir.join("semiformal_proof.md").is_file());
    assert!(dir.join("program_summary.md").is_file());

    let json = read_long_texts_as_json(&dir);
    assert_eq!(
        json.get("semantic_proof").unwrap().as_str(),
        Some("# SP\n## Section\nfacts")
    );
    assert_eq!(json.get("program_summary").unwrap().as_str(), Some("nonneg int factorial"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: persist_conclusion 只写 meta.json — 完全不碰长文本 .md
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t2_persist_does_not_touch_long_text() {
    let tmp = TempDir::new().unwrap();
    let func = "demo";

    // Setup: LLM 先直接 write semantic_proof.md
    let dir = tmp.path().join(func);
    std::fs::create_dir_all(&dir).unwrap();
    let sp_file = dir.join("semantic_proof.md");
    std::fs::write(&sp_file, "LLM CONTENT").unwrap();

    // 准备短字段 state
    let mut state = SessionState::default();
    state.store_conclusion(FunctionConclusionUpdate {
        function: func.into(),
        status: Some(VerificationStatus::Verified),
        notes: Some("hello".into()),
        ..Default::default()
    });

    // persist 只写 meta.json，不应碰 semantic_proof.md
    persist_conclusion_at(tmp.path(), func, state.get_conclusion(func).unwrap()).unwrap();
    assert!(sp_file.exists(), "persist 误碰了 semantic_proof.md");
    assert_eq!(std::fs::read_to_string(&sp_file).unwrap(), "LLM CONTENT");

    // meta.json 写了短字段
    let meta_str = std::fs::read_to_string(dir.join("meta.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
    assert_eq!(meta["status"].as_str(), Some("verified"));
    assert_eq!(meta["notes"].as_str(), Some("hello"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: write_long_text_field 空 content = no-op（保护 LLM 直接写的工作）
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t3_write_empty_is_noop() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("preserve");
    std::fs::create_dir_all(&dir).unwrap();

    write_long_text_field(&dir, "semantic_proof", "X").unwrap();
    assert!(dir.join("semantic_proof.md").is_file());

    // 空 content → no-op，文件不变
    write_long_text_field(&dir, "semantic_proof", "").unwrap();
    assert!(dir.join("semantic_proof.md").exists(), "空 content 不应删文件");
    assert_eq!(std::fs::read_to_string(dir.join("semantic_proof.md")).unwrap(), "X");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: 直接 fs::write 文件 → read_long_texts_as_json 反映（无 state 参与）
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t4_direct_write_reflected_in_response() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("direct");
    std::fs::create_dir_all(&dir).unwrap();

    // LLM Write 工具的等价（fs::write）
    std::fs::write(dir.join("semantic_proof.md"), "LLM SP").unwrap();
    std::fs::write(dir.join("semiformal_proof.md"), "LLM SF").unwrap();

    let json = read_long_texts_as_json(&dir);
    assert_eq!(json.get("semantic_proof").unwrap().as_str(), Some("LLM SP"));
    assert_eq!(json.get("semiformal_proof").unwrap().as_str(), Some("LLM SF"));
    // 防回退：analysis_summary 字段已删除，响应 JSON 不应含此 key
    // （2026-05-26 因撞 CC subagent guard 删除，见 rename-analysis-summary-subagent-guard.md）
    assert!(json.get("analysis_summary").is_none(),
        "analysis_summary 已从 LONG_TEXT_FIELDS 删除，响应不应含此 key");
    // program_summary 是 Option 语义，文件缺失 → 不在 JSON 里
    assert!(json.get("program_summary").is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: 旧 <func>.json 文件被 load_conclusions_from_disk 忽略
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t5_legacy_json_ignored() {
    let tmp = TempDir::new().unwrap();

    std::fs::write(tmp.path().join("foo.json"), r#"{"function":"foo"}"#).unwrap();

    let mut state = SessionState::default();
    state.store_conclusion(FunctionConclusionUpdate {
        function: "bar".into(),
        status: Some(VerificationStatus::Verified),
        ..Default::default()
    });
    persist_conclusion_at(tmp.path(), "bar", state.get_conclusion("bar").unwrap()).unwrap();

    let loaded = load_conclusions_from_disk(tmp.path());
    assert!(loaded.contains_key("bar"));
    assert!(!loaded.contains_key("foo"));
    assert_eq!(loaded.len(), 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6: meta.json 永不含长文本字段；manifest 只列存在的 .md 文件
// （demo bug fix: reviewer 看到 manifest 中提及但实际不存在的文件会 FAIL）
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t6_meta_json_excludes_long_text_keys_manifest_only_existing() {
    let tmp = TempDir::new().unwrap();
    let func = "f";
    let dir = tmp.path().join(func);
    std::fs::create_dir_all(&dir).unwrap();

    // 模拟 handler 写了 1 个长文本文件（semantic_proof），没写 semiformal_proof / program_summary
    write_long_text_field(&dir, "semantic_proof", "SP content").unwrap();

    let mut state = SessionState::default();
    state.store_conclusion(FunctionConclusionUpdate {
        function: func.into(),
        status: Some(VerificationStatus::Verified),
        ..Default::default()
    });
    persist_conclusion_at(tmp.path(), func, state.get_conclusion(func).unwrap()).unwrap();

    let meta_str = std::fs::read_to_string(dir.join("meta.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
    let obj = meta.as_object().unwrap();

    // meta.json 不含长文本 key（含 analysis_summary 负断言，防回退）
    for key in ["analysis_summary", "semantic_proof", "semiformal_proof", "program_summary"] {
        assert!(
            !obj.contains_key(key),
            "meta.json 不应含长文本 key '{}' (Plan A: 长文本只在 .md 文件)",
            key
        );
    }

    // manifest 只列**存在**的 1 个文件，不列 missing 的 semiformal/program
    let manifest = obj.get("_long_text_files").expect("manifest 必须存在");
    let files = manifest.get("files").and_then(|v| v.as_array()).expect("manifest.files 必须是数组");
    let file_names: Vec<String> = files.iter().filter_map(|v| v.as_str().map(String::from)).collect();
    assert!(file_names.contains(&"semantic_proof.md".to_string()), "manifest 缺 semantic_proof.md");
    assert!(!file_names.contains(&"semiformal_proof.md".to_string()),
        "manifest 不应列 semiformal_proof.md（文件不存在）");
    assert!(!file_names.contains(&"program_summary.md".to_string()),
        "manifest 不应列 program_summary.md（文件不存在）");
    // analysis_summary 已从 LONG_TEXT_FIELDS 删除，绝不在 manifest 中
    assert!(!file_names.contains(&"analysis_summary.md".to_string()),
        "manifest 不应列 analysis_summary.md（字段已删除）");

    // 没写的 2 个文件确实不在 disk
    assert!(!dir.join("semiformal_proof.md").exists());
    assert!(!dir.join("program_summary.md").exists());
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 7: 长文本 + 短字段端到端 — handler 工作流模拟
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn t7_handler_workflow_long_plus_short() {
    let tmp = TempDir::new().unwrap();
    let func = "e2e";
    let dir = tmp.path().join(func);

    // 模拟 store_function_conclusion handler:
    // 1) 显式 write_long_text_field（caller 写 long-text .md）
    std::fs::create_dir_all(&dir).unwrap();
    write_long_text_field(&dir, "semantic_proof", "PROOF").unwrap();

    // 2) state.store_conclusion 短字段
    let mut state = SessionState::default();
    state.store_conclusion(FunctionConclusionUpdate {
        function: func.into(),
        status: Some(VerificationStatus::Verified),
        notes: Some("ok".into()),
        ..Default::default()
    });

    // 3) persist meta.json
    persist_conclusion_at(tmp.path(), func, state.get_conclusion(func).unwrap()).unwrap();

    // Verify: meta.json 有短字段，long-text 在 .md
    let loaded = load_conclusion_dir(&dir).unwrap();
    assert!(matches!(loaded.status, VerificationStatus::Verified));
    assert_eq!(loaded.notes, "ok");

    // get_function_conclusion handler 响应组装（meta JSON + long_texts）
    let mut value = serde_json::to_value(&loaded).unwrap();
    if let Some(obj) = value.as_object_mut() {
        for (k, v) in read_long_texts_as_json(&dir) {
            obj.insert(k, v);
        }
    }
    assert_eq!(value["status"].as_str(), Some("verified"));
    assert_eq!(value["notes"].as_str(), Some("ok"));
    assert_eq!(value["semantic_proof"].as_str(), Some("PROOF"));
    // 防回退：响应 JSON 不应含 analysis_summary key（已删除字段）
    assert!(value.get("analysis_summary").is_none(),
        "get_function_conclusion 响应不应含 analysis_summary key");
}
