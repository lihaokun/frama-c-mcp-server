(* JSON processing using Yojson *)
open Yojson.Safe

(* Process a JSON request: extract "numbers" array, compute sum/product/count *)
let process_json json_str =
  try
    let json = from_string json_str in
    match json with
    | `Assoc fields ->
      let command = match List.assoc_opt "command" fields with
        | Some (`String s) -> s
        | _ -> "unknown"
      in
      begin match command with
      | "stats" ->
        let numbers = match List.assoc_opt "data" fields with
          | Some (`List nums) ->
            List.filter_map (function `Int n -> Some n | `Float f -> Some (int_of_float f) | _ -> None) nums
          | _ -> []
        in
        let count = List.length numbers in
        let sum = List.fold_left (+) 0 numbers in
        let avg = if count > 0 then float_of_int sum /. float_of_int count else 0.0 in
        let max_val = match numbers with [] -> 0 | _ -> List.fold_left max min_int numbers in
        let min_val = match numbers with [] -> 0 | _ -> List.fold_left min max_int numbers in
        let result = `Assoc [
          "status", `String "ok";
          "command", `String "stats";
          "count", `Int count;
          "sum", `Int sum;
          "average", `Float avg;
          "max", `Int max_val;
          "min", `Int min_val;
        ] in
        to_string result
      | "echo" ->
        let payload = match List.assoc_opt "data" fields with
          | Some v -> v
          | None -> `Null
        in
        let result = `Assoc [
          "status", `String "ok";
          "command", `String "echo";
          "echoed", payload;
        ] in
        to_string result
      | "transform" ->
        (* Reverse strings in a list *)
        let strings = match List.assoc_opt "data" fields with
          | Some (`List ss) ->
            List.filter_map (function `String s -> Some s | _ -> None) ss
          | _ -> []
        in
        let reversed = List.map (fun s ->
          let len = String.length s in
          String.init len (fun i -> s.[len - 1 - i])
        ) strings in
        let result = `Assoc [
          "status", `String "ok";
          "command", `String "transform";
          "result", `List (List.map (fun s -> `String s) reversed);
        ] in
        to_string result
      | cmd ->
        let result = `Assoc [
          "status", `String "error";
          "message", `String ("Unknown command: " ^ cmd);
        ] in
        to_string result
      end
    | _ ->
      to_string (`Assoc ["status", `String "error"; "message", `String "Expected JSON object"])
  with e ->
    to_string (`Assoc ["status", `String "error"; "message", `String (Printexc.to_string e)])

let () =
  Callback.register "process_json" process_json
