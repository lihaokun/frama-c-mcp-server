use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

/// Simple addition to verify FFI works inside Frama-C process.
#[unsafe(no_mangle)]
pub extern "C" fn rust_add(a: c_int, b: c_int) -> c_int {
    a + b
}

/// Analyze a C source snippet (placeholder for future real analysis).
/// Takes a function name, returns a JSON-formatted analysis result.
#[unsafe(no_mangle)]
pub extern "C" fn rust_analyze(func_name: *const c_char) -> *mut c_char {
    let name = unsafe { CStr::from_ptr(func_name) }
        .to_str()
        .unwrap_or("unknown");
    let json = format!(
        r#"{{"function": "{}", "status": "analyzed", "engine": "rust"}}"#,
        name
    );
    CString::new(json).unwrap().into_raw()
}

/// Free a string returned by Rust functions.
#[unsafe(no_mangle)]
pub extern "C" fn rust_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            drop(CString::from_raw(s));
        }
    }
}

// ===== Callback mechanism via function pointers =====

/// Type aliases for the callback function pointers passed from C.
type GetCountFn = unsafe extern "C" fn() -> c_int;
type GetNamesJsonFn = unsafe extern "C" fn() -> *mut c_char;

/// Stored callback pointers (set by rust_register_callbacks).
static mut CB_GET_COUNT: Option<GetCountFn> = None;
static mut CB_GET_NAMES_JSON: Option<GetNamesJsonFn> = None;

/// Register callback function pointers from the C/OCaml side.
/// Must be called before rust_query_ast.
#[unsafe(no_mangle)]
pub extern "C" fn rust_register_callbacks(
    get_count: GetCountFn,
    get_names_json: GetNamesJsonFn,
) {
    unsafe {
        CB_GET_COUNT = Some(get_count);
        CB_GET_NAMES_JSON = Some(get_names_json);
    }
}

/// Query the OCaml/Frama-C AST by calling back into OCaml via registered callbacks.
/// Returns a JSON string summarizing the AST information.
#[unsafe(no_mangle)]
pub extern "C" fn rust_query_ast() -> *mut c_char {
    let (count, names_json) = unsafe {
        let count = match CB_GET_COUNT {
            Some(f) => f(),
            None => -1,
        };

        let names_json = match CB_GET_NAMES_JSON {
            Some(f) => {
                let ptr = f();
                if ptr.is_null() {
                    "[]".to_string()
                } else {
                    let s = CStr::from_ptr(ptr)
                        .to_str()
                        .unwrap_or("[]")
                        .to_string();
                    libc::free(ptr as *mut libc::c_void);
                    s
                }
            }
            None => "[]".to_string(),
        };

        (count, names_json)
    };

    let result = format!(
        r#"{{"source": "rust_via_ocaml_callback", "function_count": {}, "function_names": {}}}"#,
        count, names_json
    );
    CString::new(result).unwrap().into_raw()
}
