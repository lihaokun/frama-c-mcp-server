# Frama-C MCP Architecture Discussion: CIL Storage & FFI Strategy

**Date:** 2025-02-18
**Status:** Architecture exploration — POC needed before implementation
**Context:** This document archives a design discussion about the Frama-C MCP server architecture, specifically exploring where to store the CIL AST and how Rust and OCaml should interact.

---

## Background

We are building an MCP (Model Context Protocol) server that makes Frama-C accessible to AI verification agents. The previous design (v2.2) assumed a Frama-C backend communicating with a Rust MCP server via ZMQ. This discussion re-evaluated that assumption.

## The Core Question

**Should the CIL AST live in Frama-C (OCaml) or be exported to Rust?**

We explored five architectural approaches:

---

## Approach 1: Frama-C Backend + ZMQ (Original Design)

```
Agent ←→ MCP Server (Rust) ←ZMQ→ Frama-C (常驻进程)
```

**Pros:**
- CIL AST and all Frama-C plugins (WP, EVA) share the same memory space
- Frama-C automatically maintains AST consistency after annotation changes
- No need to reimplement any Frama-C semantic analysis

**Cons:**
- Heavy process management (two processes)
- ZMQ serialization/deserialization overhead on every query
- Impedance mismatch between OCaml and Rust ecosystems

---

## Approach 2: Export CIL to Rust Storage

```
Frama-C (CLI, stateless) → export CIL JSON → Rust (stores/indexes CIL)
```

**Pros:**
- Code navigation queries at microsecond latency in Rust
- No long-running Frama-C process needed
- Full control over indexing and caching in Rust

**Cons (critical):**
- CIL is not static data — every annotation change modifies the AST
- Verification (WP, EVA) must still run in Frama-C → need to sync two states
- Frama-C cold start cost on every verification round (parse + analyze)
- No existing CIL export/import mechanism in Frama-C (see Research Findings below)

---

## Approach 3: Embed OCaml Runtime in Rust

```
Rust (main process) → embeds OCaml runtime → loads Frama-C as library
```

**Pros:**
- Single process, no IPC
- Direct memory access to CIL AST

**Cons:**
- **High feasibility risk:** Frama-C is not designed to be embedded
- Would require rebuilding Frama-C as a library (.cma/.cmxa) instead of executable
- OCaml runtime single-thread limitation (pre-OCaml 5)
- GC safety complexity across FFI boundary

---

## Approach 4: Frama-C Calls Rust (Validated Fallback)

```
Frama-C (main process, OCaml)
  └── MCP Bridge Plugin (thin OCaml layer)
       └── calls Rust library (.so/.a) via C ABI
            ├── MCP Server logic
            ├── Agent communication
            ├── Caching / indexing
            └── Task management
```

**Pros:**
- **Zero feasibility risk:** Frama-C's plugin architecture is designed exactly for this
- OCaml calling C ABI is native (`external` keyword); Rust generates C ABI natively (`extern "C"`)
- No intermediate C code needed — Rust's `#[no_mangle] extern "C"` is directly callable from OCaml
- Full access to all Frama-C capabilities (WP, EVA, CIL AST) — everything is in-process
- AST consistency maintained by Frama-C kernel automatically
- No IPC, no serialization overhead for AST queries
- Frama-C's plugin loading mechanism (`-load-module`) handles deployment

**Cons:**
- Build system complexity (linking Rust .so into Frama-C plugin)
- OCaml ↔ Rust type marshalling for complex types (mitigated by using JSON strings at the boundary)
- Single process means Frama-C crash = MCP server crash

---

## Approach 5: Pure OCaml Plugin (SELECTED) ⭐

```
Agent ←MCP协议→ Frama-C Plugin (全 OCaml)
                 ├── MCP protocol handling (JSON-RPC)
                 ├── Query/cache logic
                 └── Direct Frama-C API calls
```

**Pros:**
- **Zero FFI overhead** — all Frama-C API calls are native OCaml, fully type-safe
- **Minimal build complexity** — standard Frama-C plugin, single language, single build system (dune/opam)
- **Easiest debugging** — single language stack, no cross-language issues
- **Smallest codebase** — no FFI glue layer (~20-30% less code than Approach 4)
- **Frama-C Server plugin as reference** — existing infrastructure for JSON, network communication (Unix socket) can be reused or referenced
- **Full Frama-C capabilities** — same in-process advantage as Approach 4

**Cons:**
- OCaml async/networking ecosystem is weaker than Rust (Lwt/Async vs tokio)
- No official OCaml MCP SDK (need to implement MCP protocol from scratch or port)
- JSON handling (Yojson) less ergonomic than Rust's serde
- Less reusable if MCP server later needs to support non-Frama-C backends

### Why this over Approach 4 (Frama-C calls Rust):

The key realization: MCP server is fundamentally a **thin protocol adapter** over Frama-C's capabilities. The heavy lifting (AST traversal, verification, annotation management) is all Frama-C API — which is OCaml. Introducing Rust adds FFI complexity that only pays off if the MCP server layer itself becomes very complex (advanced concurrency, multi-backend support). For the initial system, pure OCaml is the path of least resistance.

---

## Research Findings: Frama-C CIL Export Capabilities

### What Frama-C HAS:

1. **`-save` / `-load` (OCaml Marshal):** Binary session serialization using OCaml's internal Marshal format. Includes full AST + plugin states. **Not readable by Rust** — uses OCaml-specific binary format with customized unmarshalling for shared structures.

2. **Server API (JSON over ZMQ/Socket):** Used by Ivette GUI. Exposes AST information as JSON via registered requests. Returns **fragmented, on-demand** views (function lists, statement details, markers), **not** a complete CIL AST dump.

3. **`-print` (CIL pretty-print):** Outputs normalized C source code. Reading it back requires full re-parsing — not a round-trip format.

4. **`-ast-diff`:** Computes differences between saved AST and current sources. Still requires re-parsing and type-checking.

### What Frama-C DOES NOT HAVE:

- **No CIL AST → JSON/Protobuf complete export**
- **No CIL AST import from external format**
- **No way for external tools to read .sav files**

### What building export/import would require:

| Component | Complexity | Notes |
|-----------|-----------|-------|
| OCaml export plugin (CIL → JSON) | Medium | Use visitor to traverse AST, serialize ~50-60 CIL type variants |
| Rust CIL data structures | High | 1:1 mirror of `cil_types.mli` including ACSL logic types |
| Rust JSON deserialization | Medium | Serde can automate once structs are defined |
| OCaml import plugin (JSON → CIL) | **Very High** | Must rebuild AST satisfying all kernel invariants (type consistency, unique IDs, etc.) |

**Conclusion:** Building full export/import is prohibitively expensive. The FFI approach (Approach 4) avoids this entirely.

---

## Rust ↔ OCaml FFI Landscape

### Available Libraries:

| Library | Direction | Maturity | Notes |
|---------|-----------|----------|-------|
| `ocaml-interop` (tizoc/tezedge) | Bidirectional | Most mature | Handles GC safety, type conversion for records/variants |
| `ocaml-rust` (LaurentMazare) | OCaml → Rust | Proof of concept | cxx-inspired, auto-generates OCaml type definitions |
| `raml` (m4b) | Bidirectional | Low-level | Direct C FFI macro wrappers |

### Key FFI Facts:

- OCaml's `external` keyword speaks C ABI natively
- Rust's `#[no_mangle] extern "C"` generates C ABI natively
- **No C code is needed as intermediary** — they share the calling convention
- For simple types (int, float): direct interop, no library needed
- For complex types (OCaml string, list, variant): need `ocaml-interop` for GC root management
- Practical strategy: **pass JSON strings across the FFI boundary** for complex data, keeping the FFI surface minimal

---

## Comparison Matrix

| Criterion | ZMQ Backend | Rust Stores CIL | Rust Embeds OCaml | Frama-C Calls Rust | **Pure OCaml Plugin** |
|-----------|------------|-----------------|-------------------|----------------------|----------------------|
| Main process | Two processes | Rust | Rust | Frama-C | **Frama-C** |
| Frama-C full capabilities | ✅ | ❌ (verification roundtrip) | ❓ (unverified) | ✅ | **✅** |
| Embedding risk | None | None | **High** | None | **None** |
| AST access | IPC roundtrip | Static copy | FFI | FFI (C ABI) | **Direct native** |
| Build complexity | Medium | Medium | High | Low-Medium | **Lowest** |
| Plugin ecosystem | ✅ | ❌ | ❓ | ✅ | **✅** |
| Query latency | ~ms (IPC) | ~μs (local) | ~μs (FFI) | ~μs (FFI) | **~ns (native call)** |
| Verification latency | Fast (AST in memory) | Slow (cold start) | Fast | Fast | **Fast (AST in memory)** |
| State consistency | ✅ (Frama-C manages) | ❌ (manual sync) | ✅ | ✅ | **✅ (Frama-C manages)** |
| Debugging | Cross-process | Two codebases | Cross-language | Cross-language | **Single language** |
| Networking/async ecosystem | N/A | Rust (strong) | Rust (strong) | Rust (strong) | **OCaml (adequate)** |

---

## Decided Architecture

### Primary: Pure OCaml Plugin

```
┌──────────────────────────────────────────────────┐
│          Frama-C Process (OCaml)                  │
│                                                  │
│  Frama-C Kernel                                  │
│    ├── CIL AST (in memory)                       │
│    ├── WP / Eva / all native plugins             │
│    │                                             │
│    └── MCP Plugin (pure OCaml)                   │
│         ├── MCP protocol (JSON-RPC)              │
│         ├── Tool implementations                 │
│         │    └── Direct Frama-C API calls        │
│         ├── Query cache / indexes                │
│         └── Task management                      │
└──────────────────────────────────────────────────┘

Launch: frama-c input.c -load-module frama_c_mcp.cmxs -mcp-start
```

### Data Flow for a Typical Agent Query:

```
Agent → MCP request (JSON-RPC)
  → OCaml MCP plugin parses request
  → OCaml directly calls Frama-C API (e.g., Globals.Functions.fold)
  → Formats MCP response → Agent
```

### Data Flow for Verification:

```
Agent → "verify function foo"
  → OCaml MCP plugin calls WP API directly
  → WP runs in-process, returns proof results
  → OCaml formats result → MCP response → Agent
```

### When to Reconsider Rust (Approach 4):

The pure OCaml path is the starting point. Rust becomes worth introducing if:
- MCP server needs advanced concurrent handling beyond Lwt's capabilities
- The system grows to support multiple verification backends (not just Frama-C)
- Complex task scheduling/persistence needs Rust's ecosystem (databases, async runtime)
- OCaml's JSON/networking libraries become a bottleneck

In that case, the FFI experiments (see POC Phase 1 below) will have already validated the integration path.

---

## POC Plan

The POC has two phases: Phase 1 validates OCaml↔Rust FFI (exploratory/educational), Phase 2 validates the actual pure OCaml plugin architecture.

### Phase 1: OCaml ↔ Rust FFI Experiment

**Goal:** Learn the mechanics of OCaml/Rust interop. This is not on the critical path for the MCP server, but validates Approach 4 as a fallback and builds understanding of the FFI boundary.

#### Experiment 1A: OCaml calls Rust (Frama-C plugin → Rust .so)

```rust
// rust_experiment/src/lib.rs — compiled as cdylib (.so)

#[no_mangle]
pub extern "C" fn rust_add(a: i64, b: i64) -> i64 {
    println!("[Rust] Computing {} + {} = {}", a, b, a + b);
    a + b
}

// String example: Rust receives an OCaml-formatted string pointer
// For simplicity, use integer-only API first, then try strings
#[no_mangle]
pub extern "C" fn rust_process_json(ptr: *const u8, len: usize) -> i64 {
    let json_str = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len)) };
    println!("[Rust] Received JSON: {}", json_str);
    // Parse, process, return status
    0
}
```

```ocaml
(* ocaml_experiment/ffi_test.ml — Frama-C plugin *)
external rust_add : int -> int -> int = "rust_add"

let () =
  Db.Main.extend (fun () ->
    let result = rust_add 17 25 in
    Kernel.feedback "Rust returned: %d" result;

    (* List functions via Frama-C API, send to Rust *)
    let names = Globals.Functions.fold
      (fun kf acc -> Kernel_function.get_name kf :: acc) [] in
    Kernel.feedback "Functions found: %s" (String.concat ", " names)
  )
```

**Validates:**
- Rust .so loads correctly in Frama-C plugin context
- Simple types (int) cross the boundary without issue
- Frama-C plugin lifecycle is compatible with Rust library initialization

**Success criteria:**
- `frama-c test.c -load-module ffi_test.cmxs` prints Rust output + function list

#### Experiment 1B: Rust calls back into OCaml

```ocaml
(* Register OCaml callbacks for Rust to call *)
let () =
  Callback.register "get_function_count" (fun () ->
    Globals.Functions.fold (fun _ acc -> acc + 1) 0
  );
  Callback.register "get_function_names_json" (fun () ->
    let names = Globals.Functions.fold
      (fun kf acc -> Kernel_function.get_name kf :: acc) [] in
    (* Return as JSON string — simplest cross-boundary format *)
    Printf.sprintf "[%s]"
      (String.concat "," (List.map (fun n -> Printf.sprintf "\"%s\"" n) names))
  )
```

```rust
// Rust side — using ocaml-interop or raw C FFI
// Option A: with ocaml-interop
ocaml! {
    fn get_function_count() -> i64;
    fn get_function_names_json() -> String;
}

#[no_mangle]
pub extern "C" fn rust_query_ast() {
    // Call back into OCaml
    let count = get_function_count(cr);
    let names_json = get_function_names_json(cr);
    println!("[Rust] AST has {} functions: {}", count, names_json);
}

// Option B: raw C FFI (no library dependency)
extern "C" {
    fn caml_named_value(name: *const i8) -> *const u8;
    fn caml_callback(closure: u64, arg: u64) -> u64;
}
```

**Validates:**
- Bidirectional FFI within Frama-C plugin lifecycle
- OCaml GC stability when Rust holds/calls closures
- JSON-as-string strategy for complex data crossing the boundary

**Success criteria:**
- Rust successfully calls OCaml callback and receives AST data
- No segfaults or GC corruption over repeated calls

#### Experiment 1C: Standalone OCaml ↔ Rust (outside Frama-C)

A simpler experiment without Frama-C complexity. Useful for isolating FFI issues.

```rust
// Pure Rust library
#[no_mangle]
pub extern "C" fn rust_greet(name_ptr: *const u8, name_len: usize) -> i64 {
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };
    println!("[Rust] Hello, {}!", name);
    name.len() as i64
}
```

```ocaml
(* Standalone OCaml program *)
external rust_greet : string -> int -> int = "rust_greet"

let () =
  let name = "OCaml" in
  let len = rust_greet name (String.length name) in
  Printf.printf "Rust processed %d bytes\n" len
```

**Validates:** Basic linking and calling works before adding Frama-C complexity.

#### FFI Experiment Build Setup

```
frama-c-mcp/
├── experiments/
│   ├── standalone-ffi/        # Experiment 1C: pure OCaml↔Rust
│   │   ├── rust_lib/
│   │   │   ├── Cargo.toml
│   │   │   └── src/lib.rs
│   │   ├── ocaml_caller/
│   │   │   ├── dune
│   │   │   └── main.ml
│   │   └── Makefile
│   │
│   └── frama-c-ffi/           # Experiments 1A & 1B: within Frama-C
│       ├── rust_lib/
│       │   ├── Cargo.toml
│       │   └── src/lib.rs
│       ├── ocaml_plugin/
│       │   ├── dune
│       │   └── ffi_test.ml
│       ├── test/
│       │   └── test.c
│       └── Makefile
│
├── src/                       # Phase 2: actual MCP plugin (pure OCaml)
│   └── ...
└── README.md
```

#### FFI Experiment Key Risks

| Risk | Mitigation |
|------|-----------|
| OCaml GC moves values during Rust callback | Use `ocaml-interop` BoxRoot; or pass only simple types / JSON strings |
| Frama-C plugin loading conflicts with Rust .so | Test dynamic linking; may need `-cclib -lrust_experiment` in dune |
| OCaml runtime not initialized when Rust calls back | Ensure callbacks registered before Rust entry point is called |
| `ocaml-interop` version incompatible with Frama-C's OCaml | Check Frama-C 32.0 required OCaml version; try raw C FFI as fallback |
| OCaml string representation differs from C string | Use explicit ptr+len passing, not null-terminated strings |

---

### Phase 2: Pure OCaml MCP Plugin (Critical Path)

**Goal:** Build a minimal but functional MCP server as a Frama-C plugin, validating the pure OCaml architecture.

#### Step 1: Minimal MCP Protocol Handler

```ocaml
(* Implement JSON-RPC 2.0 over stdin/stdout or Unix socket *)
(* Use Yojson for JSON parsing *)
(* Implement: initialize, tools/list, tools/call *)
```

**Validates:** OCaml can handle MCP protocol adequately.

#### Step 2: First Tool — list_functions

```ocaml
(* MCP tool that returns all functions in the loaded C project *)
let handle_list_functions _params =
  let functions = Globals.Functions.fold (fun kf acc ->
    let name = Kernel_function.get_name kf in
    let loc = Kernel_function.get_location kf in
    `Assoc [
      ("name", `String name);
      ("file", `String (fst loc).Filepath.pos_path);
      ("line", `Int (fst loc).Filepath.pos_lnum);
    ] :: acc
  ) [] in
  `Assoc [("functions", `List functions)]
```

**Validates:** Frama-C API is directly usable for MCP tool implementations.

#### Step 3: Verification Tool — run_wp

```ocaml
(* MCP tool that runs WP on a specified function *)
let handle_run_wp params =
  let fname = (* extract from params *) in
  (* Configure and run WP *)
  ...
```

**Validates:** Full verification workflow through MCP.

#### Phase 2 Success Criteria

1. ✅ `frama-c test.c -load-module frama_c_mcp.cmxs` starts MCP server
2. ✅ Agent can connect and call `tools/list`
3. ✅ `list_functions` returns correct function list from CIL AST
4. ✅ `run_wp` triggers verification and returns results
5. ✅ Multiple sequential requests work without state corruption

---

## Open Questions for Post-POC

1. **MCP transport:** stdin/stdout (standard MCP) vs Unix socket vs TCP? Frama-C's Server plugin already uses Unix socket — may be reusable.
2. **Concurrency:** Do we need concurrent request handling? If so, Lwt or OCaml 5 effects? Or is sequential sufficient for a single-agent use case?
3. **Hot reload:** Can we modify annotations and re-verify without restarting Frama-C? (Should be yes — the AST is mutable in-memory.)
4. **Which OCaml version does Frama-C 32.0 (Germanium) require?** Determines OCaml 5 multicore availability.
5. **Reuse Frama-C Server infrastructure?** The Server plugin already has JSON encoding, request registration, and protocol handling. Can we build MCP on top of it rather than from scratch?
6. **When to introduce Rust?** Define concrete criteria for when pure OCaml becomes insufficient and FFI migration is warranted.

---

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2025-02-18 | Reject "Rust stores CIL" approach | No existing export/import; maintaining two state copies is unsustainable |
| 2025-02-18 | Reject "Rust embeds OCaml" approach | Frama-C not designed for embedding; high risk |
| 2025-02-18 | Evaluate "Frama-C calls Rust" (Approach 4) | Leverages plugin mechanism; zero embedding risk; FFI is mature |
| 2025-02-18 | **Select "Pure OCaml Plugin" (Approach 5) as primary** | MCP server is a thin protocol adapter; OCaml avoids all FFI complexity while retaining full Frama-C API access; Approach 4 remains validated fallback |
| 2025-02-18 | FFI experiments as Phase 1 (non-blocking) | Validates Approach 4 as fallback; builds OCaml↔Rust interop knowledge; fun and educational |
| 2025-02-18 | POC before implementation | Phase 1: FFI learning; Phase 2: pure OCaml MCP plugin validation |
