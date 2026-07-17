//! Topological sort for verify-program-fsm v1.
//!
//! Tarjan SCC + petgraph::condensation + Kahn 分层。
//! 跟 `compute_topological_order` MCP 工具配套，输出含 SCC group 标记的 levels。
//!
//! 详见 docs/design/verify-program-fsm/detailed-design.md §5.1.3。

use std::collections::HashMap;

use petgraph::algo::{condensation, tarjan_scc};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

use crate::state::{Level, SccGroup};

#[derive(Debug, thiserror::Error)]
pub enum TopoError {
    #[error("internal: {0}")]
    Internal(String),
}

/// 输入 callgraph (vertices, edges) → 输出 levels (按 SCC 分组)。
///
/// 算法 (详见 detailed-design §5.1.2-§5.1.3):
/// 1. 建 DiGraph (edge: caller → callee)
/// 2. Tarjan SCC + 算 is_cycle (size≥2 或 size=1+self-loop)
/// 3. petgraph::condensation 凝缩 DAG (每节点 = Vec<member name>)
/// 4. 在 condensed 上 Kahn 分层 (level 0 = leaf SCC = 无 outgoing)
/// 5. 组织 Level + SccGroup 输出 (members + groups 都按 name/id 排序，确保确定性)
///
/// Vertices 中不在 edges 出现的孤立节点也会包含在输出（成单成员 SCC，is_cycle=false）。
pub fn compute_topological_order(
    vertices: &[String],
    edges: &[(String, String)],
) -> Result<Vec<Level>, TopoError> {
    // ── Step 1: 建 DiGraph ──
    let mut graph = DiGraph::<String, ()>::new();
    let mut name_to_node: HashMap<String, NodeIndex> = HashMap::new();
    for v in vertices {
        if !name_to_node.contains_key(v) {
            let n = graph.add_node(v.clone());
            name_to_node.insert(v.clone(), n);
        }
    }
    for (caller, callee) in edges {
        // Tolerate edges referencing names not in vertices (defensive)
        if let (Some(&c), Some(&d)) = (name_to_node.get(caller), name_to_node.get(callee)) {
            graph.add_edge(c, d, ());
        }
    }

    // ── Step 2: 空图早退 + 快照自环集 ──
    // is_cycle 不再靠「成员名集合反查 HashMap」(脆弱: 名失配/重名 → unwrap_or(false)
    // 静默把真 cycle 漏判成 false)；改为 condensation 后按 condensed 节点直接派生
    // (size≥2 或 单成员自环)，与本文件 compute_ready_functions 同写法。自环信息须在
    // condensation 消费 graph 前快照 (make_acyclic=true 会剔自环边)。
    // 详见 docs/fixes/vpfsm-review-low-items-fix.md M1。
    if graph.node_count() == 0 {
        return Ok(vec![]);
    }
    let self_loop_names: std::collections::HashSet<String> = vertices
        .iter()
        .filter(|v| {
            name_to_node
                .get(*v)
                .is_some_and(|&n| graph.contains_edge(n, n))
        })
        .cloned()
        .collect();

    // ── Step 3: condensation 凝缩 DAG ──
    // condensed 是 DiGraph<Vec<String>, ()>; node weight = 该 SCC 的成员名 list。
    // make_acyclic=true: SCC 间/自环边被剔除（凝缩后理论上不该有，保险）。
    let condensed = condensation(graph, true);

    // ── Step 4: Kahn 分层 (从 leaf 算 level=0 向上) ──
    let n_groups = condensed.node_count();
    let mut out_degree: Vec<usize> = (0..n_groups)
        .map(|i| {
            condensed
                .neighbors_directed(NodeIndex::new(i), Direction::Outgoing)
                .count()
        })
        .collect();
    let mut levels_map: Vec<usize> = vec![0; n_groups];
    let mut current_level = 0usize;
    let mut ready: Vec<NodeIndex> = (0..n_groups)
        .filter(|&i| out_degree[i] == 0)
        .map(NodeIndex::new)
        .collect();

    while !ready.is_empty() {
        for &g in &ready {
            levels_map[g.index()] = current_level;
        }
        let mut next_ready: Vec<NodeIndex> = vec![];
        for &g in &ready {
            for caller in condensed.neighbors_directed(g, Direction::Incoming) {
                out_degree[caller.index()] -= 1;
                if out_degree[caller.index()] == 0 {
                    next_ready.push(caller);
                }
            }
        }
        ready = next_ready;
        current_level += 1;
    }

    // ── Step 5: 组织 SccGroup + Level 输出 ──
    let mut groups: Vec<SccGroup> = (0..n_groups)
        .map(|i| {
            let mut members: Vec<String> = condensed[NodeIndex::new(i)].clone();
            members.sort();
            // is_cycle 直接由 SCC 自身派生：多成员 = 强连通环；单成员 = 看原图自环。
            let is_cycle = members.len() >= 2
                || (members.len() == 1 && self_loop_names.contains(&members[0]));
            SccGroup {
                id: i as u32,
                members,
                level: levels_map[i],
                is_cycle,
            }
        })
        .collect();

    // 按 (level, id) 排序——同 level 内 id 递增
    groups.sort_by_key(|g| (g.level, g.id));

    // 重排 id 为本 level 内连续 (id 0..n)，避免随机散乱
    for (new_id, g) in groups.iter_mut().enumerate() {
        g.id = new_id as u32;
    }

    let max_level = levels_map.iter().max().copied().unwrap_or(0);
    let mut levels: Vec<Level> = (0..=max_level)
        .map(|l| Level {
            level: l,
            groups: vec![],
        })
        .collect();
    for g in groups {
        let lvl = g.level;
        levels[lvl].groups.push(g);
    }

    Ok(levels)
}

/// VO-completeness fix：把 `compute_topological_order` 的 `levels` flatten 成
/// (verification_order, scc_groups)。server 在 compute_topological_order handler 调用之，
/// seed 进 in-memory project_state（agent 不再 build → VO 按构造完整、无 mis-build / 无漏函数）。
///
/// - **verification_order**：按 level 升序（bottom-up，leaf level 0 先）flatten members
/// - **scc_groups**：每组带 `.level`（= 所在 `Level.level`）
/// - **defined 过滤**：只收 `defined` 集内函数（剔除 callgraph 含的 library declared-only）；
///   全 declared-only 的组跳过（不进 VO/scc_groups）——防 VO over-complete → done-gate 永不过
pub fn flatten_levels_to_vo_scc(
    levels: &[Level],
    defined: &std::collections::HashSet<String>,
) -> (Vec<String>, Vec<SccGroup>) {
    let mut vo: Vec<String> = Vec::new();
    let mut scc_groups: Vec<SccGroup> = Vec::new();
    for level in levels {
        for group in &level.groups {
            let members: Vec<String> = group
                .members
                .iter()
                .filter(|m| defined.contains(*m))
                .cloned()
                .collect();
            if members.is_empty() {
                continue;
            }
            vo.extend(members.iter().cloned());
            scc_groups.push(SccGroup {
                id: group.id,
                members,
                level: level.level,
                is_cycle: group.is_cycle,
            });
        }
    }
    (vo, scc_groups)
}

/// fsmint-3 依赖驱动调度：返回「就绪」函数（所有非同-SCC callee 已 merge）。
///
/// **纯函数**：状态全由参数 `done`/`in_progress` 传入，不读任何持久状态（避免 #115 desync）。
///
/// ready(f) ⇔ f∉done ∧ f∉in_progress ∧ ∀ direct callee c: (c∈done) ∨ same_scc(f,c)
///   —— 同-SCC callee **豁免**（占位契约迭代收敛）；SCC 成员各自独立 ready，**不绑组同批**。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadyFunc {
    pub function: String,
    pub scc_id: Option<u32>,      // 属 is_cycle SCC 时填该 SCC 索引（同组成员相同），否则 null
    pub is_cycle: bool,
    pub scc_members: Vec<String>, // is_cycle 时全体成员（排序），否则 [self]
}

pub fn compute_ready_functions(
    vertices: &[String],
    edges: &[(String, String)],
    done: &[String],
    in_progress: &[String],
) -> Vec<ReadyFunc> {
    use std::collections::HashSet;
    // Step P1: Tarjan SCC 缩点 → ready 谓词（SCC 豁免）→ 返回 ReadyFunc

    // ── 建 DiGraph（同 compute_topological_order step1，tolerate 越界 edge）──
    let mut graph = DiGraph::<String, ()>::new();
    let mut name_to_node: HashMap<String, NodeIndex> = HashMap::new();
    for v in vertices {
        name_to_node
            .entry(v.clone())
            .or_insert_with(|| graph.add_node(v.clone()));
    }
    for (caller, callee) in edges {
        if let (Some(&c), Some(&d)) = (name_to_node.get(caller), name_to_node.get(callee)) {
            graph.add_edge(c, d, ());
        }
    }

    // ── Tarjan SCC → scc_of(name→idx) + scc_members + is_cycle ──
    let sccs = tarjan_scc(&graph);
    let mut scc_of: HashMap<String, usize> = HashMap::new();
    let mut scc_members: Vec<Vec<String>> = Vec::with_capacity(sccs.len());
    let mut is_cycle_of: Vec<bool> = Vec::with_capacity(sccs.len());
    for (i, scc) in sccs.iter().enumerate() {
        let is_cycle =
            scc.len() >= 2 || (scc.len() == 1 && graph.contains_edge(scc[0], scc[0]));
        let mut names: Vec<String> = scc.iter().map(|&n| graph[n].clone()).collect();
        for nm in &names {
            scc_of.insert(nm.clone(), i);
        }
        names.sort();
        scc_members.push(names);
        is_cycle_of.push(is_cycle);
    }

    // ── ready 谓词（SCC 豁免）──
    let done_set: HashSet<&str> = done.iter().map(String::as_str).collect();
    let inprog_set: HashSet<&str> = in_progress.iter().map(String::as_str).collect();
    let mut out: Vec<ReadyFunc> = Vec::new();
    for v in vertices {
        if done_set.contains(v.as_str()) || inprog_set.contains(v.as_str()) {
            continue;
        }
        let f_node = name_to_node[v]; // vertices 内必有
        let f_scc = scc_of[v];
        // 用 graph 邻接取 direct callees（自动跳越界 edge，与建图一致）
        let ext_ok = graph
            .neighbors_directed(f_node, Direction::Outgoing)
            .all(|cn| {
                let c = &graph[cn];
                let same_scc = scc_of.get(c) == Some(&f_scc) && is_cycle_of[f_scc];
                same_scc || done_set.contains(c.as_str())
            });
        if ext_ok {
            let is_cycle = is_cycle_of[f_scc];
            out.push(ReadyFunc {
                function: v.clone(),
                scc_id: if is_cycle { Some(f_scc as u32) } else { None },
                is_cycle,
                scc_members: if is_cycle {
                    scc_members[f_scc].clone()
                } else {
                    vec![v.clone()]
                },
            });
        }
    }
    out.sort_by(|a, b| a.function.cmp(&b.function));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(r: &[ReadyFunc]) -> Vec<String> {
        r.iter().map(|x| x.function.clone()).collect()
    }

    #[test]
    fn ready_chain() {
        // a→b→c（INV2）
        let vs: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let es = vec![("a".into(), "b".into()), ("b".into(), "c".into())];
        assert_eq!(names(&compute_ready_functions(&vs, &es, &["c".into()], &[])), vec!["b"]);
        assert_eq!(
            names(&compute_ready_functions(&vs, &es, &["b".into(), "c".into()], &[])),
            vec!["a"]
        );
    }

    // ── VO-completeness fix: flatten_levels_to_vo_scc ──
    fn defset(fs: &[&str]) -> std::collections::HashSet<String> {
        fs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flatten_vo_bottom_up_all_defined() {
        // a→b→c：c 是叶(level 0)，a 是根(高 level)。VO 应 bottom-up（c 先于 a）。
        let vs: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let es = vec![("a".into(), "b".into()), ("b".into(), "c".into())];
        let levels = compute_topological_order(&vs, &es).unwrap();
        let (vo, scc) = flatten_levels_to_vo_scc(&levels, &defset(&["a", "b", "c"]));
        assert_eq!(vo.len(), 3, "VO 含全部 defined");
        let pos = |x: &str| vo.iter().position(|v| v == x).unwrap();
        assert!(pos("c") < pos("a"), "VO bottom-up：叶 c 先于根 a");
        assert_eq!(scc.len(), 3);
        assert!(scc.iter().all(|g| !g.is_cycle && g.members.len() == 1));
    }

    #[test]
    fn flatten_excludes_declared_only() {
        // a→lib，lib 是 library declared-only（不在 defined 集）→ VO 不含 lib（防 over-complete）
        let vs: Vec<String> = vec!["a".into(), "lib".into()];
        let es = vec![("a".into(), "lib".into())];
        let levels = compute_topological_order(&vs, &es).unwrap();
        let (vo, scc) = flatten_levels_to_vo_scc(&levels, &defset(&["a"]));
        assert_eq!(vo, vec!["a".to_string()], "VO 只含 defined 的 a，不含 lib");
        assert!(
            scc.iter().all(|g| !g.members.contains(&"lib".to_string())),
            "全 declared-only 的组被跳过"
        );
    }

    #[test]
    fn flatten_scc_level_carried() {
        // a↔b 环 + a→c：scc_groups 带正确 level + is_cycle，环组 level 高于叶 c
        let vs: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let es = vec![
            ("a".into(), "b".into()),
            ("b".into(), "a".into()),
            ("a".into(), "c".into()),
        ];
        let levels = compute_topological_order(&vs, &es).unwrap();
        let (vo, scc) = flatten_levels_to_vo_scc(&levels, &defset(&["a", "b", "c"]));
        assert_eq!(vo.len(), 3);
        let cyc = scc.iter().find(|g| g.is_cycle).expect("a↔b 应成 is_cycle 组");
        let mut m = cyc.members.clone();
        m.sort();
        assert_eq!(m, vec!["a".to_string(), "b".to_string()]);
        let c_lvl = scc
            .iter()
            .find(|g| g.members == vec!["c".to_string()])
            .unwrap()
            .level;
        assert!(cyc.level > c_lvl, "环组 level 高于叶 c（level 携带正确）");
    }

    #[test]
    fn scc_single_self_loop_is_cycle() {
        // M1 回归：自递归 f→f 是单成员 SCC + 自环 → is_cycle=true（修复后由 condensation
        // 前快照的 self_loop_names 直接派生；旧版名集反查在名失配/重名时会 unwrap_or(false)
        // 静默漏判成 false）。g→f 普通调用、g 无自环 → g 组 is_cycle=false。
        let vs: Vec<String> = vec!["f".into(), "g".into()];
        let es = vec![("f".into(), "f".into()), ("g".into(), "f".into())];
        let levels = compute_topological_order(&vs, &es).unwrap();
        let (_vo, scc) = flatten_levels_to_vo_scc(&levels, &defset(&["f", "g"]));
        let fg = scc
            .iter()
            .find(|g| g.members == vec!["f".to_string()])
            .expect("f 组");
        assert!(fg.is_cycle, "自递归 f→f 应 is_cycle=true");
        let gg = scc
            .iter()
            .find(|g| g.members == vec!["g".to_string()])
            .expect("g 组");
        assert!(!gg.is_cycle, "g 无自环、非环 → is_cycle=false");
    }

    #[test]
    fn ready_scc_not_grouped() {
        // a↔b, 都→c（INV3：SCC 不绑组，各自独立 ready）
        let vs: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let es = vec![
            ("a".into(), "b".into()),
            ("b".into(), "a".into()),
            ("a".into(), "c".into()),
            ("b".into(), "c".into()),
        ];
        let r = compute_ready_functions(&vs, &es, &["c".into()], &[]);
        assert_eq!(names(&r), vec!["a", "b"]); // 外部 callee c done → a,b 各自 ready
        assert!(r.iter().all(|x| x.is_cycle)); // 都标 is_cycle，scc_members 含 [a,b]
        assert!(r.iter().all(|x| x.scc_members == vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn ready_in_progress_excluded() {
        // INV4：done={c}, inprog={b} → ready={}（a 的 callee b 未 done；b 在跑被排除）
        let vs: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let es = vec![("a".into(), "b".into()), ("b".into(), "c".into())];
        assert!(compute_ready_functions(&vs, &es, &["c".into()], &["b".into()]).is_empty());
    }

    #[test]
    fn empty_graph() {
        let levels = compute_topological_order(&[], &[]).unwrap();
        assert!(levels.is_empty());
    }

    #[test]
    fn single_function_no_edges() {
        let levels = compute_topological_order(&["foo".into()], &[]).unwrap();
        // foo 是 level 0, 单成员 group, is_cycle=false
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].level, 0);
        assert_eq!(levels[0].groups.len(), 1);
        assert_eq!(levels[0].groups[0].members, vec!["foo".to_string()]);
        assert!(!levels[0].groups[0].is_cycle);
    }

    #[test]
    fn linear_chain() {
        // foo → bar → baz: level 0 = baz, level 1 = bar, level 2 = foo
        let levels = compute_topological_order(
            &["foo".into(), "bar".into(), "baz".into()],
            &[
                ("foo".into(), "bar".into()),
                ("bar".into(), "baz".into()),
            ],
        ).unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].groups.len(), 1);
        assert_eq!(levels[0].groups[0].members, vec!["baz".to_string()]);
        assert_eq!(levels[1].groups[0].members, vec!["bar".to_string()]);
        assert_eq!(levels[2].groups[0].members, vec!["foo".to_string()]);
    }

    #[test]
    fn two_function_scc() {
        // foo ↔ bar (互递归): 同 level, 同 group, is_cycle=true
        let levels = compute_topological_order(
            &["foo".into(), "bar".into()],
            &[
                ("foo".into(), "bar".into()),
                ("bar".into(), "foo".into()),
            ],
        ).unwrap();
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].groups.len(), 1);
        let g = &levels[0].groups[0];
        assert_eq!(g.members.len(), 2);
        assert!(g.is_cycle);
        assert!(g.members.contains(&"foo".to_string()));
        assert!(g.members.contains(&"bar".to_string()));
    }

    #[test]
    fn self_recursion() {
        // foo → foo (自递归): is_cycle=true, single member SCC
        let levels = compute_topological_order(
            &["foo".into()],
            &[("foo".into(), "foo".into())],
        ).unwrap();
        let g = &levels[0].groups[0];
        assert_eq!(g.members, vec!["foo".to_string()]);
        assert!(g.is_cycle, "size=1 SCC with self-loop must be is_cycle=true");
    }

    #[test]
    fn three_function_scc_plus_caller() {
        // {a, b, c} 互递归 SCC, 加 caller d 调它们
        // level 0 = {a, b, c} SCC, level 1 = d
        let levels = compute_topological_order(
            &["a".into(), "b".into(), "c".into(), "d".into()],
            &[
                ("a".into(), "b".into()),
                ("b".into(), "c".into()),
                ("c".into(), "a".into()),
                ("d".into(), "a".into()),
            ],
        ).unwrap();
        assert_eq!(levels.len(), 2);
        // SCC 在 level 0 (是 d 的依赖)
        let scc_group = &levels[0].groups[0];
        assert!(scc_group.is_cycle);
        assert_eq!(scc_group.members.len(), 3);
        // d 在 level 1
        assert_eq!(levels[1].groups[0].members, vec!["d".to_string()]);
    }

    #[test]
    fn determinism() {
        // 同一组输入跑两次，输出完全一致 (members 排序 + groups 排序)
        let input_v = vec!["foo".into(), "bar".into(), "baz".into()];
        let input_e = vec![
            ("foo".into(), "bar".into()),
            ("foo".into(), "baz".into()),
        ];
        let r1 = compute_topological_order(&input_v, &input_e).unwrap();
        let r2 = compute_topological_order(&input_v, &input_e).unwrap();
        let s1 = serde_json::to_string(&r1).unwrap();
        let s2 = serde_json::to_string(&r2).unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn isolated_and_chain_mixed() {
        // foo 独立, bar → baz: foo 和 baz 都 level 0, bar level 1
        let levels = compute_topological_order(
            &["foo".into(), "bar".into(), "baz".into()],
            &[("bar".into(), "baz".into())],
        ).unwrap();
        assert_eq!(levels.len(), 2);
        // level 0: foo + baz (无 outgoing)
        let level_0_members: Vec<String> = levels[0]
            .groups
            .iter()
            .flat_map(|g| g.members.clone())
            .collect();
        assert!(level_0_members.contains(&"foo".to_string()));
        assert!(level_0_members.contains(&"baz".to_string()));
        // level 1: bar
        assert_eq!(levels[1].groups[0].members, vec!["bar".to_string()]);
    }
}
