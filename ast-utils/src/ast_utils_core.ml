(** ast_utils_core.ml — ACSL string-to-typed-AST conversion + CIL AST insert/remove.

    Independent public sub-library providing:
    - ACSL string → typed AST (via Logic_lexer + Logic_typing)
    - Typed AST → CIL AST insertion (via Annotations)
    - CIL AST annotation removal (by emitter tracking)

    Uses the same three-layer pipeline as Frama-C's [logic_parse_string.ml]:
    string → Logic_lexer → Logic_ptree → Logic_typing.Make(callbacks) → Cil_types *)

open Frama_c_kernel
open Cil_types

(* ====== Emitter ====== *)

let emitter = Emitter.create "ast-utils"
  [ Emitter.Funspec; Emitter.Code_annot ]
  ~correctness:[] ~tuning:[]

(* ====== Internal: typing context ====== *)

(** Exception for typing errors (mirrors Logic_parse_string.Error). *)
exception Typing_error of Cil_types.location * string

(** Resolve a C variable by name. Scope depends on [kinstr]:
    - [Kglobal] (funspec context): formals only — ACSL §2.3 restricts
      function-level contracts to caller-visible state. Locals fall through
      to the fallback chain (formals/globals); a local name raises
      [Not_found], which surfaces as an "Unbound variable" typer error.
      Logic_parse_string.ml uses [Whole_function] because it parses inline
      annotations in source files where context disambiguates scope; we
      insert funspecs at runtime and must enforce the ACSL boundary
      explicitly.
    - [Kstmt _] (annotation context): block scope, allowing locals —
      ACSL permits asserts / loop invariants / etc. to reference locals.

    Fallback for both: formals → globals, preserving legitimate uses such
    as [\old(formal)] and globals in funspec. *)
let find_var kf kinstr ?label var =
  let vi =
    try
      let scope =
        match kinstr with
        | Kglobal -> Formal kf
        | Kstmt stmt ->
          (match label with
           | None | Some "Here" | Some "Post" | Some "Old" ->
             Block_scope stmt
           | Some "Pre" -> raise Not_found
           | Some "Init" -> raise Not_found
           | Some "LoopEntry" | Some "LoopCurrent" ->
             if not (Kernel_function.stmt_in_loop kf stmt) then
               Kernel.fatal
                 "Use of LoopEntry or LoopCurrent outside of a loop";
             Block_scope
               (Kernel_function.find_enclosing_loop kf stmt)
           | Some l ->
             (try let s = Kernel_function.find_label kf l in
               Block_scope !s
              with Not_found ->
                Kernel.fatal
                  "Use of label %s that does not exist in function %a"
                  l Kernel_function.pretty kf))
      in
      Globals.Vars.find_from_astinfo var scope
    with Not_found ->
    try
      Globals.Vars.find_from_astinfo var (Formal kf)
    with Not_found ->
      Globals.Vars.find_from_astinfo var Global
  in
  Cil.cvar_to_lvar vi

(** Construct a Logic_typing module instance for the given function
    and instruction context. Replicates the default_typer pattern
    from Frama-C's logic_parse_string.ml. *)
let make_typer kf kinstr =
  let module LT = Logic_typing.Make
      (struct
        let anonCompFieldName = Cabs2cil.anonCompFieldName
        let conditionalConversion = Cabs2cil.logicConditionalConversion

        let is_loop () =
          match kinstr with
          | Kglobal -> false
          | Kstmt s -> Kernel_function.stmt_in_loop kf s

        let find_macro _ = raise Not_found

        let find_var ?label var = find_var kf kinstr ?label var

        let find_enum_tag x =
          try
            Globals.Types.find_enum_tag x
          with Not_found ->
            (* This is the final fallback for Logic_typing's identifier
               resolution chain (find_var → find_enum_tag). When in funspec
               context (Kglobal), check whether [x] is a function local —
               our [find_var] only exposes formals in Kglobal scope, so a
               local would silently fall through to here. In that case give
               an actionable error referencing the ACSL §2.3 scope rule.
               Returns a generic "Unbound variable" otherwise. *)
            let msg =
              match kinstr with
              | Kglobal ->
                (try
                   let _ = Globals.Vars.find_from_astinfo x
                             (Whole_function kf) in
                   Format.asprintf
                     "Variable '%s' is a function local; ACSL function-level \
                      contracts may only reference caller-visible state \
                      (formals, globals, \\result, \\old(formal)). Replace \
                      with the caller-visible state being modified." x
                 with Not_found -> "Unbound variable " ^ x)
              | Kstmt _ -> "Unbound variable " ^ x
            in
            raise (Typing_error (Cil_datatype.Location.unknown, msg))

        let find_comp_field info s =
          let field = Cil.getCompField info s in
          Field (field, NoOffset)

        let find_type = Globals.Types.find_type

        let find_label s = Kernel_function.find_label kf s

        let integral_cast ty t =
          raise
            (Failure
               (Format.asprintf
                  "term %a has type %a, but %a is expected."
                  Printer.pp_term t
                  Printer.pp_logic_type Linteger
                  Printer.pp_typ ty))

        let error loc msg =
          Pretty_utils.ksfprintf
            (fun e -> raise (Typing_error (loc, e))) msg

        let on_error f rollback x =
          try f x
          with Typing_error (loc, msg) as exn ->
            rollback (loc, msg); raise exn

      end)
  in
  (module LT : Logic_typing.S)

(** Synchronize type definitions with the logic environment.
    Must be called before parsing ACSL strings.
    Mirrors logic_parse_string.ml sync_typedefs. *)
let sync_typedefs () =
  Logic_env.reset_typenames ();
  Globals.Types.iter_types
    (fun name _ ns ->
       if ns = Logic_typing.Typedef then
         try ignore @@ String.index name ':' with Not_found ->
           Logic_env.add_typename name)

(* ====== annot-error suppression ====== *)

(** Run [f ()] with Kernel.wkey_annot_error temporarily downgraded from
    Wabort to Wfeedback so that ACSL parse/typing errors are reported as
    warnings rather than raising AbortError. Restores the original level
    after [f] completes (even if an exception is raised). *)
let with_annot_error_suppressed f =
  let key = Kernel.wkey_annot_error in
  let saved = Kernel.get_warn_status key in
  Kernel.set_warn_status key Log.Wfeedback;
  Fun.protect ~finally:(fun () -> Kernel.set_warn_status key saved) f

(* ====== Label injection into typed AST ====== *)

(** Prepend [label] to a predicate's [pred_name] list. *)
let label_pred label p =
  { p with pred_name = label :: p.pred_name }

(** Inject label into a [toplevel_predicate]. *)
let label_tp label tp =
  { tp with tp_statement = label_pred label tp.tp_statement }

(** Inject label into an [identified_predicate]. *)
let label_ip label ip =
  { ip with ip_content = label_tp label ip.ip_content }

(** Inject label into all predicates of a [funspec] (in-place via mutable fields).
    For function-level specs without an explicit behavior name, set the behavior
    name to include the label so that [remove_annotation_by_label] can find and
    remove it. For user-provided behavior blocks (e.g. "behavior sorted: ..."),
    preserve the original name — the label is still injected into predicates. *)
let inject_label_spec label spec =
  List.iter (fun b ->
    (* Only rename the default (unnamed) behavior; preserve user-provided names *)
    if b.b_name = "" then
      b.b_name <- label ^ "__spec";
    b.b_requires <- List.map (label_ip label) b.b_requires;
    b.b_post_cond <-
      List.map (fun (tk, ip) -> (tk, label_ip label ip)) b.b_post_cond
  ) spec.spec_behavior

(** Inject label into a [code_annotation]'s predicate (if any). *)
let inject_label_ca label ca =
  let new_content = match ca.annot_content with
    | AAssert (bhs, tp) ->
      AAssert (bhs, label_tp label tp)
    | AInvariant (bhs, is_loop, tp) ->
      AInvariant (bhs, is_loop, label_tp label tp)
    | other -> other
  in
  { ca with annot_content = new_content }

(* ====== Public API: typing ====== *)

let type_spec ?label kf spec_string =
  with_annot_error_suppressed @@ fun () ->
  try
    sync_typedefs ();
    let module LT =
      (val make_typer kf Kglobal : Logic_typing.S) in
    (* Get source position for error messages *)
    let vi = Kernel_function.get_vi kf in
    let pos = fst (vi.vdecl) in
    (* Parse ACSL string → Logic_ptree.spec *)
    match Logic_lexer.spec (pos, spec_string) with
    | None ->
      Error "ACSL syntax error in function contract"
    | Some (_, parsed_spec) ->
      (* Type-check Logic_ptree.spec → Cil_types.funspec *)
      let formals = try Some (Kernel_function.get_formals kf)
        with Kernel_function.No_Definition -> None in
      let typ = Kernel_function.get_type kf in
      let old_spec = Annotations.funspec kf in
      let behaviors =
        Logic_utils.get_behavior_names old_spec in
      let funspec =
        LT.funspec behaviors vi formals typ parsed_spec in
      (* Inject label into all predicates if provided *)
      Option.iter (fun l -> inject_label_spec l funspec) label;
      Ok funspec
  with
  | Typing_error (_, msg) -> Error msg
  | Failure msg -> Error msg
  | exn -> Error (Printexc.to_string exn)

let type_annot ?label kf stmt annot_string =
  with_annot_error_suppressed @@ fun () ->
  try
    sync_typedefs ();
    let module LT =
      (val make_typer kf (Kstmt stmt) : Logic_typing.S) in
    let loc = Cil_datatype.Stmt.loc stmt in
    let pos = fst loc in
    (* Parse ACSL string → Logic_ptree.annot list *)
    let parsed_list =
      match Logic_lexer.annot (pos, annot_string) with
      | None -> []
      | Some (_pos, annot) ->
        match annot with
        | Logic_ptree.Acode_annot (_, ca) -> [ca]
        | Logic_ptree.Aloop_annot (_, cas) -> cas
        | _ -> []
    in
    match parsed_list with
    | [] ->
      Error "ACSL syntax error in code annotation"
    | _ ->
      Populate_spec.populate_funspec kf [`Assigns];
      let spec = Annotations.funspec kf in
      let behaviors =
        Logic_utils.get_behavior_names spec in
      let ret_type =
        Ctype (Kernel_function.get_return_type kf) in
      let typed_list = List.map
        (fun parsed_ca -> LT.code_annot loc behaviors ret_type parsed_ca)
        parsed_list in
      (* Inject label into all predicates if provided *)
      let typed_list = match label with
        | Some l -> List.map (inject_label_ca l) typed_list
        | None -> typed_list
      in
      Ok typed_list
  with
  | Typing_error (_, msg) -> Error msg
  | Failure msg -> Error msg
  | exn -> Error (Printexc.to_string exn)

(* ====== Public API: insertion ====== *)

let insert_spec kf funspec =
  (* 用 canonical add_spec 一次插入整个 funspec：behaviors + complete + disjoint
     + terminates + decreases。原先手搓的三段 List.iter 漏了 spec_terminates /
     spec_variant（funspec 顶层单值字段，不在 behavior 内）→ terminates \false 被
     静默丢弃（fsmint-6 bug，见 docs/fixes/fsmint6-terminates-injection-fix-second-block.md）。
     ~force:true：terminates/decreases 覆盖 kernel populate 的默认 \true，否则 force=false
     遇已有 clause 抛 AlreadySpecified。behaviors 仍是追加，不冲掉已有 requires/ensures。 *)
  Annotations.add_spec ~force:true emitter kf funspec

let is_loop_annot annot =
  match annot.annot_content with
  | AInvariant (_, true, _) | AVariant _ | AAssigns _ | AAllocation _ -> true
  | AExtended (_, true, _) -> true
  | _ -> false

let insert_annot _kf stmt annot =
  if is_loop_annot annot then
    match stmt.skind with
    | Loop _ -> Annotations.add_code_annot ~keep_empty:false emitter ~kf:_kf stmt annot
    | _ -> raise (Invalid_argument
                    "loop annotation (invariant/variant/assigns) can only be \
                     attached to a Loop statement")
  else
    Annotations.add_code_annot emitter ~kf:_kf stmt annot

let insert_annots kf stmt annots =
  List.iter (fun annot -> insert_annot kf stmt annot) annots

(* ====== Public API: removal ====== *)

let remove kf =
  let count = ref 0 in
  (* Remove behaviors added by our emitter *)
  let behaviors_to_remove = ref [] in
  Annotations.iter_behaviors_by_emitter
    (fun e bhv ->
       if Emitter.equal e emitter then
         behaviors_to_remove := bhv :: !behaviors_to_remove)
    kf;
  List.iter (fun bhv ->
    incr count;
    Annotations.remove_behavior ~force:true emitter kf bhv
  ) !behaviors_to_remove;
  (* Remove code annotations added by our emitter *)
  (try
    let fundec = Kernel_function.get_definition kf in
    List.iter (fun stmt ->
      let annots_to_remove = ref [] in
      Annotations.iter_code_annot
        (fun e ca ->
           if Emitter.equal e emitter then
             annots_to_remove := ca :: !annots_to_remove)
        stmt;
      List.iter (fun ca ->
        incr count;
        Annotations.remove_code_annot emitter ~kf stmt ca
      ) !annots_to_remove
    ) fundec.sallstmts
  with Kernel_function.No_Definition -> ());
  !count

(** Remove a single annotation identified by its hash_label.
    Handles both function-level specs (requires/ensures/assigns stored in
    behavior predicates) and code annotations (loop invariants/assigns/etc.). *)
let remove_annotation_by_label kf label =
  let removed = ref false in
  (* Remove function-level spec behaviors whose name starts with the label.
     Must use iter_behaviors_by_emitter + remove_behavior — calling
     Annotations.funspec returns a copy, so mutating its fields is a no-op. *)
  let behaviors_to_remove = ref [] in
  Annotations.iter_behaviors_by_emitter
    (fun e bhv ->
       if Emitter.equal e emitter
          && bhv.b_name = label ^ "__spec"
       then behaviors_to_remove := bhv :: !behaviors_to_remove)
    kf;
  List.iter (fun bhv ->
    Annotations.remove_behavior ~force:true emitter kf bhv;
    removed := true
  ) !behaviors_to_remove;
  (* Remove code annotations (loop-level) whose predicates contain the label *)
  (try
    let fundec = Kernel_function.get_definition kf in
    List.iter (fun stmt ->
      Annotations.iter_code_annot (fun e ca ->
        if Emitter.equal e emitter && not !removed then
          match ca.annot_content with
          | AAssert (_, tp) | AInvariant (_, _, tp) ->
            if List.mem label tp.tp_statement.pred_name then (
              Annotations.remove_code_annot emitter ~kf stmt ca;
              removed := true
            )
          | _ -> ()
      ) stmt
    ) fundec.sallstmts
  with Kernel_function.No_Definition -> ());
  !removed
