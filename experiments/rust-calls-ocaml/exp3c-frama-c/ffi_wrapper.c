#include <caml/mlvalues.h>
#include <caml/callback.h>
#include <caml/memory.h>
#include <caml/alloc.h>
#include <string.h>
#include <stdlib.h>

void ocaml_startup(void) {
    char *argv[] = {"frama_c_embedded", NULL};
    caml_startup(argv);
}

/* Parse a C file with Frama-C. Returns malloc'd JSON string. */
char *ocaml_parse_c_file(const char *filename) {
    CAMLparam0();
    CAMLlocal1(ml_filename);
    ml_filename = caml_copy_string(filename);
    const value *closure = caml_named_value("parse_c_file");
    if (closure == NULL) {
        CAMLreturnT(char*, strdup("{\"status\":\"error\",\"message\":\"parse_c_file not registered\"}"));
    }
    value result = caml_callback(*closure, ml_filename);
    char *copy = strdup(String_val(result));
    CAMLreturnT(char*, copy);
}

/* List all functions in the parsed AST. Returns malloc'd JSON string. */
char *ocaml_list_functions(void) {
    CAMLparam0();
    const value *closure = caml_named_value("list_functions");
    if (closure == NULL) {
        CAMLreturnT(char*, strdup("{\"status\":\"error\",\"message\":\"list_functions not registered\"}"));
    }
    value result = caml_callback(*closure, Val_unit);
    char *copy = strdup(String_val(result));
    CAMLreturnT(char*, copy);
}

/* Get info about a specific function. Returns malloc'd JSON string. */
char *ocaml_get_function_info(const char *func_name) {
    CAMLparam0();
    CAMLlocal1(ml_name);
    ml_name = caml_copy_string(func_name);
    const value *closure = caml_named_value("get_function_info");
    if (closure == NULL) {
        CAMLreturnT(char*, strdup("{\"status\":\"error\",\"message\":\"get_function_info not registered\"}"));
    }
    value result = caml_callback(*closure, ml_name);
    char *copy = strdup(String_val(result));
    CAMLreturnT(char*, copy);
}
