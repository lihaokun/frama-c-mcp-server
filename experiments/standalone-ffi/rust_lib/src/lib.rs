use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

/// Simple integer addition — the most basic FFI test.
#[unsafe(no_mangle)]
pub extern "C" fn rust_add(a: c_int, b: c_int) -> c_int {
    a + b
}

/// Accepts a C string, returns a newly allocated C string (caller must free).
/// Demonstrates string passing across the FFI boundary.
#[unsafe(no_mangle)]
pub extern "C" fn rust_greet(name: *const c_char) -> *mut c_char {
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = c_str.to_str().unwrap_or("???");
    let greeting = format!("Hello from Rust, {}!", name_str);
    CString::new(greeting).unwrap().into_raw()
}

/// Free a string previously returned by rust_greet.
#[unsafe(no_mangle)]
pub extern "C" fn rust_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            drop(CString::from_raw(s));
        }
    }
}
