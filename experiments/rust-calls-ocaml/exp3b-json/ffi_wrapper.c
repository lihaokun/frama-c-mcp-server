#include <caml/mlvalues.h>
#include <caml/callback.h>
#include <caml/memory.h>
#include <caml/alloc.h>
#include <string.h>
#include <stdlib.h>

void ocaml_startup(void) {
    char *argv[] = {"ocaml_embedded", NULL};
    caml_startup(argv);
}

/* Takes a JSON string, passes it to OCaml, returns malloc'd result string */
char *ocaml_process_json(const char *json_input) {
    CAMLparam0();
    CAMLlocal1(ml_input);
    ml_input = caml_copy_string(json_input);
    const value *closure = caml_named_value("process_json");
    if (closure == NULL) {
        CAMLreturnT(char*, NULL);
    }
    value result = caml_callback(*closure, ml_input);
    const char *s = String_val(result);
    char *copy = strdup(s);
    CAMLreturnT(char*, copy);
}
