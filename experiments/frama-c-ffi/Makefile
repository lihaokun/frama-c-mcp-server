.PHONY: all rust plugin run clean

RUST_LIB_DIR := $(CURDIR)/rust_lib/target/release
PLUGIN := ocaml_plugin/Ffi_test.cmxs

# Get include paths from ocamlfind (frama-c.kernel only, no ctypes needed)
OCAML_FLAGS := $(shell ocamlfind query -r -i-format frama-c.kernel 2>/dev/null)
OCAML_CFLAGS := $(shell ocamlfind ocamlc -where)

all: rust plugin

rust:
	cd rust_lib && cargo build --release

plugin: rust
	# Compile C stubs
	ocamlfind ocamlopt -ccopt "-I $(OCAML_CFLAGS)" \
	  -c ocaml_plugin/rust_stubs.c
	mv rust_stubs.o ocaml_plugin/
	# Compile OCaml source
	ocamlfind ocamlopt $(OCAML_FLAGS) -thread \
	  -c ocaml_plugin/Ffi_test.ml -o ocaml_plugin/Ffi_test.cmx
	# Link into .cmxs
	ocamlfind ocamlopt $(OCAML_FLAGS) -thread \
	  -shared ocaml_plugin/rust_stubs.o ocaml_plugin/Ffi_test.cmx \
	  -cclib -ldl \
	  -o $(PLUGIN)

run: all
	RUST_LIB_DIR=$(RUST_LIB_DIR) \
	  frama-c test/test.c -load-module $(CURDIR)/$(PLUGIN)

clean:
	cd rust_lib && cargo clean
	rm -f ocaml_plugin/*.cmxs ocaml_plugin/*.cmx ocaml_plugin/*.cmi ocaml_plugin/*.o
