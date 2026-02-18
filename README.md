# frama-c-mcp-server

An MCP (Model Context Protocol) server for [Frama-C](https://frama-c.com/), enabling AI verification agents to interact with Frama-C's static analysis and formal verification capabilities.

## Architecture

The server is implemented as a **native Frama-C plugin** (pure OCaml), communicating with AI agents via the MCP protocol (JSON-RPC over stdio).

```
Agent <-- MCP protocol --> Frama-C Plugin (OCaml)
                            ├── MCP protocol (JSON-RPC)
                            ├── Tool implementations
                            │    └── Direct Frama-C API calls
                            ├── Query cache / indexes
                            └── Task management
```

Key design decisions:
- **Pure OCaml** — zero FFI overhead, direct access to Frama-C APIs (CIL AST, WP, EVA)
- **In-process** — AST consistency maintained by Frama-C kernel, no state synchronization
- **Standard plugin** — loads via `frama-c -load-module`, standard dune/opam build

See [docs/architecture.md](docs/architecture.md) for the full architecture discussion.

## Status

**Early development** — POC phase.

## Prerequisites

- [Frama-C](https://frama-c.com/) >= 32.0 (Germanium)
- OCaml >= 4.14
- opam

## Project Structure

```
frama-c-mcp-server/
├── src/                    # MCP plugin source (pure OCaml)
├── experiments/            # OCaml ↔ Rust FFI experiments
│   ├── standalone-ffi/
│   └── frama-c-ffi/
├── test/                   # Test C files for verification
└── docs/                   # Architecture & design docs
```

## License

MIT
