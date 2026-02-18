use serde_json::{json, Value};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

extern "C" {
    fn ocaml_startup();
    fn ocaml_process_json(json_input: *const c_char) -> *mut c_char;
}

fn call_ocaml(request: &Value) -> Result<Value, String> {
    let json_str = CString::new(request.to_string()).map_err(|e| e.to_string())?;
    let result_ptr = unsafe { ocaml_process_json(json_str.as_ptr()) };
    if result_ptr.is_null() {
        return Err("OCaml returned NULL".to_string());
    }
    let result_str = unsafe { CStr::from_ptr(result_ptr) }
        .to_str()
        .map_err(|e| e.to_string())?
        .to_string();
    unsafe { libc::free(result_ptr as *mut libc::c_void) };
    serde_json::from_str(&result_str).map_err(|e| e.to_string())
}

fn main() {
    unsafe { ocaml_startup() };
    println!("OCaml runtime initialized.\n");

    // Test 1: Stats command
    println!("=== Test 1: Stats ===");
    let req = json!({"command": "stats", "data": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]});
    println!("Request:  {}", req);
    match call_ocaml(&req) {
        Ok(resp) => {
            println!("Response: {}", resp);
            assert_eq!(resp["status"], "ok");
            assert_eq!(resp["sum"], 55);
            assert_eq!(resp["count"], 10);
            assert_eq!(resp["min"], 1);
            assert_eq!(resp["max"], 10);
            println!("PASS\n");
        }
        Err(e) => {
            eprintln!("FAIL: {}\n", e);
            std::process::exit(1);
        }
    }

    // Test 2: Echo command
    println!("=== Test 2: Echo ===");
    let req = json!({"command": "echo", "data": {"nested": true, "value": 42}});
    println!("Request:  {}", req);
    match call_ocaml(&req) {
        Ok(resp) => {
            println!("Response: {}", resp);
            assert_eq!(resp["status"], "ok");
            assert_eq!(resp["echoed"]["nested"], true);
            assert_eq!(resp["echoed"]["value"], 42);
            println!("PASS\n");
        }
        Err(e) => {
            eprintln!("FAIL: {}\n", e);
            std::process::exit(1);
        }
    }

    // Test 3: Transform command
    println!("=== Test 3: Transform (string reversal) ===");
    let req = json!({"command": "transform", "data": ["hello", "world", "OCaml"]});
    println!("Request:  {}", req);
    match call_ocaml(&req) {
        Ok(resp) => {
            println!("Response: {}", resp);
            assert_eq!(resp["status"], "ok");
            assert_eq!(resp["result"][0], "olleh");
            assert_eq!(resp["result"][1], "dlrow");
            assert_eq!(resp["result"][2], "lmaCO");
            println!("PASS\n");
        }
        Err(e) => {
            eprintln!("FAIL: {}\n", e);
            std::process::exit(1);
        }
    }

    // Test 4: Unknown command (error handling)
    println!("=== Test 4: Unknown command ===");
    let req = json!({"command": "bogus"});
    println!("Request:  {}", req);
    match call_ocaml(&req) {
        Ok(resp) => {
            println!("Response: {}", resp);
            assert_eq!(resp["status"], "error");
            println!("PASS\n");
        }
        Err(e) => {
            eprintln!("FAIL: {}\n", e);
            std::process::exit(1);
        }
    }

    // Test 5: GC stability — 20 rounds of mixed calls
    println!("=== Test 5: GC stability (20 rounds) ===");
    for i in 0..20 {
        let data: Vec<i64> = (1..=(i + 1) * 10).collect();
        let req = json!({"command": "stats", "data": data});
        let resp = call_ocaml(&req).expect("call_ocaml failed in GC test");
        assert_eq!(resp["status"], "ok");

        let strings: Vec<String> = (0..i + 1)
            .map(|j| format!("round{}item{}", i, j))
            .collect();
        let req2 = json!({"command": "transform", "data": strings});
        let resp2 = call_ocaml(&req2).expect("call_ocaml failed in GC test (transform)");
        assert_eq!(resp2["status"], "ok");

        if (i + 1) % 5 == 0 {
            println!("  Round {}/20 OK", i + 1);
        }
    }
    println!("PASS\n");

    println!("Experiment 3B: ALL TESTS PASSED");
}
