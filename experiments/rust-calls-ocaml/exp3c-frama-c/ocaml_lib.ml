(* Frama-C embedded in Rust: register callbacks for Rust to call *)

open Frama_c_kernel
open Yojson.Safe

(* Initialize Frama-C and parse a C file *)
let parse_c_file filename =
  try
    (* Set the file to parse via Frama-C's kernel parameters *)
    Kernel.Files.set [Filepath.of_string filename];
    (* Force AST computation (this triggers parsing) *)
    Ast.compute ();
    to_string (`Assoc [
      "status", `String "ok";
      "message", `String ("Parsed: " ^ filename);
    ])
  with e ->
    to_string (`Assoc [
      "status", `String "error";
      "message", `String (Printexc.to_string e);
    ])

(* List all functions in the parsed AST *)
let list_functions () =
  try
    let funcs = ref [] in
    Globals.Functions.iter (fun kf ->
      let name = Kernel_function.get_name kf in
      let loc = Kernel_function.get_location kf in
      let file = Filepath.to_string (fst loc).Filepath.pos_path in
      let line = (fst loc).Filepath.pos_lnum in
      let is_def = Kernel_function.is_definition kf in
      funcs := (`Assoc [
        "name", `String name;
        "file", `String file;
        "line", `Int line;
        "is_definition", `Bool is_def;
      ]) :: !funcs
    );
    to_string (`Assoc [
      "status", `String "ok";
      "functions", `List (List.rev !funcs);
    ])
  with e ->
    to_string (`Assoc [
      "status", `String "error";
      "message", `String (Printexc.to_string e);
    ])

(* Get details about a specific function *)
let get_function_info func_name =
  try
    let kf = Globals.Functions.find_by_name func_name in
    let name = Kernel_function.get_name kf in
    let is_def = Kernel_function.is_definition kf in
    let return_type = Kernel_function.get_return_type kf in
    let ret_str = Format.asprintf "%a" Printer.pp_typ return_type in
    let params = Kernel_function.get_formals kf in
    let param_list = List.map (fun vi ->
      `Assoc [
        "name", `String vi.Cil_types.vname;
        "type", `String (Format.asprintf "%a" Printer.pp_typ vi.Cil_types.vtype);
      ]
    ) params in
    to_string (`Assoc [
      "status", `String "ok";
      "name", `String name;
      "is_definition", `Bool is_def;
      "return_type", `String ret_str;
      "parameters", `List param_list;
    ])
  with
  | Not_found ->
    to_string (`Assoc [
      "status", `String "error";
      "message", `String ("Function not found: " ^ func_name);
    ])
  | e ->
    to_string (`Assoc [
      "status", `String "error";
      "message", `String (Printexc.to_string e);
    ])

let () =
  Callback.register "parse_c_file" parse_c_file;
  Callback.register "list_functions" list_functions;
  Callback.register "get_function_info" get_function_info
