(** ast_utils_ast.ml — CIL AST → JSON serialization.

    Converts Frama-C CIL internal representation to JSON objects
    suitable for consumption by LLM-based verification agents. *)

open Frama_c_kernel
open Cil_types

(* ====== Utilities ====== *)

let pp_to_string pp x =
  Format.asprintf "%a" pp x

(* ====== Variable info ====== *)

let varinfo_to_json (vi : varinfo) : Yojson.Basic.t =
  `Assoc [
    ("name", `String vi.vname);
    ("type", `String (pp_to_string Printer.pp_typ vi.vtype));
    ("vid", `Int vi.vid);
  ]

(* ====== Labels ====== *)

let label_to_json (l : label) : Yojson.Basic.t =
  match l with
  | Label (name, _, _) -> `Assoc [("label", `String name)]
  | Case (exp, _) -> `Assoc [("case", `String (pp_to_string Printer.pp_exp exp))]
  | Default _ -> `Assoc [("default", `Bool true)]

let labels_to_json (labels : label list) : (string * Yojson.Basic.t) list =
  match labels with
  | [] -> []
  | _ -> [("labels", `List (List.map label_to_json labels))]

(* ====== Annotations ====== *)

let annotations_to_json (s : stmt) : (string * Yojson.Basic.t) list =
  let annots = Annotations.code_annot s in
  match annots with
  | [] -> []
  | _ ->
    let texts = List.map (fun ca ->
      `String (pp_to_string Printer.pp_code_annotation ca)
    ) annots in
    [("annotations", `List texts)]

(* ====== Instructions ====== *)

(** Extract callee function name from a call expression.
    For direct calls like [f(args)], returns the function name.
    For indirect calls through function pointers, falls back to pretty-printing. *)
let extract_callee_name (fn : exp) : string =
  match fn.enode with
  | Lval (Var vi, NoOffset) -> vi.vname
  | _ -> pp_to_string Printer.pp_exp fn

let instr_to_json (i : instr) : Yojson.Basic.t option =
  match i with
  | Set _ ->
    Some (`Assoc [
      ("kind", `String "set");
      ("code", `String (pp_to_string Printer.pp_instr i));
    ])
  | Call (_, fn, args, _) ->
    let actuals = List.map (fun a ->
      `String (pp_to_string Printer.pp_exp a)
    ) args in
    Some (`Assoc [
      ("kind", `String "call");
      ("code", `String (pp_to_string Printer.pp_instr i));
      ("callee", `String (extract_callee_name fn));
      ("actuals", `List actuals);
    ])
  | Local_init (vi, AssignInit (SingleInit e), _) ->
    let code = Format.asprintf "%a %s = %a;"
      Printer.pp_typ vi.vtype vi.vname Printer.pp_exp e in
    Some (`Assoc [
      ("kind", `String "set");
      ("code", `String code);
    ])
  | Local_init (_, ConsInit (f, args, _), _) ->
    let actuals = List.map (fun a ->
      `String (pp_to_string Printer.pp_exp a)
    ) args in
    Some (`Assoc [
      ("kind", `String "call");
      ("code", `String (pp_to_string Printer.pp_instr i));
      ("callee", `String f.vname);
      ("actuals", `List actuals);
    ])
  | Local_init (vi, AssignInit (CompoundInit _), _) ->
    Some (`Assoc [
      ("kind", `String "set");
      ("code", `String (pp_to_string Printer.pp_instr i));
    ])
  | Asm _ ->
    Some (`Assoc [
      ("kind", `String "asm");
      ("code", `String (pp_to_string Printer.pp_instr i));
    ])
  | Skip _ -> None
  | Code_annot (ca, _) ->
    Some (`Assoc [
      ("kind", `String "annotation");
      ("text", `String (pp_to_string Printer.pp_code_annotation ca));
    ])

(* ====== Statements ====== *)

let rec stmt_to_json (s : stmt) : Yojson.Basic.t =
  let base =
    [("sid", `Int s.sid);
     ("pred_sids", `List (List.map (fun p -> `Int p.sid) s.preds));
     ("succ_sids", `List (List.map (fun p -> `Int p.sid) s.succs))]
    @ labels_to_json s.labels
    @ annotations_to_json s
  in
  let kind_fields =
    match s.skind with
    | Instr i ->
      (match instr_to_json i with
       | Some j -> [("kind", `String "instr"); ("instr", j)]
       | None -> [("kind", `String "skip")])
    | Return (e_opt, _) ->
      let expr = match e_opt with
        | Some e -> `String (pp_to_string Printer.pp_exp e)
        | None -> `Null
      in
      [("kind", `String "return"); ("expr", expr)]
    | If (e, tb, fb, _) ->
      [("kind", `String "if");
       ("cond", `String (pp_to_string Printer.pp_exp e));
       ("then_body", `List (block_to_json tb));
       ("else_body", `List (block_to_json fb))]
    | Loop (_, b, _, _, _) ->
      [("kind", `String "loop");
       ("body", `List (block_to_json b))]
    | Block b ->
      [("kind", `String "block");
       ("stmts", `List (block_to_json b))]
    | Switch (e, _, cases, _) ->
      let case_labels = List.filter_map (fun cs ->
        let case_vals = List.filter_map (fun l ->
          match l with
          | Case (exp, _) -> Some (pp_to_string Printer.pp_exp exp)
          | _ -> None
        ) cs.labels in
        let is_default = List.exists (fun l ->
          match l with Default _ -> true | _ -> false
        ) cs.labels in
        if case_vals <> [] || is_default then
          let fields = [("sid", `Int cs.sid)] in
          let fields = if case_vals <> [] then
            fields @ [("values", `List (List.map (fun v -> `String v) case_vals))]
          else fields in
          let fields = if is_default then
            fields @ [("default", `Bool true)]
          else fields in
          Some (`Assoc fields)
        else None
      ) cases in
      [("kind", `String "switch");
       ("expr", `String (pp_to_string Printer.pp_exp e));
       ("cases", `List case_labels)]
    | Goto (s_ref, _) ->
      [("kind", `String "goto");
       ("target_sid", `Int (!s_ref).sid)]
    | Break _ ->
      [("kind", `String "break")]
    | Continue _ ->
      [("kind", `String "continue")]
    | UnspecifiedSequence seq ->
      let stmts = List.map (fun (s, _, _, _, _) -> stmt_to_json s) seq in
      [("kind", `String "block");
       ("stmts", `List stmts)]
    | Throw _ | TryCatch _ | TryFinally _ | TryExcept _ ->
      [("kind", `String "unsupported");
       ("text", `String (pp_to_string Printer.pp_stmt s))]
  in
  `Assoc (base @ kind_fields)

and block_to_json (b : block) : Yojson.Basic.t list =
  List.map stmt_to_json b.bstmts

(* ====== Entry point ====== *)

let get_function_ast (kf : kernel_function) : Yojson.Basic.t =
  let fundec = Kernel_function.get_definition kf in
  let name = Kernel_function.get_name kf in
  let signature = pp_to_string Printer.pp_vdecl (Kernel_function.get_vi kf) in
  let formals = List.map varinfo_to_json (Kernel_function.get_formals kf) in
  let locals = List.map varinfo_to_json fundec.slocals in
  let ret_type = pp_to_string Printer.pp_typ (Kernel_function.get_return_type kf) in
  let body = block_to_json fundec.sbody in
  `Assoc [
    ("name", `String name);
    ("signature", `String signature);
    ("formals", `List formals);
    ("locals", `List locals);
    ("return_type", `String ret_type);
    ("body", `List body);
  ]
