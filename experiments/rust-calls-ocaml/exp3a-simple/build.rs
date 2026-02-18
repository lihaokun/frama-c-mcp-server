use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Step 1: Compile OCaml code into a complete object file (includes OCaml runtime)
    let ocaml_obj = out_dir.join("ocaml_lib.o");
    let status = Command::new("ocamlfind")
        .args([
            "ocamlopt",
            "-package", "stdlib",
            "-linkpkg",
            "-output-complete-obj",
            "-o",
        ])
        .arg(&ocaml_obj)
        .arg(manifest_dir.join("ocaml_lib.ml"))
        .status()
        .expect("Failed to run ocamlfind ocamlopt");
    assert!(status.success(), "ocamlfind ocamlopt failed");

    // Step 2: Compile C FFI wrapper
    // Get OCaml include dir for caml headers
    let ocaml_where = Command::new("ocamlfind")
        .args(["ocamlopt", "-where"])
        .output()
        .expect("Failed to get ocaml -where");
    let ocaml_lib_dir = String::from_utf8(ocaml_where.stdout)
        .unwrap()
        .trim()
        .to_string();

    cc::Build::new()
        .file(manifest_dir.join("ffi_wrapper.c"))
        .include(&ocaml_lib_dir)
        .compile("ffi_wrapper");

    // Step 3: Link the OCaml object file
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    // Tell cargo to link the OCaml .o file directly
    // We need to use a static library approach: wrap ocaml_lib.o into a .a
    let status = Command::new("ar")
        .args(["rcs"])
        .arg(out_dir.join("libocaml_lib.a"))
        .arg(&ocaml_obj)
        .status()
        .expect("Failed to run ar");
    assert!(status.success(), "ar failed");

    println!("cargo:rustc-link-lib=static=ocaml_lib");
    // OCaml runtime dependencies
    println!("cargo:rustc-link-lib=dylib=m");
    println!("cargo:rustc-link-lib=dylib=dl");
    println!("cargo:rustc-link-lib=dylib=pthread");

    // Rerun if sources change
    println!("cargo:rerun-if-changed=ocaml_lib.ml");
    println!("cargo:rerun-if-changed=ffi_wrapper.c");
}
