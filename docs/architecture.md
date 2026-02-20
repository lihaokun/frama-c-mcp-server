# Frama-C MCP Server 架构决策

## 选定架构：Rust MCP Server + Frama-C Server (Unix Socket)

```
┌─────────────────────────────────────────────────────────┐
│                    LLM Agent (Claude)                    │
└──────────────────────┬──────────────────────────────────┘
                       │ MCP (JSON-RPC 2.0 / stdio)
                       ▼
┌─────────────────────────────────────────────────────────┐
│              frama-c-mcp-server  (Rust)                  │
│                                                          │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────┐  │
│  │ MCP Layer   │  │ Tool Router  │  │ State Manager  │  │
│  │ (rmcp 0.16) │  │ 20 tools     │  │ (会话状态)     │  │
│  │ stdio       │  │              │  │                │  │
│  └──────┬──────┘  └──────┬───────┘  └────────┬───────┘  │
│         │                │                    │          │
│  ┌──────▼────────────────▼────────────────────▼───────┐  │
│  │              Frama-C Client (Unix Socket)           │  │
│  │  自定义协议适配（Frama-C CMD/REPLY ↔ 内部调用）     │  │
│  └──────────┬─────────────────────────────────────────┘  │
└─────────────┼────────────────────────────────────────────┘
              │ Unix Socket (Frama-C Server 协议)
              ▼
┌─────────────────────────────────────────────────────────┐
│              Frama-C Server 进程                         │
│                                                          │
│  ┌──────────┐  ┌──────┐  ┌──────┐  ┌─────────────────┐  │
│  │ Kernel   │  │ EVA  │  │ WP   │  │ MCP Bridge      │  │
│  │ AST/Prop │  │      │  │      │  │ Plugin (Phase 3) │  │
│  └──────────┘  └──────┘  └──────┘  └─────────────────┘  │
└─────────────────────────────────────────────────────────┘

启动: frama-c input.c -server-socket /tmp/frama-c.sock
      frama-c-mcp-server --socket /tmp/frama-c.sock
```

### 决策理由

**2025-02-19 更新**：经过完整评估后，将架构从"纯 OCaml 插件"修改为"Rust MCP Server + Frama-C Server"。

评估了四个可行方案：

| 方案 | MCP 协议 | Frama-C 能力 | 工程难度 | 性能 |
|------|---------|-------------|---------|------|
| A: Rust + Frama-C Server | ★★★★★ | ★★★☆☆→★★★★★ | 中 | ms 级 |
| B: 纯 OCaml 插件 | ★★☆☆☆ | ★★★★★ | 中高 | ns 级 |
| C: 混合 (A 的超集) | ★★★★★ | ★★★★★ | 高 | ms 级 |
| D: Rust + CLI 子进程 | ★★★★★ | ★☆☆☆☆ | 低 | 秒级 |

选择方案 A 的核心原因：

1. **MCP 生态成熟度**：rmcp 是官方 Rust MCP SDK（月下载 110 万+），完整支持 MCP 2025-11-25 规范（Task、Tool annotations、Streamable HTTP）。OCaml 没有可用的 MCP SDK（`ocaml-mcp` 需要 OCaml 5.0+，当前环境是 4.14.2）
2. **Frama-C Server 已存在**：Frama-C 内置 Server 插件（Ivette GUI 的后端），支持 Unix Socket 传输，已注册 200+ 个 request。不需要从零构建 Frama-C 交互层
3. **异步能力**：EVA/WP 分析可能运行数分钟。Rust (tokio) + MCP Task 天然支持异步长时间操作；OCaml 4.14 缺乏异步手段
4. **渐进增强**：Phase 1-2 使用 Frama-C Server 内置 request 即可覆盖 15+ 个 tool；Phase 3 按需写 OCaml 插件扩展（自然演进到方案 C）

### 与旧决策的关系

| 日期 | 决策 | 状态 |
|------|------|------|
| 2025-02-17 | v2.2 设计：Rust + ZMQ | [已废弃传输层] ZMQ 未安装，改用 Unix Socket。Tool 定义和类型系统保留 |
| 2025-02-18 | 纯 OCaml 插件 (Approach 5) | [已废弃] OCaml MCP 生态不足，异步能力受限 |
| 2025-02-19 | **Rust + Frama-C Server Unix Socket** | **当前选定** |

### 关键技术细节

**Frama-C Server 协议**（非 JSON-RPC）：
- 命令类型：`GET(id,request,data)`, `SET(id,request,data)`, `EXEC(id,request,data)`, `POLL`, `SHUTDOWN`
- 回复类型：`DATA(id,data)`, `ERROR(id,msg)`, `SIGNAL(id)`, `REJECTED(id)`
- 传输：Unix Socket，自定义分帧协议（`S`+3 hex / `L`+7 hex 长度前缀）
- EXEC 是异步的，需通过 POLL 获取中间 SIGNAL 和最终结果

**环境约束**：
- 当前 Frama-C 31.0 未安装 OCaml zmq 包，`-server-zmq` 不可用
- `-server-socket` 和 `-server-batch` 可用
- OCaml 4.14.2（无 OCaml 5 multicore/Eio）

### 参考文档

- [v2.2 设计文档](frama-c-mcp-rust-zmq-设计-v2.2.md) — Tool 定义、参数类型、类型系统、实施路线图（传输层需从 ZMQ 适配为 Unix Socket）
- [设计讨论记录](frama-c-mcp-设计讨论记录.md) — 设计演进过程
- [架构讨论全文](architecture-discussion-full.md) — 五种方案的完整分析

---

## 方案评估历程

### 考虑过的方案

1. **Frama-C Backend + ZMQ** — v2.2 原方案。ZMQ 未安装，改用 Unix Socket
2. **Export CIL to Rust** — 否决（无 CIL 导出/导入机制，状态同步不可持续）
3. **Embed OCaml in Rust** — 否决（实验 3C 证实 Frama-C 无法脱离 dune 嵌入）
4. **Frama-C Calls Rust** — 已验证可行（实验 1A/1B），保留为备选
5. **Pure OCaml Plugin** — 否决（无 MCP SDK，异步能力不足）
6. **Rust + Frama-C Server (Unix Socket)** — **选定**

### FFI 实验结论

| 实验 | 状态 | 结论 |
|------|------|------|
| 1A: Frama-C 插件调 Rust | 通过 | C 桩 + dlopen 可靠 |
| 1B: Rust 回调 OCaml | 通过 | 函数指针方案，JSON 跨边界稳定 |
| 1C: 独立 OCaml↔Rust | 通过 | ctypes.foreign 直接可用 |
| 3A: Rust 嵌入 OCaml | 通过 | ocamlopt -output-complete-obj 可行 |
| 3B: Rust↔OCaml JSON | 通过 | 20 轮 GC 稳定 |
| 3C: Rust 嵌入 Frama-C | 阻塞 | dune 虚拟模块无法脱离 dune 提供 |
