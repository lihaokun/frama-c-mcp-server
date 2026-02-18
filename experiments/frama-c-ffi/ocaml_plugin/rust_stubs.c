/* C stubs for calling Rust functions from OCaml inside a Frama-C plugin.
   Uses dlopen/dlsym to load the Rust shared library at runtime.
   Provides callback helpers so Rust can call back into OCaml via
   function pointers (avoids symbol resolution issues with dlopen). */

#include <caml/mlvalues.h>
#include <caml/memory.h>
#include <caml/alloc.h>
#include <caml/fail.h>
#include <caml/callback.h>
#include <dlfcn.h>
#include <string.h>
#include <stdlib.h>

/* ===== OCaml -> Rust: cached function pointers ===== */

static void *rust_handle = NULL;

typedef int (*rust_add_fn)(int, int);
typedef char *(*rust_analyze_fn)(const char *);
typedef void (*rust_free_string_fn)(char *);
typedef void (*rust_register_callbacks_fn)(
    int (*get_count)(void),
    char *(*get_names_json)(void)
);
typedef char *(*rust_query_ast_fn)(void);

static rust_add_fn fn_rust_add = NULL;
static rust_analyze_fn fn_rust_analyze = NULL;
static rust_free_string_fn fn_rust_free_string = NULL;
static rust_register_callbacks_fn fn_rust_register_callbacks = NULL;
static rust_query_ast_fn fn_rust_query_ast = NULL;

/* ===== Rust -> OCaml: callback helpers (passed as function pointers) ===== */

/* Get the number of functions from OCaml's AST. */
static int ocaml_callback_get_function_count(void) {
    const value *closure = caml_named_value("get_function_count");
    if (!closure) return -1;
    value result = caml_callback(*closure, Val_unit);
    return Int_val(result);
}

/* Get JSON string of function names from OCaml's AST.
   Returns a malloc'd C string that the caller (Rust) must free. */
static char *ocaml_callback_get_function_names_json(void) {
    const value *closure = caml_named_value("get_function_names_json");
    if (!closure) {
        return strdup("{\"error\": \"callback not registered\"}");
    }
    value result = caml_callback(*closure, Val_unit);
    const char *str = String_val(result);
    return strdup(str);
}

/* ===== OCaml-callable stubs ===== */

/* Load the Rust shared library and register callbacks */
CAMLprim value caml_rust_load(value path) {
    CAMLparam1(path);
    const char *lib_path = String_val(path);

    rust_handle = dlopen(lib_path, RTLD_NOW);
    if (!rust_handle) {
        caml_failwith(dlerror());
    }

    fn_rust_add = (rust_add_fn)dlsym(rust_handle, "rust_add");
    fn_rust_analyze = (rust_analyze_fn)dlsym(rust_handle, "rust_analyze");
    fn_rust_free_string = (rust_free_string_fn)dlsym(rust_handle, "rust_free_string");
    fn_rust_register_callbacks = (rust_register_callbacks_fn)dlsym(rust_handle, "rust_register_callbacks");
    fn_rust_query_ast = (rust_query_ast_fn)dlsym(rust_handle, "rust_query_ast");

    if (!fn_rust_add || !fn_rust_analyze || !fn_rust_free_string) {
        caml_failwith("Failed to resolve core Rust symbols");
    }

    /* Register OCaml callbacks with Rust (if available) */
    if (fn_rust_register_callbacks) {
        fn_rust_register_callbacks(
            ocaml_callback_get_function_count,
            ocaml_callback_get_function_names_json
        );
    }

    CAMLreturn(Val_unit);
}

/* Call rust_add(a, b) -> int */
CAMLprim value caml_rust_add(value a, value b) {
    CAMLparam2(a, b);
    int result = fn_rust_add(Int_val(a), Int_val(b));
    CAMLreturn(Val_int(result));
}

/* Call rust_analyze(name) -> string */
CAMLprim value caml_rust_analyze(value name) {
    CAMLparam1(name);
    CAMLlocal1(result);

    char *json = fn_rust_analyze(String_val(name));
    result = caml_copy_string(json);
    fn_rust_free_string(json);

    CAMLreturn(result);
}

/* Call rust_query_ast() -> string (Rust calls back into OCaml during this) */
CAMLprim value caml_rust_query_ast(value unit) {
    CAMLparam1(unit);
    CAMLlocal1(result);

    if (!fn_rust_query_ast) {
        caml_failwith("rust_query_ast not available");
    }

    char *json = fn_rust_query_ast();
    result = caml_copy_string(json);
    fn_rust_free_string(json);

    CAMLreturn(result);
}
