(** ast_utils_ghost.ml — Ghost statement insertion/removal via direct CIL construction.

    Provides:
    - Type name resolution (string → CIL typ)
    - Simple C expression parser (recursive descent)
    - Ghost variable declaration insertion
    - Ghost assignment insertion
    - Ghost statement/variable removal with registry tracking

    All inserted statements have stmt.ghost = true and target variables
    have vghost = true, ensuring soundness (no effect on program semantics). *)

open Frama_c_kernel
open Cil_types

(* ====== Ghost Registry ====== *)

(* key: (function_name, var_name), value: sids of statements we inserted *)
let ghost_registry : (string * string, int list) Hashtbl.t =
  Hashtbl.create 16

(* ====== Type Resolution ====== *)

let resolve_ghost_type name =
  match name with
  | "int" -> Ok Cil_const.intType
  | "unsigned int" | "unsigned" -> Ok Cil_const.uintType
  | "long" -> Ok Cil_const.longType
  | "unsigned long" -> Ok Cil_const.ulongType
  | "long long" -> Ok Cil_const.longLongType
  | "unsigned long long" -> Ok Cil_const.ulongLongType
  | "char" -> Ok Cil_const.charType
  | "short" -> Ok Cil_const.shortType
  | "unsigned short" -> Ok Cil_const.ushortType
  | "float" -> Ok Cil_const.floatType
  | "double" -> Ok Cil_const.doubleType
  | _ ->
    try Ok (Globals.Types.find_type Logic_typing.Typedef name)
    with Not_found ->
      Error (Printf.sprintf "unsupported type '%s'" name)

(* ====== Expression Parser ====== *)

type token =
  | TkInt of int
  | TkIdent of string
  | TkPlus | TkMinus | TkStar | TkSlash | TkPercent
  | TkLBracket | TkRBracket | TkLParen | TkRParen
  | TkEof

type lexer_state = {
  input : string;
  len : int;
  mutable pos : int;
}

let mk_lexer s = { input = s; len = String.length s; pos = 0 }

let skip_ws ls =
  while ls.pos < ls.len &&
        let c = ls.input.[ls.pos] in
        c = ' ' || c = '\t' || c = '\n' || c = '\r' do
    ls.pos <- ls.pos + 1
  done

let is_digit c = c >= '0' && c <= '9'
let is_alpha c = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c = '_'
let is_alnum c = is_digit c || is_alpha c

let next_token ls =
  skip_ws ls;
  if ls.pos >= ls.len then TkEof
  else
    let c = ls.input.[ls.pos] in
    match c with
    | '+' -> ls.pos <- ls.pos + 1; TkPlus
    | '-' -> ls.pos <- ls.pos + 1; TkMinus
    | '*' -> ls.pos <- ls.pos + 1; TkStar
    | '/' -> ls.pos <- ls.pos + 1; TkSlash
    | '%' -> ls.pos <- ls.pos + 1; TkPercent
    | '[' -> ls.pos <- ls.pos + 1; TkLBracket
    | ']' -> ls.pos <- ls.pos + 1; TkRBracket
    | '(' -> ls.pos <- ls.pos + 1; TkLParen
    | ')' -> ls.pos <- ls.pos + 1; TkRParen
    | _ when is_digit c ->
      let start = ls.pos in
      while ls.pos < ls.len && is_digit ls.input.[ls.pos] do
        ls.pos <- ls.pos + 1
      done;
      TkInt (int_of_string (String.sub ls.input start (ls.pos - start)))
    | _ when is_alpha c ->
      let start = ls.pos in
      while ls.pos < ls.len && is_alnum ls.input.[ls.pos] do
        ls.pos <- ls.pos + 1
      done;
      TkIdent (String.sub ls.input start (ls.pos - start))
    | _ ->
      failwith (Printf.sprintf
        "parse error: unexpected '%c' at position %d" c ls.pos)

let peek ls =
  let saved = ls.pos in
  let tok = next_token ls in
  ls.pos <- saved;
  tok

let find_var_in_fundec fundec name =
  try List.find (fun vi -> vi.vname = name) fundec.slocals
  with Not_found ->
  try List.find (fun vi -> vi.vname = name) fundec.sformals
  with Not_found ->
  try Globals.Vars.find_from_astinfo name Global
  with Not_found ->
    failwith (Printf.sprintf "unknown variable '%s'" name)

let rec parse_expr_impl ls fundec loc =
  parse_additive ls fundec loc

and parse_additive ls fundec loc =
  let left = ref (parse_multiplicative ls fundec loc) in
  let cont = ref true in
  while !cont do
    match peek ls with
    | TkPlus ->
      ignore (next_token ls);
      left := Cil.mkBinOp ~loc PlusA !left
                (parse_multiplicative ls fundec loc)
    | TkMinus ->
      ignore (next_token ls);
      left := Cil.mkBinOp ~loc MinusA !left
                (parse_multiplicative ls fundec loc)
    | _ -> cont := false
  done;
  !left

and parse_multiplicative ls fundec loc =
  let left = ref (parse_unary ls fundec loc) in
  let cont = ref true in
  while !cont do
    match peek ls with
    | TkStar ->
      ignore (next_token ls);
      left := Cil.mkBinOp ~loc Mult !left
                (parse_unary ls fundec loc)
    | TkSlash ->
      ignore (next_token ls);
      left := Cil.mkBinOp ~loc Div !left
                (parse_unary ls fundec loc)
    | TkPercent ->
      ignore (next_token ls);
      left := Cil.mkBinOp ~loc Mod !left
                (parse_unary ls fundec loc)
    | _ -> cont := false
  done;
  !left

and parse_unary ls fundec loc =
  match peek ls with
  | TkMinus ->
    ignore (next_token ls);
    let e = parse_unary ls fundec loc in
    Cil.new_exp ~loc (UnOp (Neg, e, Cil.typeOf e))
  | _ -> parse_primary ls fundec loc

and parse_primary ls fundec loc =
  match next_token ls with
  | TkInt n ->
    Cil.integer ~loc n
  | TkIdent name ->
    let vi = find_var_in_fundec fundec name in
    (match peek ls with
     | TkLBracket ->
       ignore (next_token ls);
       let idx = parse_expr_impl ls fundec loc in
       (match next_token ls with
        | TkRBracket -> ()
        | _ -> failwith (Printf.sprintf
                 "parse error: expected ']' at position %d" ls.pos));
       (match Ast_types.unroll_node vi.vtype with
        | TArray _ ->
          Cil.new_exp ~loc (Lval (Var vi, Index (idx, NoOffset)))
        | TPtr _ ->
          let addr = Cil.mkBinOp ~loc PlusPI (Cil.evar ~loc vi) idx in
          Cil.new_exp ~loc (Lval (Cil.mkMem ~addr ~off:NoOffset))
        | _ ->
          failwith (Printf.sprintf
            "variable '%s' is not an array or pointer" name))
     | _ ->
       Cil.evar ~loc vi)
  | TkLParen ->
    let e = parse_expr_impl ls fundec loc in
    (match next_token ls with
     | TkRParen -> ()
     | _ -> failwith (Printf.sprintf
              "parse error: expected ')' at position %d" ls.pos));
    e
  | TkEof ->
    failwith "parse error: unexpected end of input"
  | _ ->
    failwith (Printf.sprintf
      "parse error: unexpected token at position %d" ls.pos)

let parse_c_expr fundec loc expr_string =
  if String.length (String.trim expr_string) = 0 then
    Error "empty expression"
  else
    try
      let ls = mk_lexer expr_string in
      let e = parse_expr_impl ls fundec loc in
      (match peek ls with
       | TkEof -> Ok e
       | _ -> Error (Printf.sprintf
                "parse error: unexpected token at position %d" ls.pos))
    with Failure msg -> Error msg

(* ====== Internal Helpers ====== *)

let rebuild_cfg fundec =
  Cfg.clearCFGinfo ~clear_id:false fundec;
  Cfg.cfgFun fundec

(* Insert new_stmt before target_sid in the function body (recursive) *)
let rec insert_in_block target_sid new_stmt b =
  let found = ref false in
  let new_bstmts = List.fold_right (fun s acc ->
    if s.sid = target_sid then begin
      found := true;
      new_stmt :: s :: acc
    end else begin
      insert_in_skind target_sid new_stmt s.skind;
      s :: acc
    end
  ) b.bstmts [] in
  if !found then b.bstmts <- new_bstmts;
  !found

and insert_in_skind target_sid new_stmt = function
  | If (_, tb, fb, _) ->
    ignore (insert_in_block target_sid new_stmt tb);
    ignore (insert_in_block target_sid new_stmt fb)
  | Loop (_, b, _, _, _) | Block b ->
    ignore (insert_in_block target_sid new_stmt b)
  | Switch (_, b, _, _) ->
    ignore (insert_in_block target_sid new_stmt b)
  | _ -> ()

(* Remove statements by sid list from the function body (recursive) *)
let rec remove_from_block sids b =
  b.bstmts <- List.filter (fun s -> not (List.mem s.sid sids)) b.bstmts;
  List.iter (fun s -> remove_from_skind sids s.skind) b.bstmts

and remove_from_skind sids = function
  | If (_, tb, fb, _) ->
    remove_from_block sids tb;
    remove_from_block sids fb
  | Loop (_, b, _, _, _) | Block b ->
    remove_from_block sids b
  | Switch (_, b, _, _) ->
    remove_from_block sids b
  | _ -> ()

(* Check if a CIL expression references a varinfo (by vid) *)
let rec exp_references_var vid e =
  match e.enode with
  | Lval lv | AddrOf lv | StartOf lv -> lval_references_var vid lv
  | BinOp (_, e1, e2, _) ->
    exp_references_var vid e1 || exp_references_var vid e2
  | UnOp (_, e', _) | CastE (_, e') ->
    exp_references_var vid e'
  | SizeOfE e' | AlignOfE e' ->
    exp_references_var vid e'
  | Const _ | SizeOf _ | SizeOfStr _ | AlignOf _ -> false

and lval_references_var vid (lhost, off) =
  (match lhost with
   | Var vi -> vi.vid = vid
   | Mem e -> exp_references_var vid e)
  || offset_references_var vid off

and offset_references_var vid = function
  | NoOffset -> false
  | Field (_, off) -> offset_references_var vid off
  | Index (e, off) ->
    exp_references_var vid e || offset_references_var vid off

(* Check if a statement's expression references a varinfo *)
let stmt_references_var vid stmt =
  match stmt.skind with
  | Instr (Local_init (_, AssignInit (SingleInit e), _)) ->
    exp_references_var vid e
  | Instr (Set (_, e, _)) ->
    exp_references_var vid e
  | _ -> false

(* Check cross-references from other ghost entries *)
let check_cross_references fundec kf_name var_name vid =
  let errors = ref [] in
  Hashtbl.iter (fun (kn, vn) sids ->
    if kn = kf_name && vn <> var_name then
      List.iter (fun sid ->
        try
          let stmt = List.find (fun s -> s.sid = sid) fundec.sallstmts in
          if stmt_references_var vid stmt then
            errors :=
              Printf.sprintf
                "variable '%s' is referenced by ghost statement sid=%d"
                var_name sid
              :: !errors
        with Not_found -> ()
      ) sids
  ) ghost_registry;
  !errors

(* Check if a code annotation references a varinfo (text-based word search) *)
let annot_references_var vi ca =
  let text = Format.asprintf "%a" Printer.pp_code_annotation ca in
  let name = vi.vname in
  let nlen = String.length name in
  let tlen = String.length text in
  let rec search i =
    if i > tlen - nlen then false
    else if String.sub text i nlen = name then
      let before_ok = i = 0 || not (is_alnum text.[i - 1]) in
      let after_ok = i + nlen >= tlen || not (is_alnum text.[i + nlen]) in
      if before_ok && after_ok then true
      else search (i + 1)
    else search (i + 1)
  in
  search 0

(* Remove code annotations that reference a varinfo from all stmts *)
let remove_referencing_annots kf fundec vi =
  let count = ref 0 in
  List.iter (fun stmt ->
    let to_remove = ref [] in
    Annotations.iter_code_annot
      (fun _e ca ->
         if annot_references_var vi ca then
           to_remove := ca :: !to_remove)
      stmt;
    List.iter (fun ca ->
      incr count;
      Annotations.remove_code_annot Ast_utils_core.emitter ~kf stmt ca
    ) !to_remove
  ) fundec.sallstmts;
  !count

(* ====== Public API ====== *)

let insert_ghost_decl kf stmt var_name typ init_exp =
  try
    let fundec = Kernel_function.get_definition kf in
    let kf_name = Kernel_function.get_name kf in
    (* Check for duplicate variable name *)
    let exists_in_locals =
      List.exists (fun vi -> vi.vname = var_name) fundec.slocals in
    let exists_in_formals =
      List.exists (fun vi -> vi.vname = var_name) fundec.sformals in
    if exists_in_locals || exists_in_formals then
      Error (Printf.sprintf "variable '%s' already exists" var_name)
    else begin
      let loc = Cil_datatype.Stmt.loc stmt in
      (* Type compatibility check + implicit cast *)
      let cast_exp =
        let exp_typ = Cil.typeOf init_exp in
        if Cil_datatype.Typ.equal exp_typ typ then init_exp
        else Cil.mkCast ~newt:typ init_exp
      in
      (* Create ghost local variable (insert:false = don't auto-add) *)
      let vi = Cil.makeLocalVar ~insert:false ~ghost:true ~loc
                 fundec var_name typ in
      (* Manually add to slocals *)
      fundec.slocals <- fundec.slocals @ [vi];
      (* Build Local_init instruction *)
      let init_instr =
        Local_init (vi, AssignInit (SingleInit cast_exp), loc) in
      let new_stmt =
        Cil.mkStmtOneInstr ~ghost:true ~valid_sid:true init_instr in
      (* Insert before target stmt *)
      let inserted = insert_in_block stmt.sid new_stmt fundec.sbody in
      if not inserted then
        Error (Printf.sprintf "statement %d not found in function body"
                 stmt.sid)
      else begin
        fundec.sallstmts <- new_stmt :: fundec.sallstmts;
        rebuild_cfg fundec;
        (* Update registry *)
        Hashtbl.replace ghost_registry
          (kf_name, var_name) [new_stmt.sid];
        Ok new_stmt
      end
    end
  with
  | Kernel_function.No_Definition ->
    Error (Printf.sprintf "function '%s' has no definition"
             (Kernel_function.get_name kf))
  | Failure msg -> Error msg
  | exn -> Error (Printexc.to_string exn)

let insert_ghost_assign kf stmt target_name expr =
  try
    let fundec = Kernel_function.get_definition kf in
    let kf_name = Kernel_function.get_name kf in
    (* Find the target ghost variable *)
    let vi =
      try List.find (fun vi -> vi.vname = target_name) fundec.slocals
      with Not_found ->
        failwith (Printf.sprintf "variable '%s' not found" target_name)
    in
    if not vi.vghost then
      failwith (Printf.sprintf
        "variable '%s' is not a ghost variable" target_name);
    let loc = Cil_datatype.Stmt.loc stmt in
    (* Type compatibility check + implicit cast *)
    let cast_exp =
      let exp_typ = Cil.typeOf expr in
      if Cil_datatype.Typ.equal exp_typ vi.vtype then expr
      else Cil.mkCast ~newt:vi.vtype expr
    in
    (* Build Set instruction *)
    let set_instr = Set ((Var vi, NoOffset), cast_exp, loc) in
    let new_stmt =
      Cil.mkStmtOneInstr ~ghost:true ~valid_sid:true set_instr in
    (* Insert before target stmt *)
    let inserted = insert_in_block stmt.sid new_stmt fundec.sbody in
    if not inserted then
      Error (Printf.sprintf "statement %d not found in function body"
               stmt.sid)
    else begin
      fundec.sallstmts <- new_stmt :: fundec.sallstmts;
      rebuild_cfg fundec;
      (* Update registry *)
      let key = (kf_name, target_name) in
      let existing =
        try Hashtbl.find ghost_registry key
        with Not_found -> [] in
      Hashtbl.replace ghost_registry key (new_stmt.sid :: existing);
      Ok new_stmt
    end
  with
  | Kernel_function.No_Definition ->
    Error (Printf.sprintf "function '%s' has no definition"
             (Kernel_function.get_name kf))
  | Failure msg -> Error msg
  | exn -> Error (Printexc.to_string exn)

let remove_ghost_stmt kf sid =
  try
    let fundec = Kernel_function.get_definition kf in
    let kf_name = Kernel_function.get_name kf in
    (* Find which registry entry contains this sid *)
    let entry = ref None in
    Hashtbl.iter (fun (kn, vn) sids ->
      if kn = kf_name && List.mem sid sids then
        entry := Some (vn, sids)
    ) ghost_registry;
    match !entry with
    | None ->
      Error (Printf.sprintf
        "statement %d is not a tracked ghost statement" sid)
    | Some (var_name, sids) ->
      (* Find the statement *)
      let stmt =
        try List.find (fun s -> s.sid = sid) fundec.sallstmts
        with Not_found ->
          failwith (Printf.sprintf "statement %d not found" sid)
      in
      (* Check if it's a declaration (Local_init) *)
      let is_decl = match stmt.skind with
        | Instr (Local_init (vi, _, _)) -> vi.vname = var_name
        | _ -> false
      in
      if is_decl then begin
        (* Cascade: remove all related statements + annotations *)
        let vi = match stmt.skind with
          | Instr (Local_init (vi, _, _)) -> vi
          | _ -> assert false
        in
        (* Cross-reference check *)
        let errors =
          check_cross_references fundec kf_name var_name vi.vid in
        match errors with
        | err :: _ -> Error err
        | [] ->
          (* Remove annotations referencing this variable *)
          let _annot_count =
            remove_referencing_annots kf fundec vi in
          (* Remove all statements for this variable *)
          remove_from_block sids fundec.sbody;
          fundec.sallstmts <-
            List.filter (fun s -> not (List.mem s.sid sids))
              fundec.sallstmts;
          (* Remove from slocals *)
          fundec.slocals <-
            List.filter (fun v -> v.vid <> vi.vid) fundec.slocals;
          rebuild_cfg fundec;
          (* Clean registry *)
          Hashtbl.remove ghost_registry (kf_name, var_name);
          Ok ()
      end else begin
        (* Just remove this single assignment statement *)
        remove_from_block [sid] fundec.sbody;
        fundec.sallstmts <-
          List.filter (fun s -> s.sid <> sid) fundec.sallstmts;
        rebuild_cfg fundec;
        (* Update registry *)
        let key = (kf_name, var_name) in
        let new_sids =
          List.filter (fun s -> s <> sid) sids in
        if new_sids = [] then
          Hashtbl.remove ghost_registry key
        else
          Hashtbl.replace ghost_registry key new_sids;
        Ok ()
      end
  with
  | Kernel_function.No_Definition ->
    Error (Printf.sprintf "function '%s' has no definition"
             (Kernel_function.get_name kf))
  | Failure msg -> Error msg
  | exn -> Error (Printexc.to_string exn)

let remove_ghost_var kf var_name =
  try
    let fundec = Kernel_function.get_definition kf in
    let kf_name = Kernel_function.get_name kf in
    let key = (kf_name, var_name) in
    let sids =
      try Hashtbl.find ghost_registry key
      with Not_found ->
        failwith (Printf.sprintf
          "ghost variable '%s' not tracked" var_name)
    in
    (* Determine if API-created (has Local_init in sid list) *)
    let decl_vi = ref None in
    List.iter (fun sid ->
      try
        let stmt = List.find (fun s -> s.sid = sid) fundec.sallstmts in
        match stmt.skind with
        | Instr (Local_init (vi, _, _)) when vi.vname = var_name ->
          decl_vi := Some vi
        | _ -> ()
      with Not_found -> ()
    ) sids;
    let count = ref 0 in
    begin match !decl_vi with
    | Some vi ->
      (* API-created variable: full cascade *)
      let errors =
        check_cross_references fundec kf_name var_name vi.vid in
      (match errors with
       | err :: _ -> failwith err
       | [] ->
         (* Remove referencing annotations *)
         let annot_count =
           remove_referencing_annots kf fundec vi in
         count := !count + annot_count;
         (* Remove all statements *)
         remove_from_block sids fundec.sbody;
         fundec.sallstmts <-
           List.filter (fun s -> not (List.mem s.sid sids))
             fundec.sallstmts;
         count := !count + List.length sids;
         (* Remove from slocals *)
         fundec.slocals <-
           List.filter (fun v -> v.vid <> vi.vid) fundec.slocals;
         rebuild_cfg fundec)
    | None ->
      (* Source-level ghost: only remove our assignment statements *)
      remove_from_block sids fundec.sbody;
      fundec.sallstmts <-
        List.filter (fun s -> not (List.mem s.sid sids))
          fundec.sallstmts;
      count := !count + List.length sids;
      rebuild_cfg fundec
    end;
    Hashtbl.remove ghost_registry key;
    Ok !count
  with
  | Kernel_function.No_Definition ->
    Error (Printf.sprintf "function '%s' has no definition"
             (Kernel_function.get_name kf))
  | Failure msg -> Error msg
  | exn -> Error (Printexc.to_string exn)
