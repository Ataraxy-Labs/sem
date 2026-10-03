//! Config the system runs with: env definitions (dotenv, compose, k8s/helm
//! manifests, Dockerfiles, platform specs), and plugin manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;

/// Where each env var is defined.
#[derive(Clone, Debug, Default, Serialize)]
pub struct EnvDefs {
    pub keys: BTreeMap<String, BTreeSet<String>>,
}

impl EnvDefs {
    fn add(&mut self, key: &str, file: &str) {
        let k = key.trim().trim_matches(['"', '\'']);
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
            return;
        }
        self.keys.entry(k.to_string()).or_default().insert(file.to_string());
    }
    pub fn defined(&self, key: &str) -> bool {
        self.keys.contains_key(key)
    }
}

/// A plugin manifest: an ordered list of plugin names and where they live.
#[derive(Clone, Debug, Serialize)]
pub struct Manifest {
    pub file: String,
    pub kind: String,
    /// (plugin name, package / command / entry point)
    pub entries: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ConfigFacts {
    pub env: EnvDefs,
    pub manifests: Vec<Manifest>,
    pub files: Vec<String>,
}

fn is_env_file(name: &str) -> bool {
    name == ".env" || name.starts_with(".env.") || name.ends_with(".env") || name == ".envrc"
}

fn is_yaml(name: &str) -> bool {
    name.ends_with(".yml") || name.ends_with(".yaml")
}

/// Collect config facts from `files` (paths relative to `root`).
pub fn collect(root: &Path, files: &[String]) -> ConfigFacts {
    let mut out = ConfigFacts::default();
    for f in files {
        let name = f.rsplit('/').next().unwrap_or(f);
        let path = root.join(f);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if text.len() > 4_000_000 {
            continue;
        }
        let before = out.env.keys.len() + out.manifests.len();
        if is_env_file(name) {
            for line in text.lines() {
                let l = line.trim().trim_start_matches("export ");
                if l.starts_with('#') {
                    continue;
                }
                if let Some((k, _)) = l.split_once('=') {
                    out.env.add(k, f);
                }
            }
        } else if name.starts_with("Dockerfile") || name.ends_with(".Dockerfile") || name == "Containerfile" {
            for line in text.lines() {
                let l = line.trim();
                let rest = l.strip_prefix("ENV ").or_else(|| l.strip_prefix("ARG "));
                if let Some(rest) = rest {
                    if rest.contains('=') {
                        for part in rest.split_whitespace() {
                            if let Some((k, _)) = part.split_once('=') {
                                out.env.add(k, f);
                            }
                        }
                    } else if let Some(k) = rest.split_whitespace().next() {
                        out.env.add(k, f);
                    }
                }
            }
        } else if is_yaml(name) {
            // serde_yaml's document iterator can keep yielding the same error
            // forever on templated YAML (helm `{{ }}`): stop at the first error,
            // and bound the document count
            for doc in serde_yaml::Deserializer::from_str(&text).take(2000) {
                match serde_yaml::Value::deserialize_from(doc) {
                    Ok(v) => yaml_env(&v, f, &mut out.env, 0),
                    Err(_) => break,
                }
            }
        } else if name == "plugin.cfg" {
            let entries = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(|l| l.split_once(':'))
                .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
                .collect();
            out.manifests.push(Manifest { file: f.clone(), kind: "plugin.cfg".into(), entries });
        } else if name == "pyproject.toml" || name == "book.toml" || name == "Cargo.toml" || name == "fly.toml" {
            if let Ok(v) = text.parse::<toml::Value>() {
                toml_manifest(&v, f, name, &mut out);
            }
        } else if name == "setup.cfg" {
            let mut in_ep = false;
            let mut entries = Vec::new();
            for line in text.lines() {
                let t = line.trim();
                if t.starts_with('[') {
                    in_ep = t == "[options.entry_points]";
                } else if in_ep && t.contains('=') && line.starts_with(char::is_whitespace) {
                    let (n, v) = t.split_once('=').unwrap();
                    entries.push((n.trim().to_string(), v.trim().to_string()));
                }
            }
            if !entries.is_empty() {
                out.manifests.push(Manifest { file: f.clone(), kind: "entry_points".into(), entries });
            }
        }
        if out.env.keys.len() + out.manifests.len() > before {
            out.files.push(f.clone());
        }
    }
    out
}

trait FromDoc {
    fn deserialize_from(d: serde_yaml::Deserializer) -> Result<serde_yaml::Value, serde_yaml::Error>;
}
impl FromDoc for serde_yaml::Value {
    fn deserialize_from(d: serde_yaml::Deserializer) -> Result<serde_yaml::Value, serde_yaml::Error> {
        use serde::Deserialize;
        serde_yaml::Value::deserialize(d)
    }
}

/// `env:` / `environment:` / `envs:` blocks anywhere in a YAML document:
/// a list of `{name|key: X}` / `"X=v"` items, or a mapping of X -> v.
fn yaml_env(v: &serde_yaml::Value, file: &str, env: &mut EnvDefs, depth: usize) {
    if depth > 30 {
        return;
    }
    match v {
        serde_yaml::Value::Mapping(m) => {
            for (k, val) in m {
                let key = k.as_str().unwrap_or("");
                if matches!(key, "env" | "environment" | "envs" | "envFrom" | "extraEnv" | "variables") {
                    match val {
                        serde_yaml::Value::Sequence(items) => {
                            for it in items {
                                match it {
                                    serde_yaml::Value::String(s) => {
                                        env.add(s.split('=').next().unwrap_or(s), file);
                                    }
                                    serde_yaml::Value::Mapping(im) => {
                                        for f in ["name", "key"] {
                                            if let Some(n) = im.get(f).and_then(|x| x.as_str()) {
                                                env.add(n, file);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        serde_yaml::Value::Mapping(em) => {
                            for ek in em.keys() {
                                if let Some(s) = ek.as_str() {
                                    env.add(s, file);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                yaml_env(val, file, env, depth + 1);
            }
        }
        serde_yaml::Value::Sequence(s) => {
            for x in s {
                yaml_env(x, file, env, depth + 1);
            }
        }
        _ => {}
    }
}

fn toml_manifest(v: &toml::Value, f: &str, name: &str, out: &mut ConfigFacts) {
    match name {
        "pyproject.toml" => {
            let eps = v
                .get("project")
                .and_then(|p| p.get("entry-points"))
                .or_else(|| v.get("tool").and_then(|t| t.get("poetry")).and_then(|p| p.get("plugins")));
            let mut entries = Vec::new();
            if let Some(t) = eps.and_then(|e| e.as_table()) {
                for (group, items) in t {
                    if let Some(it) = items.as_table() {
                        for (n, target) in it {
                            entries.push((format!("{group}:{n}"), target.as_str().unwrap_or("").to_string()));
                        }
                    }
                }
            }
            if let Some(scripts) = v.get("project").and_then(|p| p.get("scripts")).and_then(|s| s.as_table()) {
                for (n, target) in scripts {
                    entries.push((format!("console_scripts:{n}"), target.as_str().unwrap_or("").to_string()));
                }
            }
            if !entries.is_empty() {
                out.manifests.push(Manifest { file: f.into(), kind: "entry_points".into(), entries });
            }
        }
        "book.toml" => {
            let mut entries = Vec::new();
            for section in ["preprocessor", "output"] {
                if let Some(t) = v.get(section).and_then(|x| x.as_table()) {
                    for (n, cfg) in t {
                        let cmd = cfg.get("command").and_then(|c| c.as_str()).unwrap_or("");
                        entries.push((format!("{section}:{n}"), cmd.to_string()));
                    }
                }
            }
            if !entries.is_empty() {
                out.manifests.push(Manifest { file: f.into(), kind: "book.toml".into(), entries });
            }
        }
        "fly.toml" => {
            if let Some(t) = v.get("env").and_then(|e| e.as_table()) {
                for k in t.keys() {
                    out.env.add(k, f);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_sources() {
        let d = std::env::temp_dir().join(format!("sem-system-config-{}", std::process::id()));
        std::fs::create_dir_all(d.join("k8s")).unwrap();
        std::fs::write(d.join(".env"), "# c\nDB_URL=x\nexport TOKEN=1\n").unwrap();
        std::fs::write(d.join("Dockerfile"), "FROM x\nENV PORT=8080 MODE=prod\nARG BUILD\n").unwrap();
        std::fs::write(
            d.join("k8s/dep.yaml"),
            "kind: Deployment\nspec:\n  containers:\n  - env:\n    - name: REDIS_ADDR\n      value: r\n---\nservices:\n  api:\n    environment:\n      SECRET: s\n",
        )
        .unwrap();
        std::fs::write(d.join("plugin.cfg"), "# x\nlog:log\ncache:github.com/a/cache\n").unwrap();
        let files: Vec<String> = [".env", "Dockerfile", "k8s/dep.yaml", "plugin.cfg"].iter().map(|s| s.to_string()).collect();
        let c = collect(&d, &files);
        for k in ["DB_URL", "TOKEN", "PORT", "MODE", "BUILD", "REDIS_ADDR", "SECRET"] {
            assert!(c.env.defined(k), "{k}");
        }
        assert_eq!(c.manifests[0].entries[1].1, "github.com/a/cache");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn templated_yaml_terminates() {
        let d = std::env::temp_dir().join(format!("sem-system-config-tpl-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("t.yaml"), "a: {{ .Values.x }}\n---\nenv:\n  - name: {{ bad\n").unwrap();
        let c = collect(&d, &["t.yaml".to_string()]);
        assert!(c.env.keys.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
