(** ast_utils_register.ml — Frama-C plugin registration + server request handlers.

    Registers the ast-utils plugin and exposes:
    - getFunctionAst: function name → JSON AST
    - getAcslValidation: parse + type-check ACSL without inserting
    - execAddAnnotation: parse + type-check + insert ACSL
    - execRemoveAnnotations: remove all ast-utils emitter annotations
    - execInsertGhostStmt: insert ghost declaration or assignment
    - execRemoveGhostStmt: remove a single ghost statement by sid
    - execRemoveGhostVar: remove a ghost variable and all its statements
    - getVcDetails: get WP verification condition details (sequent)
    - execCreateSandbox: deep copy function for CEGIS experimentation
    - execResetSandbox: delete + recreate sandbox preserving experiment ID
    - execDeleteSandbox: remove sandbox function from AST
    - execExtractAnnotations: extract our emitter's annotations from sandbox *)

open Frama_c_kernel
open Cil_types

(* ====== Plugin registration ====== *)

module Self = Plugin.Register (struct
  let name = "AST Utils"
  let shortname = "ast-utils"
  let help = "CIL AST JSON serialization and ACSL annotation manipulation"
end)

(* ====== Helpers ====== *)

let find_kf name =
  try Ok (Globals.Functions.find_by_name name)
  with Not_found ->
    Error (Printf.sprintf "function '%s' not found" name)

let find_stmt kf sid =
  try
    let fundec = Kernel_function.get_definition kf in
    Ok (List.find (fun s -> s.sid = sid) fundec.sallstmts)
  with
  | Kernel_function.No_Definition ->
    Error (Printf.sprintf "function '%s' has no definition"
             (Kernel_function.get_name kf))
  | Not_found ->
    Error (Printf.sprintf "statement %d not found" sid)

let error_json msg : Json.t =
  `Assoc [("error", `String msg)]

let ok_json (payload : (string * Json.t) list) : Json.t =
  `Assoc payload

(** Extract original function name from sandbox name.
    "__sandbox__copy_counter_a3f2e1b7" → "copy_counter" *)
let original_name_of_sandbox sandbox_name =
  let prefix = "__sandbox__" in
  let plen = String.length prefix in
  let slen = String.length sandbox_name in
  if slen > plen + 9 && String.sub sandbox_name 0 plen = prefix then
    (* strip prefix and _<8-char hash> suffix *)
    Some (String.sub sandbox_name plen (slen - plen - 9))
  else
    None

let sid_map_to_json sid_map : Json.t =
  `List (List.map (fun (orig, sandbox) ->
    `List [`Int orig; `Int sandbox]
  ) sid_map)

(* ====== Server package ====== *)

let package =
  Server.Package.package
    ~plugin:"ast-utils"
    ~title:"AST Utils"
    ~descr:(Markdown.plain
              "CIL AST JSON serialization and ACSL annotation manipulation")
    ()

(* ====== getFunctionAst ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`GET
    ~name:"getFunctionAst"
    ~descr:(Markdown.plain "Get JSON representation of a function's CIL AST")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun name ->
       match find_kf name with
       | Error msg -> error_json msg
       | Ok kf ->
         try
           let ast = Ast_utils_ast.get_function_ast kf in
           (ast :> Json.t)
         with Kernel_function.No_Definition ->
           error_json (Printf.sprintf "function '%s' has no definition" name))

(* ====== getAcslValidation ====== *)

let () =
  let s = Server.Request.signature () in
  let get_function = Server.Request.param s
    ~name:"function" ~descr:(Markdown.plain "Function name")
    (module Server.Data.Jstring) in
  let get_kind = Server.Request.param s
    ~name:"kind" ~descr:(Markdown.plain "\"spec\" or \"annot\"")
    (module Server.Data.Jstring) in
  let get_acsl = Server.Request.param s
    ~name:"acsl" ~descr:(Markdown.plain "ACSL string to validate")
    (module Server.Data.Jstring) in
  let get_stmt = Server.Request.param_opt s
    ~name:"stmt" ~descr:(Markdown.plain "Statement ID (required for kind=annot)")
    (module Server.Data.Jint) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Validation result")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`GET
    ~name:"getAcslValidation"
    ~descr:(Markdown.plain
              "Parse and type-check ACSL string without inserting into AST")
    (fun rq () ->
       let fname = get_function rq in
       let kind = get_kind rq in
       let acsl = get_acsl rq in
       let stmt_opt = get_stmt rq in
       let result =
         match find_kf fname with
         | Error msg -> error_json msg
         | Ok kf ->
           match kind with
           | "spec" ->
             (match Ast_utils_core.type_spec kf acsl with
              | Ok _ ->
                ok_json [("valid", `Bool true); ("error", `Null)]
              | Error msg ->
                ok_json [("valid", `Bool false); ("error", `String msg)])
           | "annot" ->
             (match stmt_opt with
              | None ->
                error_json "stmt parameter required for kind=annot"
              | Some sid ->
                match find_stmt kf sid with
                | Error msg -> error_json msg
                | Ok stmt ->
                  match Ast_utils_core.type_annot kf stmt acsl with
                  | Ok _ ->
                    ok_json [("valid", `Bool true); ("error", `Null)]
                  | Error msg ->
                    ok_json [("valid", `Bool false); ("error", `String msg)])
           | _ ->
             error_json (Printf.sprintf "unknown kind '%s', expected 'spec' or 'annot'" kind)
       in
       set_result rq result)

(* ====== execAddAnnotation ====== *)

let () =
  let s = Server.Request.signature () in
  let get_function = Server.Request.param s
    ~name:"function" ~descr:(Markdown.plain "Function name")
    (module Server.Data.Jstring) in
  let get_kind = Server.Request.param s
    ~name:"kind" ~descr:(Markdown.plain "\"spec\" or \"annot\"")
    (module Server.Data.Jstring) in
  let get_acsl = Server.Request.param s
    ~name:"acsl" ~descr:(Markdown.plain "ACSL string to add")
    (module Server.Data.Jstring) in
  let get_stmt = Server.Request.param_opt s
    ~name:"stmt" ~descr:(Markdown.plain "Statement ID (required for kind=annot)")
    (module Server.Data.Jint) in
  let get_label = Server.Request.param_opt s
    ~name:"label" ~descr:(Markdown.plain "Label to inject into pred_name (optional)")
    (module Server.Data.Jstring) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Operation result")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`EXEC
    ~name:"execAddAnnotation"
    ~descr:(Markdown.plain
              "Parse, type-check and insert ACSL annotation into CIL AST")
    (fun rq () ->
       let fname = get_function rq in
       let kind = get_kind rq in
       let acsl = get_acsl rq in
       let stmt_opt = get_stmt rq in
       let label = get_label rq in
       let result =
         match find_kf fname with
         | Error msg -> error_json msg
         | Ok kf ->
           match kind with
           | "spec" ->
             (match Ast_utils_core.type_spec ?label kf acsl with
              | Ok funspec ->
                Ast_utils_core.insert_spec kf funspec;
                ok_json [("success", `Bool true); ("error", `Null)]
              | Error msg ->
                ok_json [("success", `Bool false); ("error", `String msg)])
           | "annot" ->
             (match stmt_opt with
              | None ->
                error_json "stmt parameter required for kind=annot"
              | Some sid ->
                match find_stmt kf sid with
                | Error msg -> error_json msg
                | Ok stmt ->
                  match Ast_utils_core.type_annot ?label kf stmt acsl with
                  | Ok annots ->
                    (try
                       Ast_utils_core.insert_annots kf stmt annots;
                       ok_json [("success", `Bool true); ("error", `Null);
                                ("count", `Int (List.length annots))]
                     with Invalid_argument msg ->
                       ok_json [("success", `Bool false); ("error", `String msg)])
                  | Error msg ->
                    ok_json [("success", `Bool false); ("error", `String msg)])
           | _ ->
             error_json (Printf.sprintf "unknown kind '%s', expected 'spec' or 'annot'" kind)
       in
       set_result rq result)

(* ====== execRemoveAnnotations ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`EXEC
    ~name:"execRemoveAnnotations"
    ~descr:(Markdown.plain
              "Remove all annotations added by ast-utils emitter from a function")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun name ->
       match find_kf name with
       | Error msg -> error_json msg
       | Ok kf ->
         let n = Ast_utils_core.remove kf in
         ok_json [("success", `Bool true); ("removed_count", `Int n)])

(* ====== execRemoveAnnotationByLabel ====== *)

let () =
  let s = Server.Request.signature () in
  let get_function = Server.Request.param s
    ~name:"function" ~descr:(Markdown.plain "Function name")
    (module Server.Data.Jstring) in
  let get_label = Server.Request.param s
    ~name:"label" ~descr:(Markdown.plain "Hash label of annotation to remove")
    (module Server.Data.Jstring) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Operation result")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`EXEC
    ~name:"execRemoveAnnotationByLabel"
    ~descr:(Markdown.plain
              "Remove a single annotation identified by its hash_label")
    (fun rq () ->
       let fname = get_function rq in
       let label = get_label rq in
       let result =
         match find_kf fname with
         | Error msg -> error_json msg
         | Ok kf ->
           match Ast_utils_core.remove_annotation_by_label kf label with
           | true ->
             ok_json [("success", `Bool true); ("error", `Null)]
           | false ->
             ok_json [("success", `Bool false);
                      ("error", `String "annotation not found")]
       in
       set_result rq result)

(* ====== execInsertGhostStmt ====== *)

let () =
  let s = Server.Request.signature () in
  let get_function = Server.Request.param s
    ~name:"function" ~descr:(Markdown.plain "Function name")
    (module Server.Data.Jstring) in
  let get_stmt = Server.Request.param s
    ~name:"stmt" ~descr:(Markdown.plain "Statement ID to insert before")
    (module Server.Data.Jint) in
  let get_op = Server.Request.param s
    ~name:"op" ~descr:(Markdown.plain "\"decl\" or \"set\"")
    (module Server.Data.Jstring) in
  let get_name = Server.Request.param s
    ~name:"name" ~descr:(Markdown.plain "Variable name")
    (module Server.Data.Jstring) in
  let get_type = Server.Request.param_opt s
    ~name:"type" ~descr:(Markdown.plain "Type name (default: \"int\", for op=decl)")
    (module Server.Data.Jstring) in
  let get_expr = Server.Request.param s
    ~name:"expr" ~descr:(Markdown.plain "Expression string")
    (module Server.Data.Jstring) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Operation result")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`EXEC
    ~name:"execInsertGhostStmt"
    ~descr:(Markdown.plain
              "Insert a ghost declaration or assignment before a statement")
    (fun rq () ->
       let fname = get_function rq in
       let sid = get_stmt rq in
       let op = get_op rq in
       let var_name = get_name rq in
       let type_name = match get_type rq with Some t -> t | None -> "int" in
       let expr_str = get_expr rq in
       let result =
         match find_kf fname with
         | Error msg -> error_json msg
         | Ok kf ->
           match find_stmt kf sid with
           | Error msg -> error_json msg
           | Ok stmt ->
             let fundec =
               try Kernel_function.get_definition kf
               with Kernel_function.No_Definition ->
                 failwith "function has no definition"
             in
             let loc = Cil_datatype.Stmt.loc stmt in
             match op with
             | "decl" ->
               (match Ast_utils_ghost.resolve_ghost_type type_name with
                | Error msg -> error_json msg
                | Ok typ ->
                  match Ast_utils_ghost.parse_c_expr fundec loc expr_str with
                  | Error msg -> error_json msg
                  | Ok init_exp ->
                    match Ast_utils_ghost.insert_ghost_decl
                            kf stmt var_name typ init_exp with
                    | Error msg ->
                      ok_json [("success", `Bool false);
                               ("sid", `Null);
                               ("error", `String msg)]
                    | Ok new_stmt ->
                      ok_json [("success", `Bool true);
                               ("sid", `Int new_stmt.sid);
                               ("error", `Null)])
             | "set" ->
               (match Ast_utils_ghost.parse_c_expr fundec loc expr_str with
                | Error msg -> error_json msg
                | Ok expr ->
                  match Ast_utils_ghost.insert_ghost_assign
                          kf stmt var_name expr with
                  | Error msg ->
                    ok_json [("success", `Bool false);
                             ("sid", `Null);
                             ("error", `String msg)]
                  | Ok new_stmt ->
                    ok_json [("success", `Bool true);
                             ("sid", `Int new_stmt.sid);
                             ("error", `Null)])
             | _ ->
               error_json (Printf.sprintf
                 "unknown op '%s', expected 'decl' or 'set'" op)
       in
       set_result rq result)

(* ====== execRemoveGhostStmt ====== *)

let () =
  let s = Server.Request.signature () in
  let get_function = Server.Request.param s
    ~name:"function" ~descr:(Markdown.plain "Function name")
    (module Server.Data.Jstring) in
  let get_sid = Server.Request.param s
    ~name:"sid" ~descr:(Markdown.plain "Statement ID to remove")
    (module Server.Data.Jint) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Operation result")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`EXEC
    ~name:"execRemoveGhostStmt"
    ~descr:(Markdown.plain
              "Remove a single ghost statement by its sid")
    (fun rq () ->
       let fname = get_function rq in
       let sid = get_sid rq in
       let result =
         match find_kf fname with
         | Error msg -> error_json msg
         | Ok kf ->
           match Ast_utils_ghost.remove_ghost_stmt kf sid with
           | Error msg ->
             ok_json [("success", `Bool false); ("error", `String msg)]
           | Ok () ->
             ok_json [("success", `Bool true); ("error", `Null)]
       in
       set_result rq result)

(* ====== execRemoveGhostVar ====== *)

let () =
  let s = Server.Request.signature () in
  let get_function = Server.Request.param s
    ~name:"function" ~descr:(Markdown.plain "Function name")
    (module Server.Data.Jstring) in
  let get_name = Server.Request.param s
    ~name:"name" ~descr:(Markdown.plain "Ghost variable name")
    (module Server.Data.Jstring) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Operation result")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`EXEC
    ~name:"execRemoveGhostVar"
    ~descr:(Markdown.plain
              "Remove a ghost variable and all its related statements")
    (fun rq () ->
       let fname = get_function rq in
       let var_name = get_name rq in
       let result =
         match find_kf fname with
         | Error msg -> error_json msg
         | Ok kf ->
           match Ast_utils_ghost.remove_ghost_var kf var_name with
           | Error msg ->
             ok_json [("success", `Bool false);
                      ("removed_count", `Int 0);
                      ("error", `String msg)]
           | Ok count ->
             ok_json [("success", `Bool true);
                      ("removed_count", `Int count);
                      ("error", `Null)]
       in
       set_result rq result)

(* ====== getVcDetails ====== *)

(** Pretty-print a WP predicate to string. *)
let pp_pred_to_string p =
  Format.asprintf "%a" Wp.Lang.F.pp_pred p

(** Extract hypothesis kind string and predicate string from a condition step. *)
let step_to_json (step : Wp.Conditions.step) : Json.t option =
  let make kind p =
    Some (`Assoc [
      ("kind", `String kind);
      ("formula", `String (pp_pred_to_string p))
    ])
  in
  match step.condition with
  | Wp.Conditions.Have p -> make "have" p
  | Wp.Conditions.When p -> make "when" p
  | Wp.Conditions.Type p -> make "type" p
  | Wp.Conditions.Init p -> make "init" p
  | Wp.Conditions.Core p -> make "core" p
  | Wp.Conditions.Branch (p, _, _) -> make "branch" p
  | Wp.Conditions.Either _ | Wp.Conditions.State _
  | Wp.Conditions.Probe _ -> None

let () =
  let s = Server.Request.signature () in
  let get_function = Server.Request.param s
    ~name:"function" ~descr:(Markdown.plain "Function name")
    (module Server.Data.Jstring) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "VC details")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`GET
    ~name:"getVcDetails"
    ~descr:(Markdown.plain
              "Get WP verification condition details (sequent) for a function")
    (fun rq () ->
       let fname = get_function rq in
       let result =
         match find_kf fname with
         | Error msg -> error_json msg
         | Ok kf ->
           try
             Wp.Wp_parameters.Model.set ["Bytes"];
             let goals = Wp.VC.generate_kf kf in
             let vcs = ref [] in
             let idx = ref 0 in
             Bag.iter (fun vc ->
               let desc = Wp.VC.get_description vc in
               let (hyps_raw, goal_raw) = Wp.VC.get_sequent vc in
               let steps = Wp.Conditions.list hyps_raw in
               let hyp_jsons = List.filter_map step_to_json steps in
               let goal_str = pp_pred_to_string goal_raw in
               let vc_json = `Assoc [
                 ("index", `Int !idx);
                 ("description", `String desc);
                 ("goal", `String goal_str);
                 ("hypotheses", `List hyp_jsons)
               ] in
               vcs := vc_json :: !vcs;
               incr idx
             ) goals;
             ok_json [
               ("function", `String fname);
               ("vc_count", `Int !idx);
               ("vcs", `List (List.rev !vcs))
             ]
           with exn ->
             error_json (Printf.sprintf "WP error: %s" (Printexc.to_string exn))
       in
       set_result rq result)

(* ====== execSetWpConfig ====== *)

let () =
  let s = Server.Request.signature () in
  let get_model = Server.Request.param_opt s
    ~name:"model" ~descr:(Markdown.plain
      "WP memory model: \"Bytes\" (default, safe) or \"Typed\" \
       (better for assigns/validity, but unsound with casts)")
    (module Server.Data.Jstring) in
  let get_prop = Server.Request.param_opt s
    ~name:"prop" ~descr:(Markdown.plain
      "Property filter (comma-separated). Use +name to include, \
       -name to exclude. Example: \"+my_inv,-assigns\"")
    (module Server.Data.Jstring) in
  let get_timeout = Server.Request.param_opt s
    ~name:"timeout" ~descr:(Markdown.plain
      "Per-goal prover timeout in seconds (default: 2)")
    (module Server.Data.Jint) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Configuration result")
    (module Server.Data.Jany) in
  Server.Request.register_sig s
    ~package
    ~kind:`EXEC
    ~name:"execSetWpConfig"
    ~descr:(Markdown.plain
              "Configure WP parameters (model, property filter, timeout) \
               for subsequent WP runs")
    (fun rq () ->
       let changed = ref [] in
       (match get_model rq with
        | Some m ->
          let model_list = String.split_on_char ',' m in
          Wp.Wp_parameters.Model.set model_list;
          changed := ("model", `String m) :: !changed
        | None -> ());
       (match get_prop rq with
        | Some p ->
          let prop_list = String.split_on_char ',' p in
          Wp.Wp_parameters.Properties.set prop_list;
          changed := ("prop", `String p) :: !changed
        | None -> ());
       (match get_timeout rq with
        | Some t ->
          Wp.Wp_parameters.Timeout.set t;
          changed := ("timeout", `Int t) :: !changed
        | None -> ());
       set_result rq
         (ok_json [("success", `Bool true);
                    ("changed", `List (List.map (fun (k, v) ->
                       `Assoc [("param", `String k); ("value", v)]
                     ) !changed))]))

(* ====== execCreateSandbox ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`EXEC
    ~name:"execCreateSandbox"
    ~descr:(Markdown.plain
              "Deep copy a function for CEGIS experimentation. \
               Returns sandbox name, experiment ID, and sid mapping.")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun name ->
       match find_kf name with
       | Error msg -> error_json msg
       | Ok kf ->
         match Ast_utils_sandbox.create_sandbox kf with
         | Error msg -> error_json msg
         | Ok (sandbox_name, hash, sid_map) ->
           ok_json [
             ("sandbox_name", `String sandbox_name);
             ("experiment_id", `String hash);
             ("sid_map", sid_map_to_json sid_map)
           ])

(* ====== execResetSandbox ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`EXEC
    ~name:"execResetSandbox"
    ~descr:(Markdown.plain
              "Delete and recreate sandbox from original, preserving experiment ID. \
               Requires the original function to exist.")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun sandbox_name ->
       match original_name_of_sandbox sandbox_name with
       | None ->
         error_json (Printf.sprintf
           "cannot extract original function name from '%s'" sandbox_name)
       | Some orig_name ->
         match find_kf orig_name with
         | Error msg -> error_json msg
         | Ok original_kf ->
           match Ast_utils_sandbox.reset_sandbox sandbox_name original_kf with
           | Error msg -> error_json msg
           | Ok (new_name, sid_map) ->
             ok_json [
               ("sandbox_name", `String new_name);
               ("sid_map", sid_map_to_json sid_map)
             ])

(* ====== execDeleteSandbox ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`EXEC
    ~name:"execDeleteSandbox"
    ~descr:(Markdown.plain
              "Remove sandbox function from AST. Idempotent.")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun sandbox_name ->
       match Ast_utils_sandbox.delete_sandbox sandbox_name with
       | Error msg -> error_json msg
       | Ok () ->
         ok_json [("success", `Bool true)])

(* ====== execExtractAnnotations ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`GET
    ~name:"execExtractAnnotations"
    ~descr:(Markdown.plain
              "Extract annotations added by ast-utils emitter from a sandbox function. \
               Returns list of (sid, annotation_text) pairs.")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun sandbox_name ->
       match find_kf sandbox_name with
       | Error msg -> error_json msg
       | Ok sandbox_kf ->
         match Ast_utils_sandbox.extract_our_annotations sandbox_kf with
         | Error msg -> error_json msg
         | Ok annots ->
           ok_json [
             ("annotations", `List (List.map (fun (sid, text) ->
               `Assoc [("sid", `Int sid); ("acsl", `String text)]
             ) annots))
           ])

(* ====== extractFunctionWithDeps ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`GET
    ~name:"extractFunctionWithDeps"
    ~descr:(Markdown.plain
              "Extract a function with all type/callee/global dependencies as a self-contained C string")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun name ->
       match find_kf name with
       | Error msg -> error_json msg
       | Ok kf ->
         match Ast_utils_extract.extract kf with
         | Ok (c_source, sids) ->
           ok_json [("success", `Bool true);
                    ("source", `String c_source);
                    ("sids", `List (List.map (fun s -> `Int s) sids))]
         | Error msg ->
           ok_json [("success", `Bool false);
                    ("error", `String msg)])

(* ====== getSallstmts ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`GET
    ~name:"getSallstmts"
    ~descr:(Markdown.plain
              "Return the sallstmts SID list for a function (authoritative statement ordering)")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun name ->
       match find_kf name with
       | Error msg -> error_json msg
       | Ok kf ->
         match Ast_utils_extract.get_sallstmts kf with
         | Ok sids ->
           `List (List.map (fun s -> `Int s) sids)
         | Error msg -> error_json msg)

(* ====== extractMultipleFunctions ====== *)

(* NOTE: extract_multiple has known bugs (Phase A/B ordering causing
   use-before-declare for GVar initializers that reference function pointers;
   typedef output missing semicolons in some paths). printSource has been
   switched to Printer.pp_file and no longer calls it. sandbox extraction
   (ast_utils_sandbox.ml) has its own extraction path and also does not call it.
   This endpoint currently has no known external caller.

   A fix analysis for the ordering bug exists on branch
   bugfix/sandbox-extract-parse-errors (commit 0e56e4b), but implementation
   is shelved — since no active caller exists, resurrect the fix only if
   this endpoint or extract_multiple is wired back into a real path. *)

let () =
  let s = Server.Request.signature () in
  let get_functions = Server.Request.param s
    ~name:"functions" ~descr:(Markdown.plain
      "List of function names to extract (JSON array of strings)")
    (module Server.Data.Jany) in
  let set_result = Server.Request.result s
    ~name:"result" ~descr:(Markdown.plain "Extraction result")
    (module Server.Data.Jany) in
  Server.Request.register_sig
    ~package
    ~kind:`GET
    ~name:"extractMultipleFunctions"
    ~descr:(Markdown.plain
      "Extract multiple functions as a single self-contained C file. \
       Callees in the target list get forward declarations; \
       external callees get contract stubs or empty bodies.")
    s
    (fun rq () ->
      let funcs_json = get_functions rq in
      let func_names = match funcs_json with
        | `List items ->
          List.filter_map (fun item ->
            match item with `String s -> Some s | _ -> None
          ) items
        | _ -> []
      in
      let kfs = List.filter_map (fun name ->
        match find_kf name with
        | Ok kf -> Some kf
        | Error _ -> None
      ) func_names in
      match Ast_utils_extract.extract_multiple kfs with
      | Ok source ->
        set_result rq (ok_json [("success", `Bool true);
                                ("source", `String source)])
      | Error msg ->
        set_result rq (ok_json [("success", `Bool false);
                                ("error", `String msg)]))

(* ====== printSource: output complete annotated C source ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`GET
    ~name:"printSource"
    ~descr:(Markdown.plain
              "Print complete annotated C source (all globals with ACSL + RTE assertions)")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jstring)
    (fun _ignored ->
       (* Use pp_file directly: extract_multiple was designed for extracting
          a small subset of functions, not the entire file. When all functions
          are passed as targets, extract_multiple outputs GVar globals (Phase A)
          before forward-declaring target functions (Phase B), causing
          use-before-declare errors for global variable initializers that
          reference function pointers (e.g. dispatch tables). pp_file outputs
          the full AST in original declaration order, including all ACSL
          annotations and RTE assertions already present in the CIL AST. *)
       let buf = Buffer.create 65536 in
       let fmt = Format.formatter_of_buffer buf in
       Printer.pp_file fmt (Ast.get ());
       Format.pp_print_flush fmt ();
       Buffer.contents buf)

(* ====== dumpProject: export F-CIL JSON ====== *)

let () =
  Server.Request.register
    ~package
    ~kind:`GET
    ~name:"dumpProject"
    ~descr:(Markdown.plain
      "Dump complete project AST as F-CIL JSON format. \
       Output includes all type definitions, global variables, \
       functions (with ACSL contracts), and ident_names mapping.")
    ~input:(module Server.Data.Jstring)
    ~output:(module Server.Data.Jany)
    (fun _ignored -> Ast_utils_export.dump_project ())

(* ====== Command-line export mode ====== *)

module ExportFile = Self.String(struct
  let option_name = "-ast-utils-export"
  let default = ""
  let arg_name = "file"
  let help = "Export F-CIL JSON to file (- for stdout)"
end)

let () = Boot.Main.extend (fun () ->
  let path = ExportFile.get () in
  if path <> "" then begin
    let json = Ast_utils_export.dump_project () in
    let oc = if path = "-" then stdout else open_out path in
    Yojson.Basic.pretty_to_channel oc json;
    if path <> "-" then close_out oc
  end)
