# Frama-C MCP Architecture Discussion: CIL Storage & FFI Strategy

> Archived from design discussion (2025-02-18).
> See the original file for the complete discussion with code examples and comparison matrices.

## Selected Architecture: Pure OCaml Plugin

The MCP server is implemented as a standard Frama-C plugin, entirely in OCaml.

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

### Why Pure OCaml

MCP server is fundamentally a **thin protocol adapter** over Frama-C's capabilities. The heavy lifting (AST traversal, verification, annotation management) is all Frama-C API — which is OCaml. Introducing Rust adds FFI complexity that only pays off if the MCP server layer itself becomes very complex.

### Fallback: Frama-C Calls Rust (Approach 4)

If OCaml's async/networking becomes insufficient, validated fallback is FFI to a Rust library via C ABI. See `experiments/` for FFI validation work.

## Approaches Considered

1. **Frama-C Backend + ZMQ** — rejected (two-process overhead)
2. **Export CIL to Rust** — rejected (no existing export/import; state sync unsustainable)
3. **Embed OCaml in Rust** — rejected (Frama-C not designed for embedding)
4. **Frama-C Calls Rust** — validated fallback
5. **Pure OCaml Plugin** — selected
