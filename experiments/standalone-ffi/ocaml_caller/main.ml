open Ctypes
open Foreign

(* Load the Rust shared library *)
let rust_lib =
  Dl.dlopen
    ~filename:"../rust_lib/target/release/librust_ffi_demo.so"
    ~flags:[Dl.RTLD_NOW]

(* Bind Rust functions *)
let rust_add =
  foreign ~from:rust_lib "rust_add" (int @-> int @-> returning int)

let rust_greet =
  foreign ~from:rust_lib "rust_greet" (string @-> returning (ptr char))

let rust_free_string =
  foreign ~from:rust_lib "rust_free_string" (ptr char @-> returning void)

let () =
  (* Test 1: integer addition *)
  let result = rust_add 17 25 in
  Printf.printf "rust_add(17, 25) = %d\n" result;
  assert (result = 42);

  (* Test 2: string passing *)
  let greeting_ptr = rust_greet "OCaml" in
  let greeting = coerce (ptr char) string greeting_ptr in
  Printf.printf "rust_greet(\"OCaml\") = \"%s\"\n" greeting;
  assert (greeting = "Hello from Rust, OCaml!");
  rust_free_string greeting_ptr;

  (* Test 3: multiple calls to check for leaks/crashes *)
  for i = 1 to 5 do
    let p = rust_greet (Printf.sprintf "test_%d" i) in
    let s = coerce (ptr char) string p in
    Printf.printf "  round %d: %s\n" i s;
    rust_free_string p
  done;

  Printf.printf "\nAll tests passed!\n"
