use serde_json::Value;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;

extern "C" {
    fn ocaml_startup();
    fn ocaml_parse_c_file(filename: *const c_char) -> *mut c_char;
    fn ocaml_list_functions() -> *mut c_char;
    fn ocaml_get_function_info(func_name: *const c_char) -> *mut c_char;
}

fn call_json(ptr: *mut c_char) -> Value {
    assert!(!ptr.is_null(), "Received NULL from OCaml");
    let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
    unsafe { libc::free(ptr as *mut libc::c_void) };
    serde_json::from_str(&s).unwrap()
}

fn main() {
    println!("=== Experiment 3C: Rust embeds Frama-C ===\n");

    // Initialize OCaml runtime (which also runs Frama-C Boot.play_analysis)
    println!("Initializing OCaml runtime + Frama-C kernel...");
    unsafe { ocaml_startup() };
    println!("Initialization complete.\n");

    // Resolve the path to test.c relative to the binary's original source
    let test_c = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test.c");
    let test_c_str = test_c.to_str().unwrap();
    println!("Parsing: {}", test_c_str);

    // Step 1: Parse the C file
    let c_path = CString::new(test_c_str).unwrap();
    let resp = call_json(unsafe { ocaml_parse_c_file(c_path.as_ptr()) });
    println!("Parse result: {}", resp);
    assert_eq!(resp["status"], "ok", "Failed to parse C file: {}", resp);
    println!("PASS: C file parsed successfully.\n");

    // Step 2: List all functions
    println!("Listing functions in AST...");
    let resp = call_json(unsafe { ocaml_list_functions() });
    println!("Functions: {}", serde_json::to_string_pretty(&resp).unwrap());
    assert_eq!(resp["status"], "ok", "Failed to list functions: {}", resp);
    let functions = resp["functions"].as_array().unwrap();
    let func_names: Vec<&str> = functions
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    println!("Found {} function(s): {:?}", func_names.len(), func_names);
    assert!(
        func_names.contains(&"factorial"),
        "Expected 'factorial' in function list"
    );
    assert!(
        func_names.contains(&"add"),
        "Expected 'add' in function list"
    );
    assert!(
        func_names.contains(&"main"),
        "Expected 'main' in function list"
    );
    println!("PASS: All expected functions found.\n");

    // Step 3: Get details about 'factorial'
    println!("Getting info for 'factorial'...");
    let name = CString::new("factorial").unwrap();
    let resp = call_json(unsafe { ocaml_get_function_info(name.as_ptr()) });
    println!(
        "factorial info: {}",
        serde_json::to_string_pretty(&resp).unwrap()
    );
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["return_type"], "int");
    let params = resp["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1);
    assert_eq!(params[0]["name"], "n");
    assert_eq!(params[0]["type"], "int");
    println!("PASS: factorial info correct.\n");

    // Step 4: Get details about 'add'
    println!("Getting info for 'add'...");
    let name = CString::new("add").unwrap();
    let resp = call_json(unsafe { ocaml_get_function_info(name.as_ptr()) });
    println!("add info: {}", serde_json::to_string_pretty(&resp).unwrap());
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["return_type"], "int");
    let params = resp["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 2);
    println!("PASS: add info correct.\n");

    // Step 5: Query a non-existent function
    println!("Querying non-existent function 'bogus'...");
    let name = CString::new("bogus").unwrap();
    let resp = call_json(unsafe { ocaml_get_function_info(name.as_ptr()) });
    println!("bogus result: {}", resp);
    assert_eq!(resp["status"], "error");
    println!("PASS: Error handled correctly.\n");

    println!("=== Experiment 3C: ALL TESTS PASSED ===");
}
