(* Frama-C plugin that:
   1. Loads a Rust shared library and calls its functions
   2. Uses Frama-C's AST API to iterate over C functions
   3. Registers OCaml callbacks so Rust can query the AST back *)

open Frama_c_kernel

(* --- Rust FFI bindings via C stubs --- *)

external rust_load : string -> unit = "caml_rust_load"
external rust_add : int -> int -> int = "caml_rust_add"
external rust_analyze : string -> string = "caml_rust_analyze"
external rust_query_ast : unit -> string = "caml_rust_query_ast"

(* --- OCaml callbacks (called by Rust via C helpers) --- *)

let get_function_count () =
  let count = ref 0 in
  Globals.Functions.iter (fun _kf -> incr count);
  !count

let get_function_names_json () =
  let names = ref [] in
  Globals.Functions.iter (fun kf ->
    names := Printf.sprintf "%S" (Kernel_function.get_name kf) :: !names
  );
  let json_array = String.concat ", " (List.rev !names) in
  Printf.sprintf "[%s]" json_array

(* Register callbacks so Rust can find them via caml_named_value *)
let () = Callback.register "get_function_count" get_function_count
let () = Callback.register "get_function_names_json" get_function_names_json

(* --- Plugin logic --- *)

let run () =
  (* Load the Rust library *)
  let lib_path =
    let dir = Sys.getenv_opt "RUST_LIB_DIR" |> Option.value ~default:"." in
    Filename.concat dir "libframa_rust_ffi.so"
  in
  (try rust_load lib_path
   with Failure msg ->
     Kernel.fatal "Cannot load Rust library %s: %s" lib_path msg);

  Kernel.feedback "=== FFI Test Plugin (with callbacks) ===";

  (* Test 1: OCaml -> Rust: integer addition *)
  let sum = rust_add 100 200 in
  Kernel.feedback "Rust: rust_add(100, 200) = %d" sum;

  (* Test 2: OCaml -> Rust: per-function analysis *)
  Kernel.feedback "Functions in the C file:";
  Globals.Functions.iter (fun kf ->
    let name = Kernel_function.get_name kf in
    Kernel.feedback "  - %s" name;
    let result = rust_analyze name in
    Kernel.feedback "    Rust analysis: %s" result
  );

  (* Test 3: OCaml -> Rust -> OCaml: Rust queries the AST via callbacks *)
  Kernel.feedback "--- Rust querying OCaml AST via callbacks ---";
  let ast_info = rust_query_ast () in
  Kernel.feedback "Rust AST query result: %s" ast_info;

  (* Test 4: Multiple callback round-trips to verify stability *)
  Kernel.feedback "--- Multiple callback round-trips ---";
  for i = 1 to 3 do
    let info = rust_query_ast () in
    Kernel.feedback "  Round %d: %s" i info
  done;

  Kernel.feedback "=== FFI Test Complete ==="

(* Register as a Frama-C extension that runs after the AST is computed *)
let () = Boot.Main.extend run
