let fib n =
  let rec aux a b i =
    if i >= n then a
    else aux b (a + b) (i + 1)
  in
  aux 0 1 0

let greet name =
  "Hello from OCaml, " ^ name ^ "!"

let () =
  Callback.register "fib" fib;
  Callback.register "greet" greet
