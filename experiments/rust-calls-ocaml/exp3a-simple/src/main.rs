use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

extern "C" {
    fn ocaml_startup();
    fn ocaml_fib(n: c_int) -> c_int;
    fn ocaml_greet(name: *const c_char) -> *mut c_char;
}

fn main() {
    // Initialize the OCaml runtime
    unsafe { ocaml_startup() };
    println!("OCaml runtime initialized.");

    // Test fib
    let n = 10;
    let result = unsafe { ocaml_fib(n) };
    println!("fib({}) = {}", n, result);

    // Test greet
    let name = CString::new("Rust").unwrap();
    let greeting_ptr = unsafe { ocaml_greet(name.as_ptr()) };
    if !greeting_ptr.is_null() {
        let greeting = unsafe { CStr::from_ptr(greeting_ptr) };
        println!("greet(\"Rust\") = \"{}\"", greeting.to_str().unwrap());
        // Free the strdup'd string
        unsafe { libc::free(greeting_ptr as *mut libc::c_void) };
    } else {
        eprintln!("ERROR: greet returned NULL");
    }

    println!("\nExperiment 3A: SUCCESS");
}
