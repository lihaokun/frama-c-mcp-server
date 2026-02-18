/* Simple C file for Frama-C to parse */
int factorial(int n) {
    if (n <= 1) return 1;
    return n * factorial(n - 1);
}

int add(int a, int b) {
    return a + b;
}

int main(void) {
    int x = factorial(5);
    int y = add(3, 4);
    return x + y;
}
