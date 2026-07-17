/*
 * Benchmark: mini_no_recursion
 *
 * Purpose: happy path. 3 函数无递归调用链 c → b → a (call graph: a→b→c).
 *   主 FSM 应该 topologically sort 出 3 个 SCC (单元素), 派单 batch=[c], 完成后
 *   派 [b], 完成后派 [a], 最后 final_gate 跑全文件 WP.
 *
 * Expected FSM outcome:
 *   - final_gate: PASSED
 *   - completion_map: { c: completed, b: completed, a: completed }
 *   - scc_iteration_counters: 全 0 或缺省 (无 SCC 迭代)
 *   - failure_evidence: null
 *
 * Source design notes:
 *   - 无 cast (Typed+nocast 兼容)
 *   - 所有运算保持在 int 范围内 (调用方 a(0) → b(0)=2 → c(0)=1, 数值小)
 *   - WP 可推 ensures (闭式: c(x)=x+1, b(x)=2*(x+1), a(x)=2*(x+1)+10)
 */

int c(int x);
int b(int x);
int a(int x);

int c(int x) {
    return x + 1;
}

int b(int x) {
    int y = c(x);
    return y + y;
}

int a(int x) {
    int z = b(x);
    return z + 10;
}
