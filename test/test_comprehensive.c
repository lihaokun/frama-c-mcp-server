// Comprehensive test for Phase 2 MCP tools.
// Designed to exercise: EVA alarms, WP unknown goals, globals,
// multi-level call chains, loop invariants, and array access.

int buffer[10];
int buf_size = 10;
int error_count = 0;

// ── Leaf: easily provable ──

/*@ requires 0 <= idx < 10;
    ensures \result == buffer[idx];
    assigns \nothing;
*/
int get_element(int idx) {
    return buffer[idx];
}

// ── Safe division with precondition — WP provable ──

/*@ requires y != 0;
    assigns \nothing;
    ensures \result == x / y;
*/
int safe_div(int x, int y) {
    return x / y;
}

// ── Unsafe division — NO precondition, EVA will alarm ──

int unsafe_div(int x, int y) {
    return x / y;  // EVA: division_by_zero alarm
}

// ── Loop with invariant — WP may struggle ──

/*@ requires 0 <= n <= 10;
    assigns \nothing;
    ensures \result >= 0;
*/
int sum_positive(int n) {
    int sum = 0;
    /*@ loop invariant 0 <= i <= n;
        loop assigns i, sum;
        loop variant n - i;
    */
    for (int i = 0; i < n; i++) {
        int v = buffer[i];
        if (v > 0) sum += v;
    }
    return sum;
}

// ── Deliberately WRONG postcondition — WP will report unknown ──

/*@ assigns \nothing;
    ensures \result > 0;
*/
int identity(int x) {
    return x;  // WRONG: x can be 0 or negative
}

// ── Level 3: validate element ──

/*@ requires 0 <= idx < 10;
    assigns error_count;
    ensures \result >= -1;
*/
int validate(int idx) {
    int val = get_element(idx);
    if (val < 0) {
        error_count++;
        return -1;
    }
    return val;
}

// ── Level 2: process range ──

int process_range(int start, int count) {
    int total = 0;
    for (int i = start; i < start + count && i < buf_size; i++) {
        int v = validate(i);
        if (v >= 0) total += v;
    }
    return total;
}

// ── Level 1: run pipeline ──

int run_pipeline(int n) {
    int subtotal = process_range(0, n);
    int avg = safe_div(subtotal, n);
    return avg;
}

// ── Entry point ──

int main(void) {
    buffer[0] = 10;
    buffer[1] = 20;
    buffer[2] = -5;
    buffer[3] = 15;

    int a = run_pipeline(4);
    int b = unsafe_div(100, a);   // EVA alarm: a could be 0
    int c = identity(0);          // WP: ensures \result > 0 fails
    int d = sum_positive(4);

    return a + b + c + d;
}
