/* Test 1: 旧语法 ghost — ACSL 注解 */
int sum_old_ghost(int *a, int n) {
  int s = 0;
  /*@ ghost int gs = 0; */
  for (int i = 0; i < n; i++) {
    s += a[i];
    /*@ ghost gs += a[i]; */
  }
  /*@ assert s == gs; */
  return s;
}

/* Test 2: __attribute__((ghost)) 语法 */
int sum_attr_ghost(int *a, int n) {
  int s = 0;
  int __attribute__((ghost)) gs2 = 0;
  for (int i = 0; i < n; i++) {
    s += a[i];
  }
  return s;
}

/* Test 3: \ghost 关键字 in spec */
/*@ ghost int global_ghost; */
