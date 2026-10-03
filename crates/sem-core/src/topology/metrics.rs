//! Whole-graph measurements. Signals, not gates: each is a named, exactly
//! defined quantity over the multigraph.
//!
//! * `production` = prod-kind edges (world endpoints kept);
//!   `in_slice` = production without world nodes.
//! * density `D/(N(N-1))` and cyclomatic `E-N+C` count parallel edges;
//!   reach, betweenness and depth collapse them.
//! * propagation cost `Σ|reach(v)∖{v}| / (N(N-1))` (no diagonal).
//! * cyclicity is measured on *capability* edges only (runtime value edges
//!   that are not designations), whatever the reference selection.

use std::collections::HashSet;

use serde::Serialize;

use super::algo;
use super::graph::{FileKind, Graph};
use super::imports::RefKind;

#[derive(Debug, Serialize)]
pub struct Complexity {
    pub node_count: usize,
    pub edge_count: usize,
    pub density_edges: usize,
    pub density: f64,
    pub propagation_cost: f64,
    pub cyclic_module_count: usize,
    pub cyclic_component_count: usize,
    pub cyclic_module_fraction: f64,
    pub betweenness_centralization: f64,
    pub betweenness_centralization_numerator: f64,
    pub betweenness_maximum: f64,
    pub betweenness_nonzero: usize,
    pub betweenness_concentration: f64,
    pub concentration_mean: f64,
    pub average_out_degree: f64,
    pub weak_component_count: usize,
    pub cyclomatic_number: i64,
    pub cyclomatic_normalized: f64,
}

#[derive(Debug, Serialize)]
pub struct NodeMeasures {
    pub name: String,
    pub fan_in: usize,
    pub fan_out: usize,
    pub betweenness: f64,
    pub depth: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub complexity: Complexity,
    pub nodes: Vec<NodeMeasures>,
    pub articulation_points: Vec<String>,
    pub weak_components: Vec<Vec<String>>,
    pub sources: Vec<String>,
    pub sinks: Vec<String>,
}

/// `designation(edge_index)`: the edge is an inert value reference (names only, no authority).
pub fn measure(g: &Graph, designation: &dyn Fn(usize) -> bool) -> Report {
    let n_all = g.nodes.len();
    // world nodes exist in `g` only as endpoints of some edge
    let adj = g.adjacency();
    let rev = algo::reverse(&adj);
    let btw_all = algo::betweenness(&adj);
    let depth = scc_depth(&adj);

    let internal: Vec<usize> = (0..n_all).filter(|&u| !g.is_world(u)).collect();
    let pos: Vec<Option<usize>> = {
        let mut p = vec![None; n_all];
        for (i, &u) in internal.iter().enumerate() {
            p[u] = Some(i);
        }
        p
    };
    let n = internal.len();
    let prod: Vec<usize> = (0..g.edges.len()).filter(|&i| g.edges[i].kind == FileKind::Prod).collect();
    let in_slice: Vec<usize> = prod.iter().copied().filter(|&i| !g.is_world(g.edges[i].from) && !g.is_world(g.edges[i].to)).collect();
    let sub = |edges: &[usize]| -> algo::Adj {
        let mut a = vec![Vec::new(); n];
        for &i in edges {
            let (s, t) = (pos[g.edges[i].from].unwrap(), pos[g.edges[i].to].unwrap());
            if !a[s].contains(&t) {
                a[s].push(t);
            }
        }
        a
    };
    let slice_adj = sub(&in_slice);
    let cap_edges: Vec<usize> = in_slice.iter().copied().filter(|&i| g.edges[i].reference == RefKind::Value && !designation(i)).collect();
    let cap_adj = sub(&cap_edges);

    let nf = n as f64;
    let pairs = nf * (nf - 1.0);
    let density_edges = in_slice.iter().filter(|&&i| g.edges[i].from != g.edges[i].to).count();
    let reach = algo::reach_counts(&slice_adj);
    let cyclic = algo::cyclic_nodes(&cap_adj);
    let (comp, _) = algo::scc(&cap_adj);
    let cyclic_components: HashSet<usize> = (0..n).filter(|&u| cyclic[u]).map(|u| comp[u]).collect();
    let btw_slice = algo::betweenness(&slice_adj);
    let bmax = btw_slice.iter().cloned().fold(0.0, f64::max);
    let numer: f64 = btw_slice.iter().map(|b| bmax - b).sum();
    let denom = (nf - 1.0) * (nf - 1.0) * (nf - 2.0);

    // concentration: betweenness over the production graph, world included in the mean
    let prod_nodes: Vec<usize> = {
        let mut s: Vec<usize> = internal.clone();
        let world_ends: HashSet<usize> = prod.iter().map(|&i| g.edges[i].to).filter(|&t| g.is_world(t)).collect();
        s.extend(world_ends);
        s.sort_unstable();
        s
    };
    let ppos: std::collections::HashMap<usize, usize> = prod_nodes.iter().enumerate().map(|(i, &u)| (u, i)).collect();
    let mut prod_adj = vec![Vec::new(); prod_nodes.len()];
    for &i in &prod {
        let (s, t) = (ppos[&g.edges[i].from], ppos[&g.edges[i].to]);
        if !prod_adj[s].contains(&t) {
            prod_adj[s].push(t);
        }
    }
    let btw_prod = algo::betweenness(&prod_adj);
    let pmax = btw_prod.iter().cloned().fold(0.0, f64::max);
    let pmean = if btw_prod.is_empty() { 0.0 } else { btw_prod.iter().sum::<f64>() / btw_prod.len() as f64 };
    let weak = algo::weak_components(&slice_adj).len();
    let e = in_slice.len() as i64;

    let complexity = Complexity {
        node_count: n,
        edge_count: in_slice.len(),
        density_edges,
        density: if pairs > 0.0 { density_edges as f64 / pairs } else { 0.0 },
        propagation_cost: if n >= 2 { reach.iter().sum::<usize>() as f64 / pairs } else { 0.0 },
        cyclic_module_count: cyclic.iter().filter(|c| **c).count(),
        cyclic_component_count: cyclic_components.len(),
        cyclic_module_fraction: if n > 0 { cyclic.iter().filter(|c| **c).count() as f64 / nf } else { 0.0 },
        betweenness_centralization: if n >= 3 { numer / denom } else { 0.0 },
        betweenness_centralization_numerator: numer,
        betweenness_maximum: bmax,
        betweenness_nonzero: btw_slice.iter().filter(|b| **b > 0.0).count(),
        betweenness_concentration: if pmean > 0.0 { pmax / pmean } else { 0.0 },
        concentration_mean: pmean,
        average_out_degree: if n > 0 { prod.len() as f64 / nf } else { 0.0 },
        weak_component_count: weak,
        cyclomatic_number: e - n as i64 + weak as i64,
        cyclomatic_normalized: if n > 0 { (e - n as i64 + weak as i64) as f64 / nf } else { 0.0 },
    };

    let name = |u: usize| g.nodes[u].id.clone();
    let mut weak_components: Vec<Vec<String>> = algo::weak_components(&adj)
        .into_iter()
        .map(|c| {
            let mut v: Vec<String> = c.into_iter().map(name).collect();
            v.sort();
            v
        })
        .collect();
    weak_components.sort();
    let mut sources: Vec<String> = (0..n_all).filter(|&u| rev[u].is_empty()).map(name).collect();
    let mut sinks: Vec<String> = (0..n_all).filter(|&u| adj[u].is_empty()).map(name).collect();
    sources.sort();
    sinks.sort();
    let mut aps: Vec<String> = algo::articulation_points(&adj).into_iter().map(name).collect();
    aps.sort();
    Report {
        complexity,
        nodes: (0..n_all)
            .map(|u| NodeMeasures { name: name(u), fan_in: rev[u].len(), fan_out: adj[u].len(), betweenness: btw_all[u], depth: depth[u] })
            .collect(),
        articulation_points: aps,
        weak_components,
        sources,
        sinks,
    }
}

/// depth(v) = (|SCC(v)|-1) + L(SCC(v)), L(C) = max over successor components D of |D| + L(D).
pub fn scc_depth(adj: &algo::Adj) -> Vec<usize> {
    let (comp, comps) = algo::scc(adj);
    let mut l = vec![0usize; comps.len()];
    for (c, members) in comps.iter().enumerate() {
        let mut best = 0;
        for &u in members {
            for &v in &adj[u] {
                let d = comp[v];
                if d != c {
                    best = best.max(comps[d].len() + l[d]);
                }
            }
        }
        l[c] = best;
    }
    (0..adj.len()).map(|u| comps[comp[u]].len() - 1 + l[comp[u]]).collect()
}
