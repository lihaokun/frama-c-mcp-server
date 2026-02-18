# OCaml ↔ Rust FFI 实验报告

> 2025-02-18 完成。验证 Frama-C 插件调用 Rust 的可行性，作为架构备选方案（Approach 4）的技术储备。

## 环境

| 组件 | 版本 |
|------|------|
| OCaml | 4.14.2 |
| dune | 3.20.2 |
| opam switch | `frama` |
| Frama-C | 31.0 (Gallium) |
| Rust | nightly 1.95.0 |
| ctypes | 0.24.0 |
| ctypes-foreign | 0.24.0（需单独安装） |

## 实验 1C：独立 OCaml ↔ Rust FFI

**路径**：`experiments/standalone-ffi/`

**目标**：在最简环境下验证 OCaml 通过 C ABI 调用 Rust 函数。

**架构**：

```
OCaml (ctypes.foreign)  →  Dl.dlopen  →  Rust cdylib (.so)
```

**Rust 侧**（`rust_lib/src/lib.rs`）导出三个 `extern "C"` 函数：

- `rust_add(a, b) → int` — 整数加法
- `rust_greet(name) → char*` — 接收 C 字符串，返回新分配的问候字符串
- `rust_free_string(s)` — 释放 Rust 分配的字符串

**OCaml 侧**（`ocaml_caller/main.ml`）通过 `ctypes` + `ctypes-foreign` 绑定：

```ocaml
let rust_lib = Dl.dlopen ~filename:"..." ~flags:[Dl.RTLD_NOW]
let rust_add = foreign ~from:rust_lib "rust_add" (int @-> int @-> returning int)
```

**测试结果**：

```
rust_add(17, 25) = 42
rust_greet("OCaml") = "Hello from Rust, OCaml!"
  round 1..5: 均正确
All tests passed!
```

**结论**：独立环境下 OCaml 通过 `ctypes.foreign` 调用 Rust 非常直接，整数和字符串跨边界传递均正常。

---

## 实验 1A：Frama-C 插件调用 Rust

**路径**：`experiments/frama-c-ffi/`

**目标**：在 Frama-C 插件（`.cmxs`）中加载 Rust `.so` 并调用，同时使用 Frama-C AST API。

**架构**：

```
Frama-C 进程
  └── Ffi_test.cmxs (OCaml 插件)
        ├── rust_stubs.c (C 桩：dlopen/dlsym)
        ├── Ffi_test.ml (OCaml：external 声明 + Frama-C API)
        └── dlopen → libframa_rust_ffi.so (Rust cdylib)
```

### 踩坑：ctypes-foreign 不可用于 Frama-C 插件

初始尝试在插件中使用 `ctypes.foreign`（与实验 1C 相同方式），结果加载 `.cmxs` 时报错：

```
undefined symbol: camlLibffi_abi__43
```

**原因**：`ctypes-foreign` 依赖 `libffi` 的 OCaml 绑定模块 `Libffi_abi`。该模块在独立程序中由链接器链入，但 Frama-C 进程本身没有链接 `ctypes-foreign`，`.cmxs` 动态加载时找不到这个符号。

**解决方案**：放弃 `ctypes-foreign`，改用 C 桩 + `dlopen`/`dlsym`：

```c
// rust_stubs.c
static int (*fn_rust_add)(int, int) = NULL;

CAMLprim value caml_rust_load(value path) {
    rust_handle = dlopen(String_val(path), RTLD_NOW);
    fn_rust_add = dlsym(rust_handle, "rust_add");
    ...
}

CAMLprim value caml_rust_add(value a, value b) {
    int result = fn_rust_add(Int_val(a), Int_val(b));
    return Val_int(result);
}
```

```ocaml
(* Ffi_test.ml *)
open Frama_c_kernel
external rust_load : string -> unit = "caml_rust_load"
external rust_add : int -> int -> int = "caml_rust_add"
```

**测试结果**：

```
[kernel] === FFI Test Plugin ===
[kernel] Rust: rust_add(100, 200) = 300
[kernel] Functions in the C file:
[kernel] - add
[kernel]     Rust analysis: {"function": "add", "status": "analyzed", "engine": "rust"}
[kernel] - factorial
[kernel]     Rust analysis: {"function": "factorial", ...}
[kernel] - main
[kernel] - swap
[kernel] === FFI Test Complete ===
```

**结论**：C 桩方案可靠工作。插件成功同时调用 Rust 函数和 Frama-C AST API。

---

## 实验 1B：Rust 回调 OCaml

**路径**：在实验 1A 基础上扩展（同一目录）

**目标**：Rust 通过回调获取 Frama-C AST 数据（函数计数、函数名列表），验证 JSON 字符串跨边界传递。

**架构**：

```
OCaml                        C 桩                           Rust
─────                        ────                           ────
Callback.register            caml_named_value()
  "get_function_count"       caml_callback()
  "get_function_names_json"       │
                                  │
                             rust_register_callbacks(       保存函数指针
                               get_count_ptr,               CB_GET_COUNT
                               get_names_json_ptr)          CB_GET_NAMES_JSON
                                                                │
rust_query_ast() ──────────────────────────────────→ 调用 CB_GET_COUNT()
  ← 返回 JSON 字符串 ←─────────────────────────────── 调用 CB_GET_NAMES_JSON()
```

### 踩坑：不能在 Rust 中用 extern 声明宿主进程的符号

初始尝试在 Rust 中直接声明：

```rust
extern "C" {
    fn ocaml_callback_get_function_count() -> c_int;
}
```

这些符号定义在 C 桩中（`.cmxs` 的一部分），存在于 Frama-C 进程内。但 Rust `.so` 是通过 `dlopen` 加载的，**`dlopen` 默认不会从宿主进程解析未定义符号**（除非宿主以 `-rdynamic` 链接或使用 `RTLD_GLOBAL`）。

**解决方案**：改用函数指针传递：

1. C 桩定义回调辅助函数（`static`，不需要导出）
2. 加载 Rust `.so` 后，调用 `rust_register_callbacks(fn_ptr1, fn_ptr2)` 将函数指针传给 Rust
3. Rust 保存函数指针到全局变量，需要时调用

**字符串内存管理约定**：

| 方向 | 分配方 | 释放方 | 机制 |
|------|--------|--------|------|
| OCaml → Rust | OCaml (`caml_copy_string`) | — | C 桩用 `String_val()` 读取，`strdup()` 复制 |
| Rust → OCaml | Rust (`CString::into_raw`) | Rust (`rust_free_string`) | C 桩拿到后 `caml_copy_string`，再调 `rust_free_string` |
| OCaml callback → Rust | C 桩 (`strdup`) | Rust (`libc::free`) | 回调返回的是 `strdup` 的副本 |

**测试结果**：

```
[kernel] --- Rust querying OCaml AST via callbacks ---
[kernel] Rust AST query result: {"source": "rust_via_ocaml_callback",
         "function_count": 4, "function_names": ["add", "factorial", "main", "swap"]}
[kernel] --- Multiple callback round-trips ---
[kernel]   Round 1: {..., "function_count": 4, ...}
[kernel]   Round 2: {..., "function_count": 4, ...}
[kernel]   Round 3: {..., "function_count": 4, ...}
[kernel] === FFI Test Complete ===
```

**结论**：函数指针方案稳定工作，JSON 字符串跨 OCaml → C → Rust → C → OCaml 边界正确传递，多轮调用无泄漏无崩溃。

---

## Frama-C 外部插件构建模式

```makefile
# 1. 获取 include 路径（仅 -I，不用 -linkpkg）
OCAML_FLAGS := $(shell ocamlfind query -r -i-format frama-c.kernel)

# 2. 编译 C 桩
ocamlfind ocamlopt -c rust_stubs.c

# 3. 编译 OCaml（需 open Frama_c_kernel）
ocamlfind ocamlopt $(OCAML_FLAGS) -thread -c Ffi_test.ml

# 4. 链接为 .cmxs
ocamlfind ocamlopt $(OCAML_FLAGS) -thread -shared \
  rust_stubs.o Ffi_test.cmx -cclib -ldl -o Ffi_test.cmxs

# 5. 运行
frama-c test.c -load-module Ffi_test.cmxs
```

关键点：
- `.cmxs` 是动态加载到 Frama-C 进程的共享库，**不链接** Frama-C 内核（符号在运行时已存在）
- 必须 `open Frama_c_kernel` 才能访问 `Kernel`、`Globals`、`Kernel_function`、`Boot` 等模块
- 插件入口：`Boot.Main.extend run`（在 AST 解析完成后执行）
- 遍历函数：`Globals.Functions.iter (fun kf -> Kernel_function.get_name kf)`

## 关键教训总结

| 问题 | 原因 | 解决方案 |
|------|------|---------|
| `ctypes-foreign` 在 Frama-C 插件中报 `undefined symbol: camlLibffi_abi` | Frama-C 进程未链接 `ctypes-foreign` 及其 `libffi` 绑定 | 用 C 桩 + `dlopen`/`dlsym` 替代 |
| Rust `.so` 中 `extern "C"` 声明的宿主符号无法解析 | `dlopen` 默认不从宿主进程解析未定义符号 | 改用函数指针传递（`rust_register_callbacks`） |
| `ctypes-foreign` 未找到 | `ctypes-foreign` 是独立 opam 包，不随 `ctypes` 安装 | `opam install ctypes-foreign` |
| 插件编译时 `Unbound module Kernel` | 未加 Frama-C 内核的 include 路径 | `ocamlfind query -r -i-format frama-c.kernel` 获取 `-I` 路径 |
