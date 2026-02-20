# frama-c-mcp-server

An MCP (Model Context Protocol) server for [Frama-C](https://frama-c.com/), enabling AI verification agents to interact with Frama-C's static analysis and formal verification capabilities.

## Architecture

A **Rust MCP server** communicates with a **Frama-C Server** process via Unix Socket. Rust handles the MCP protocol (JSON-RPC over stdio); Frama-C handles all verification work (CIL AST, WP, EVA).

```
Agent <-- MCP (stdio) --> Rust (rmcp) <-- Unix Socket --> Frama-C Server
                           ├── MCP protocol (JSON-RPC)       ├── Kernel / CIL AST
                           ├── Tool router (20 tools)        ├── EVA / WP plugins
                           ├── Session state                 └── Server plugin
                           └── Frama-C protocol adapter
```

Key design decisions:
- **Rust + rmcp** — official MCP SDK, full spec 2025-11-25 support (Task, Tool annotations)
- **Frama-C Server** — reuse Frama-C's built-in Server plugin (200+ registered requests)
- **Unix Socket** — low-latency IPC, no external dependencies

See [docs/architecture.md](docs/architecture.md) for the full architecture discussion and decision log.

## Status

**Early development** — architecture finalized, implementation not started.

## Prerequisites

- [Frama-C](https://frama-c.com/) >= 31.0 (Gallium)
- OCaml >= 4.14
- Rust (nightly, for edition 2024)
- opam

## Project Structure

```
frama-c-mcp-server/
├── src/                    # Rust MCP server (not yet created)
├── experiments/            # OCaml ↔ Rust FFI experiments (completed)
│   ├── standalone-ffi/     # OCaml↔Rust via ctypes.foreign
│   ├── frama-c-ffi/        # Frama-C plugin calling Rust
│   └── rust-calls-ocaml/   # Rust embedding OCaml runtime
├── test/                   # Test C files for verification
└── docs/                   # Architecture & design docs
```

## License

MIT
