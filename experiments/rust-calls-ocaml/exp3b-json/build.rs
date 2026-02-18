use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Step 1: Compile OCaml code with yojson into a complete object file
    let ocaml_obj = out_dir.join("ocaml_lib.o");
    let status = Command::new("ocamlfind")
        .args([
            "ocamlopt",
            "-package", "yojson",
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

    // Step 3: Package OCaml .o into a .a and link
    let status = Command::new("ar")
        .args(["rcs"])
        .arg(out_dir.join("libocaml_lib.a"))
        .arg(&ocaml_obj)
        .status()
        .expect("Failed to run ar");
    assert!(status.success(), "ar failed");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=ocaml_lib");
    println!("cargo:rustc-link-lib=dylib=m");
    println!("cargo:rustc-link-lib=dylib=dl");
    println!("cargo:rustc-link-lib=dylib=pthread");

    println!("cargo:rerun-if-changed=ocaml_lib.ml");
    println!("cargo:rerun-if-changed=ffi_wrapper.c");
}
