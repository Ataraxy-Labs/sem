//! Graph algorithms over an adjacency list (`adj[u]` = successors of `u`).
//! All iterative (no recursion limits), deterministic, and parallel where the
//! work is embarrassingly per-source.

use std::collections::VecDeque;

use rayon::prelude::*;

pub type Adj = Vec<Vec<usize>>;

pub fn reverse(adj: &Adj) -> Adj {
    let mut r = vec![Vec::new(); adj.len()];
    for (u, vs) in adj.iter().enumerate() {
        for &v in vs {
            r[v].push(u);
        }
    }
    r
}

/// Tarjan's strongly connected components. Returns (component id per node, components),
/// components in reverse topological order of the condensation (sinks first).
pub fn scc(adj: &Adj) -> (Vec<usize>, Vec<Vec<usize>>) {
    let n = adj.len();
    let (mut index, mut low, mut on) = (vec![usize::MAX; n], vec![0; n], vec![false; n]);
    let (mut stack, mut comps, mut comp) = (Vec::new(), Vec::new(), vec![usize::MAX; n]);
    let mut next = 0;
    for s in 0..n {
        if index[s] != usize::MAX {
            continue;
        }
        let mut call: Vec<(usize, usize)> = vec![(s, 0)];
        index[s] = next;
        low[s] = next;
        next += 1;
        stack.push(s);
        on[s] = true;
        while let Some(&mut (u, ref mut i)) = call.last_mut() {
            if *i < adj[u].len() {
                let v = adj[u][*i];
                *i += 1;
                if index[v] == usize::MAX {
                    index[v] = next;
                    low[v] = next;
                    next += 1;
                    stack.push(v);
                    on[v] = true;
                    call.push((v, 0));
                } else if on[v] {
                    low[u] = low[u].min(index[v]);
                }
            } else {
                call.pop();
                if let Some(&(p, _)) = call.last() {
                    low[p] = low[p].min(low[u]);
                }
                if low[u] == index[u] {
                    let mut c = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on[w] = false;
                        comp[w] = comps.len();
                        c.push(w);
                        if w == u {
                            break;
                        }
                    }
                    c.sort_unstable();
                    comps.push(c);
                }
            }
        }
    }
    (comp, comps)
}

/// Nodes on a directed cycle (in an SCC of size > 1, or with a self-loop).
pub fn cyclic_nodes(adj: &Adj) -> Vec<bool> {
    let (comp, comps) = scc(adj);
    (0..adj.len()).map(|u| comps[comp[u]].len() > 1 || adj[u].contains(&u)).collect()
}

/// Longest path (in edges) through the condensation DAG — cycle-safe depth.
pub fn condensation_depth(adj: &Adj) -> usize {
    let (comp, comps) = scc(adj);
    // comps are sinks-first, so a component's successors are already finalized
    let mut depth = vec![0usize; comps.len()];
    for (c, members) in comps.iter().enumerate() {
        let mut d = 0;
        for &u in members {
            for &v in &adj[u] {
                if comp[v] != c {
                    d = d.max(depth[comp[v]] + 1);
                }
            }
        }
        depth[c] = d;
    }
    depth.into_iter().max().unwrap_or(0)
}

/// Nodes reachable from `s` (excluding `s` unless on a cycle back to it).
pub fn reach(adj: &Adj, s: usize) -> Vec<usize> {
    let mut seen = vec![false; adj.len()];
    let mut q = VecDeque::from([s]);
    let mut out = Vec::new();
    while let Some(u) = q.pop_front() {
        for &v in &adj[u] {
            if !seen[v] {
                seen[v] = true;
                out.push(v);
                q.push_back(v);
            }
        }
    }
    out.sort_unstable();
    out
}

/// Number of nodes reachable from every node (excluding itself), in parallel.
pub fn reach_counts(adj: &Adj) -> Vec<usize> {
    (0..adj.len())
        .into_par_iter()
        .map(|s| {
            let mut seen = vec![false; adj.len()];
            seen[s] = true;
            let mut q = VecDeque::from([s]);
            let mut c = 0;
            while let Some(u) = q.pop_front() {
                for &v in &adj[u] {
                    if !seen[v] {
                        seen[v] = true;
                        c += 1;
                        q.push_back(v);
                    }
                }
            }
            c
        })
        .collect()
}

/// Unit-cost shortest path s -> t (BFS), as a node sequence.
pub fn shortest_path(adj: &Adj, s: usize, t: usize) -> Option<Vec<usize>> {
    let mut prev = vec![usize::MAX; adj.len()];
    let mut seen = vec![false; adj.len()];
    seen[s] = true;
    let mut q = VecDeque::from([s]);
    while let Some(u) = q.pop_front() {
        if u == t {
            let mut path = vec![t];
            let mut c = t;
            while c != s {
                c = prev[c];
                path.push(c);
            }
            path.reverse();
            return Some(path);
        }
        let mut succ = adj[u].clone();
        succ.sort_unstable();
        for v in succ {
            if !seen[v] {
                seen[v] = true;
                prev[v] = u;
                q.push_back(v);
            }
        }
    }
    None
}

/// Weakly connected components (union-find over the undirected projection).
pub fn weak_components(adj: &Adj) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for (u, vs) in adj.iter().enumerate() {
        for &v in vs {
            let (a, b) = (find(&mut parent, u), find(&mut parent, v));
            if a != b {
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for u in 0..n {
        let r = find(&mut parent, u);
        groups.entry(r).or_default().push(u);
    }
    groups.into_values().collect()
}

/// Articulation points of the undirected projection (iterative Hopcroft–Tarjan).
pub fn articulation_points(adj: &Adj) -> Vec<usize> {
    let n = adj.len();
    let mut und = vec![Vec::new(); n];
    for (u, vs) in adj.iter().enumerate() {
        for &v in vs {
            if u != v {
                und[u].push(v);
                und[v].push(u);
            }
        }
    }
    for l in &mut und {
        l.sort_unstable();
        l.dedup();
    }
    let (mut disc, mut low, mut is_ap) = (vec![usize::MAX; n], vec![0; n], vec![false; n]);
    let mut t = 0;
    for root in 0..n {
        if disc[root] != usize::MAX {
            continue;
        }
        let mut children = 0;
        disc[root] = t;
        low[root] = t;
        t += 1;
        let mut st: Vec<(usize, usize, usize)> = vec![(root, usize::MAX, 0)];
        while let Some(&mut (u, parent, ref mut i)) = st.last_mut() {
            if *i < und[u].len() {
                let v = und[u][*i];
                *i += 1;
                if disc[v] == usize::MAX {
                    disc[v] = t;
                    low[v] = t;
                    t += 1;
                    if u == root {
                        children += 1;
                    }
                    st.push((v, u, 0));
                } else if v != parent {
                    low[u] = low[u].min(disc[v]);
                }
            } else {
                st.pop();
                if let Some(&(p, _, _)) = st.last() {
                    low[p] = low[p].min(low[u]);
                    if p != root && low[u] >= disc[p] {
                        is_ap[p] = true;
                    }
                }
            }
        }
        if children > 1 {
            is_ap[root] = true;
        }
    }
    (0..n).filter(|&u| is_ap[u]).collect()
}

/// Brandes betweenness centrality on the directed, unweighted graph (unnormalized).
pub fn betweenness(adj: &Adj) -> Vec<f64> {
    let n = adj.len();
    (0..n)
        .into_par_iter()
        .fold(
            || vec![0.0f64; n],
            |mut cb, s| {
                let mut stack = Vec::with_capacity(n);
                let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
                let mut sigma = vec![0.0f64; n];
                let mut dist = vec![-1i64; n];
                sigma[s] = 1.0;
                dist[s] = 0;
                let mut q = VecDeque::from([s]);
                while let Some(v) = q.pop_front() {
                    stack.push(v);
                    for &w in &adj[v] {
                        if dist[w] < 0 {
                            dist[w] = dist[v] + 1;
                            q.push_back(w);
                        }
                        if dist[w] == dist[v] + 1 {
                            sigma[w] += sigma[v];
                            preds[w].push(v);
                        }
                    }
                }
                let mut delta = vec![0.0f64; n];
                while let Some(w) = stack.pop() {
                    for &v in &preds[w] {
                        delta[v] += sigma[v] / sigma[w] * (1.0 + delta[w]);
                    }
                    if w != s {
                        cb[w] += delta[w];
                    }
                }
                cb
            },
        )
        .reduce(|| vec![0.0f64; n], |mut a, b| {
            for (x, y) in a.iter_mut().zip(b) {
                *x += y;
            }
            a
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scc_finds_cycles_and_orders_sinks_first() {
        // 0 -> 1 -> 2 -> 0, 2 -> 3
        let adj = vec![vec![1], vec![2], vec![0, 3], vec![]];
        let (comp, comps) = scc(&adj);
        assert_eq!(comps.len(), 2);
        assert_eq!(comp[0], comp[1]);
        assert_eq!(comp[1], comp[2]);
        assert_eq!(comps[0], vec![3]);
        assert_eq!(condensation_depth(&adj), 1);
        assert_eq!(cyclic_nodes(&adj), vec![true, true, true, false]);
    }

    #[test]
    fn articulation_points_of_a_bowtie() {
        // 0-1-2 triangle, 2-3-4 triangle: 2 is the cut vertex
        let adj = vec![vec![1], vec![2], vec![0, 3], vec![4], vec![2]];
        assert_eq!(articulation_points(&adj), vec![2]);
    }

    #[test]
    fn betweenness_of_a_chain() {
        let adj = vec![vec![1], vec![2], vec![]];
        assert_eq!(betweenness(&adj), vec![0.0, 1.0, 0.0]);
    }

    #[test]
    fn paths_and_reach() {
        let adj = vec![vec![1, 2], vec![3], vec![3], vec![]];
        assert_eq!(shortest_path(&adj, 0, 3), Some(vec![0, 1, 3]));
        assert_eq!(reach(&adj, 0), vec![1, 2, 3]);
        assert_eq!(reach_counts(&adj), vec![3, 1, 1, 0]);
        assert_eq!(weak_components(&adj).len(), 1);
    }
}
