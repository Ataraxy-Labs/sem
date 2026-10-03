//! Service contracts: OpenAPI operations, protobuf services, GraphQL root
//! fields. Each is a named operation a client can call across a process
//! boundary; the collector links client call sites to them and them to the
//! handler that serves them.

use std::path::Path;

use regex::Regex;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct HttpOp {
    pub file: String,
    pub method: String,
    /// Full path template, server base path included.
    pub path: String,
    pub operation_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Rpc {
    pub file: String,
    pub service: String,
    pub method: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct GqlField {
    pub file: String,
    pub root: String,
    pub field: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Contracts {
    pub http: Vec<HttpOp>,
    pub rpc: Vec<Rpc>,
    pub graphql: Vec<GqlField>,
    pub files: Vec<String>,
}

pub fn collect(root: &Path, files: &[String]) -> Contracts {
    let mut c = Contracts::default();
    for f in files {
        let lower = f.to_ascii_lowercase();
        let is_spec = lower.ends_with(".yml") || lower.ends_with(".yaml") || lower.ends_with(".json");
        let is_proto = lower.ends_with(".proto");
        let is_gql = lower.ends_with(".graphql") || lower.ends_with(".gql");
        if !(is_spec || is_proto || is_gql) {
            continue;
        }
        if is_spec && (lower.contains("package") || lower.contains("lock") || lower.contains("tsconfig")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(f)) else { continue };
        let before = c.http.len() + c.rpc.len() + c.graphql.len();
        if is_spec {
            if !(text.contains("openapi") || text.contains("swagger")) || !text.contains("paths") {
                continue;
            }
            let v: Option<serde_yaml::Value> = if lower.ends_with(".json") {
                serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|j| serde_yaml::to_value(j).ok())
            } else {
                serde_yaml::from_str(&text).ok()
            };
            if let Some(v) = v {
                openapi(&v, f, &mut c);
            }
        } else if is_proto {
            proto(&text, f, &mut c);
        } else {
            graphql(&text, f, &mut c);
        }
        if c.http.len() + c.rpc.len() + c.graphql.len() > before {
            c.files.push(f.clone());
        }
    }
    c
}

fn openapi(v: &serde_yaml::Value, f: &str, c: &mut Contracts) {
    if v.get("openapi").is_none() && v.get("swagger").is_none() {
        return;
    }
    let mut base = v.get("basePath").and_then(|b| b.as_str()).unwrap_or("").to_string();
    if let Some(url) = v
        .get("servers")
        .and_then(|s| s.as_sequence())
        .and_then(|s| s.first())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
    {
        // "https://host/api" -> "/api"; "/api" stays
        let p = url.splitn(4, '/').collect::<Vec<_>>();
        base = if url.starts_with('/') {
            url.to_string()
        } else if p.len() == 4 {
            format!("/{}", p[3])
        } else {
            String::new()
        };
    }
    let Some(paths) = v.get("paths").and_then(|p| p.as_mapping()) else { return };
    for (p, item) in paths {
        let Some(p) = p.as_str() else { continue };
        let Some(ops) = item.as_mapping() else { continue };
        for (m, op) in ops {
            let Some(m) = m.as_str() else { continue };
            if !matches!(m, "get" | "put" | "post" | "delete" | "options" | "head" | "patch" | "trace") {
                continue;
            }
            c.http.push(HttpOp {
                file: f.into(),
                method: m.to_ascii_uppercase(),
                path: format!("{}{}", base.trim_end_matches('/'), p),
                operation_id: op.get("operationId").and_then(|o| o.as_str()).map(String::from),
            });
        }
    }
}

fn proto(text: &str, f: &str, c: &mut Contracts) {
    static SVC: std::sync::OnceLock<(Regex, Regex)> = std::sync::OnceLock::new();
    let (svc, rpc) = SVC.get_or_init(|| {
        (
            Regex::new(r"(?s)\bservice\s+(\w+)\s*\{(.*?)\n\}").unwrap(),
            Regex::new(r"\brpc\s+(\w+)\s*\(").unwrap(),
        )
    });
    // strip // comments
    let clean: String = text
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    for s in svc.captures_iter(&clean) {
        for r in rpc.captures_iter(&s[2]) {
            c.rpc.push(Rpc { file: f.into(), service: s[1].to_string(), method: r[1].to_string() });
        }
    }
}

fn graphql(text: &str, f: &str, c: &mut Contracts) {
    static R: std::sync::OnceLock<(Regex, Regex)> = std::sync::OnceLock::new();
    let (ty, field) = R.get_or_init(|| {
        (
            Regex::new(r"(?s)\b(?:extend\s+)?type\s+(Query|Mutation|Subscription)\s*\{(.*?)\}").unwrap(),
            Regex::new(r"(?m)^\s*(\w+)\s*[(:]").unwrap(),
        )
    });
    for t in ty.captures_iter(text) {
        for fl in field.captures_iter(&t[2]) {
            c.graphql.push(GqlField { file: f.into(), root: t[1].to_string(), field: fl[1].to_string() });
        }
    }
}

/// Path template → segments, params as `*`.
pub fn path_segments(p: &str) -> Vec<String> {
    let p = p.split(['?', '#']).next().unwrap_or(p);
    // drop scheme://host
    let p = match p.find("://") {
        Some(i) => p[i + 3..].find('/').map(|j| &p[i + 3 + j..]).unwrap_or("/"),
        None => p,
    };
    p.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('{') || s.starts_with(':') || s.starts_with('<') || s.starts_with('$') || s.contains("${")
                || s.contains('{') || s == "%s" || s == "%d" || s == "%v" || s.starts_with('*') || s == "(.*)"
            {
                "*".to_string()
            } else {
                s.to_string()
            }
        })
        .collect()
}

/// How well a client URL matches a served path: `Some(literal segments
/// matched)` when one is a suffix of the other segment-wise (routers mount
/// under prefixes the other side may not show), params matching anything.
pub fn path_match(url: &[String], route: &[String]) -> Option<usize> {
    if url.is_empty() || route.is_empty() {
        return None;
    }
    let n = url.len().min(route.len());
    let (a, b) = (&url[url.len() - n..], &route[route.len() - n..]);
    let mut lits = 0;
    for (x, y) in a.iter().zip(b) {
        if x == "*" || y == "*" {
            continue;
        }
        if x != y {
            return None;
        }
        lits += 1;
    }
    // demand the shorter side be fully covered and carry a literal
    (lits >= 1).then_some(lits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_proto_graphql() {
        let mut c = Contracts::default();
        let v: serde_yaml::Value = serde_yaml::from_str(
            "openapi: 3.0.0\nservers:\n  - url: https://x/api\npaths:\n  /trainer/{id}:\n    get:\n      operationId: getTrainer\n    parameters: []\n",
        )
        .unwrap();
        openapi(&v, "a.yml", &mut c);
        assert_eq!(c.http[0].path, "/api/trainer/{id}");
        assert_eq!(c.http[0].operation_id.as_deref(), Some("getTrainer"));
        proto("service Cart {\n  rpc AddItem(AddItemRequest) returns (Empty) {}\n  // rpc Old(x)\n  rpc GetCart(G) returns (C);\n}\n", "d.proto", &mut c);
        assert_eq!(c.rpc.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(), ["AddItem", "GetCart"]);
        graphql("type Query {\n  user(id: ID!): User\n  users: [User]\n}\n", "s.graphql", &mut c);
        assert_eq!(c.graphql.len(), 2);
    }

    #[test]
    fn matching_paths() {
        let u = path_segments("https://h/api/v1/items/${id}?x=1");
        let r = path_segments("/items/{item_id}");
        assert_eq!(path_match(&u, &r), Some(1));
        assert_eq!(path_match(&path_segments("/users/login"), &path_segments("/users/:id")), Some(1));
        assert_eq!(path_match(&path_segments("/a/b"), &path_segments("/a/c")), None);
    }
}
