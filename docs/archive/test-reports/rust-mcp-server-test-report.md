# Rust MCP Server — Test Report

## 1. Test Summary

| Layer | Tests | Pass | Fail | Duration |
|-------|-------|------|------|----------|
| Unit tests (`cargo test --lib`) | 23 | 23 | 0 | <0.01s |
| Integration tests (`cargo test --test integration_test`) | 6 | 6 | 0 | 3.67s |
| MCP E2E test (Python, NDJSON over stdio) | 8 steps | 8 | 0 | ~15s |
| **Total** | **37** | **37** | **0** | |

Test environment: Frama-C 31.0 (Gallium), OCaml 4.14.2, Rust nightly 1.95.0, rmcp 0.16.0

Test target: `test/test_abs.c` (3 functions: `abs_val`, `square`, `main`)

---

## 2. Unit Tests (23)

### 2.1 codec module (18 tests)

| Test | Description |
|------|-------------|
| `decode_data_response` | DATA response with JSON object |
| `decode_data_null` | DATA response with null data |
| `decode_error_response` | ERROR response |
| `decode_rejected_response` | REJECTED response |
| `decode_signal_response` | SIGNAL response |
| `decode_killed_response` | KILLED response |
| `decode_cmdlineon` | CMDLINEON response |
| `decode_cmdlineoff` | CMDLINEOFF response |
| `decode_frame_incomplete` | Incomplete frame returns None |
| `decode_frame_invalid_prefix` | Invalid prefix returns error |
| `encode_get_command` | GET command encoding |
| `encode_poll_command` | POLL command encoding |
| `encode_kill_command` | KILL command encoding |
| `encode_shutdown_command` | SHUTDOWN command encoding |
| `frame_roundtrip_small` | S-frame encode/decode roundtrip |
| `frame_roundtrip_large` | L-frame encode/decode roundtrip |
| `full_roundtrip_get` | Full GET command roundtrip |
| `full_roundtrip_poll` | Full POLL command roundtrip |

### 2.2 state module (5 tests)

| Test | Description |
|------|-------------|
| `update_and_resolve` | Parse fetchFunctions JSON, resolve by name |
| `resolve_missing` | Missing function returns None |
| `skip_empty_name` | Entries with empty name are skipped |
| `invalidate_all` | Clears all state and functions |
| `invariants` | State transitions maintain invariants |

---

## 3. Integration Tests (6)

Prerequisites: Live Frama-C Server at `/tmp/frama-c-test.sock`

### 3.1 `test_full_workflow` — 14 steps, all 8 tools

| Step | Tool/Feature | Assertions |
|------|-------------|------------|
| 1 | Connect + state | 3 functions loaded, correct file/line |
| 2 | `get_function_info` | printDeclaration returns array |
| 3 | `getFiles` | Non-empty file list |
| 4 | `get_callgraph` | Returns edges + vertices |
| 5 | `run_eva` | computationState == "computed" |
| 6 | `get_eva_alarms` | 10 properties total, 5 for abs_val |
| 7 | `get_eva_value` | Returns value ranges for #s2 |
| 8 | `run_wp` | startProofs succeeds with #v marker |
| 9 | `get_verification_status` | 12 valid properties |
| 10 | Rejected request | nonexistent.endpoint → Rejected error |
| 11 | Incremental fetch | consumed=0, reload=3, again=0 |
| 12 | Cache invalidation + refresh | invalidate_all → reloadFunctions + fetchFunctions restores 3 functions (F2/F3 regression) |
| 13 | Scoped property filtering | After cache refresh, abs_val filter returns 7 of 12 properties (F7 regression) |
| 14 | Final state | project_loaded, eva_completed, wp_completed |

### 3.2 Offline edge case tests

| Test | Description |
|------|-------------|
| `test_function_not_found` | resolve_function returns None for unknown |
| `test_state_invalidation` | invalidate_all clears all fields |
| `test_update_functions_empty_clears_cache` | F2 regression: update_functions(&[]) clears existing cache |
| `test_connect_bad_socket` | Nonexistent socket path → I/O error |

---

## 4. MCP End-to-End Test (8 steps)

Spawns the `frama-c-mcp-server` binary, communicates via JSON-RPC over stdio (NDJSON framing).

| Step | MCP Request | Assertions |
|------|-------------|------------|
| 1 | `initialize` | Server responds with serverInfo |
| 2 | `notifications/initialized` | No error |
| 3 | `tools/list` | 8 tools, all have description + inputSchema |
| 4 | `tools/call` get_function_info(abs_val) | Returns name, file, line, declaration, signature |
| 5 | `tools/call` run_eva | computation_state == "computed" |
| 6 | `tools/call` get_eva_alarms(function=abs_val) | 5 properties for abs_val |
| 7 | `tools/call` get_verification_status | total_properties > 0, eva=true |
| 8 | `tools/call` run_wp(nonexistent_func) | Returns error response (isError=true) |

---

## 5. Bugs Found and Fixed During Testing

### 5.1 Protocol-Level Bugs

| # | Bug | Root Cause | Fix |
|---|-----|-----------|-----|
| 1 | `setProvers`/`setTimeout` timeout | SET commands are queued (like EXEC), not immediate (like GET) | `set()` uses `poll_loop` |
| 2 | Response ordering corruption | `get()`/`set()` didn't verify response ID matches request ID | Added `wait_for_id()` |
| 3 | `startProofs(#F24)` → "invalid marker" | `#F` is AST.Decl, not AST.Marker | Convert `#F` → `#v` (PVDecl) |
| 4 | `startProofs(#v24)` → 0 tasks | PVDecl markers not registered until printDeclaration called | Call `printDeclaration` before `startProofs` |
| 5 | `getValues(callstack: null)` → error | `callstack` is param_opt, must be omitted, not null | Remove callstack from JSON |
| 6 | Repeated `fetchStatus` returns empty | Incremental fetch: consumed once, needs `reloadStatus` | Call `reloadStatus` before `fetchStatus` |
| 7 | Tracing output corrupts stdout | tracing_subscriber defaults to stdout in some configs | Explicit `.with_writer(std::io::stderr)` |

### 5.2 Code Review Fixes (post-testing)

| # | Bug | Severity | Fix |
|---|-----|----------|-----|
| 10 | `reload_project` missing `reloadFunctions` before `fetchFunctions` | Medium | Add `reloadFunctions` call (defensive) |
| 11 | `get_function_info` cache cascade: `fetchFunctions` returns empty → `update_functions(&[])` clears cache | High | Add `reloadFunctions` via `resolve_function_or_refresh` |
| 12 | `run_wp` cache miss → direct error, no refresh attempt | Medium | Use `resolve_function_or_refresh` |
| 13 | `get_eva_alarms` scope filter silently skipped on cache miss | Medium | Use `resolve_function_or_refresh` |
| 14 | `get_verification_status` hardcoded `project_loaded: true` | Medium | Read from `state.project_loaded` |
| 15 | Clippy: `payload.as_bytes().len()` → `payload.len()` | Low | Fixed |
| 16 | Doc comment: marker example `#S42` → `#s2` | Low | Fixed |

### 5.3 Data Format Bugs

| # | Bug | Root Cause | Fix |
|---|-----|-----------|-----|
| 8 | `fetchFunctions` field mapping wrong | `sloc` is nested object, not top-level `file`/`line` | Use `entry["sloc"]["file"]` |
| 9 | `printDeclaration` parameter wrong | Accepts plain string marker, not object | Changed from `json!({"marker": ...})` to `json!(marker)` |

---

## 6. Test Coverage Analysis

### 6.1 Covered

- All 8 MCP tools: reload_project, get_function_info, get_callgraph, run_eva, get_eva_alarms, get_eva_value, run_wp, get_verification_status
- All Frama-C Server protocol commands: GET, SET, EXEC, POLL, SHUTDOWN
- Frame encoding/decoding: S-frame (small), L-frame (large)
- All response types: DATA, ERROR, REJECTED, SIGNAL, KILLED, CMDLINEON, CMDLINEOFF
- State management: function cache, EVA/WP flags, invalidation, cache refresh after invalidation
- Error handling: nonexistent functions, rejected requests, bad sockets
- Cache resilience: invalidate → reload + fetch restores state (F2/F3 regression)
- Incremental fetch: consumption, reload, re-fetch
- MCP protocol: initialize, tools/list, tools/call, error responses

### 6.2 Not Covered (Deferred)

- `reload_project` with file list parameter (tested without files arg via connect)
- `get_eva_value` with specific callstack index
- WP with custom prover (only tested with default)
- Large programs (only tested with 3-function program)
- Concurrent MCP requests (Phase 1 is single-threaded by design)
- W-frame (>16MB payload, unrealistic for current usage)
