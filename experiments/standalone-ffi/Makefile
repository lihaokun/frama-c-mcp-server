.PHONY: all rust ocaml run clean

all: rust ocaml

rust:
	cd rust_lib && cargo build --release

ocaml:
	cd ocaml_caller && dune build

run: all
	cd ocaml_caller && dune exec ./main.exe

clean:
	cd rust_lib && cargo clean
	cd ocaml_caller && dune clean
