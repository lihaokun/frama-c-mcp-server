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

int ocaml_fib(int n) {
    const value *closure = caml_named_value("fib");
    if (closure == NULL) return -1;
    value result = caml_callback(*closure, Val_int(n));
    return Int_val(result);
}

/* Returns a malloc'd string that the caller must free */
char *ocaml_greet(const char *name) {
    CAMLparam0();
    CAMLlocal1(ml_name);
    ml_name = caml_copy_string(name);
    const value *closure = caml_named_value("greet");
    if (closure == NULL) {
        CAMLreturnT(char*, NULL);
    }
    value result = caml_callback(*closure, ml_name);
    const char *s = String_val(result);
    char *copy = strdup(s);
    CAMLreturnT(char*, copy);
}
