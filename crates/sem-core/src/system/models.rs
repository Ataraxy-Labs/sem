//! Declarative boundary models (`models.toml`) and the matcher that turns
//! scanned syntax facts into boundary sites.

use serde::{Deserialize, Serialize};

use super::scan::{Arg, CallFact, FileScan, Lang, MemberRead};
use super::sql;

pub const DEFAULT_MODELS: &str = include_str!("models.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Sql,
    Env,
    HttpOut,
    RpcOut,
    Queue,
    Dispatch,
    RouteIn,
    Subprocess,
    Fs,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Sql => "sql",
            Kind::Env => "env",
            Kind::HttpOut => "http_out",
            Kind::RpcOut => "rpc_out",
            Kind::Queue => "queue",
            Kind::Dispatch => "dispatch",
            Kind::RouteIn => "route_in",
            Kind::Subprocess => "subprocess",
            Kind::Fs => "fs",
        }
    }
    /// Kinds that end a source→sink path.
    pub fn is_sink(&self) -> bool {
        matches!(self, Kind::Sql | Kind::HttpOut | Kind::RpcOut | Kind::Queue | Kind::Subprocess | Kind::Fs)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ArgSel {
    Index(usize),
    Named(String),
}

#[derive(Clone, Debug, Deserialize)]
pub struct CallModel {
    pub kind: Kind,
    pub langs: Vec<String>,
    pub callee: Vec<String>,
    pub arg: Option<ArgSel>,
    #[serde(default)]
    pub min_args: usize,
    pub decorator: Option<bool>,
    pub handler: Option<String>,
    pub method: Option<String>,
    #[serde(default)]
    pub receiver: Vec<String>,
    #[serde(default)]
    pub require_sql: bool,
    #[serde(default)]
    pub orm: bool,
    #[serde(default)]
    pub path_like: bool,
    #[serde(default)]
    pub dynamic_only: bool,
    pub prefix_class_decorator: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MemberModel {
    pub kind: Kind,
    pub langs: Vec<String>,
    pub text: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RegistryModel {
    pub langs: Vec<String>,
    pub register: Vec<String>,
    pub lookup: Vec<String>,
    pub key_arg: usize,
    pub value_arg: ArgSel,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Models {
    #[serde(default)]
    pub call: Vec<CallModel>,
    #[serde(default)]
    pub member: Vec<MemberModel>,
    #[serde(default)]
    pub registry: Vec<RegistryModel>,
}

impl Models {
    pub fn load(extra: Option<&str>) -> Result<Models, String> {
        let mut m: Models = toml::from_str(DEFAULT_MODELS).map_err(|e| format!("models.toml: {e}"))?;
        if let Some(x) = extra {
            let more: Models = toml::from_str(x).map_err(|e| format!("extra models: {e}"))?;
            m.call.extend(more.call);
            m.member.extend(more.member);
            m.registry.extend(more.registry);
        }
        Ok(m)
    }
}

/// Glob with `*` only, anchored at both ends.
pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut pos = 0;
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        if i == 0 {
            if !s.starts_with(p) {
                return false;
            }
            pos = p.len();
        } else if i == parts.len() - 1 {
            return s.len() >= pos + p.len() && s[pos..].ends_with(p);
        } else {
            match s[pos..].find(p) {
                Some(j) => pos += j + p.len(),
                None => return false,
            }
        }
    }
    true
}

/// One place data crosses the code boundary.
#[derive(Clone, Debug, Serialize)]
pub struct BoundarySite {
    pub file: String,
    pub byte: usize,
    pub line: usize,
    pub lang: Lang,
    pub kind: Kind,
    /// Callee or member text that matched.
    pub text: String,
    /// The literal key / SQL / URL / path, when there is one.
    pub key: Option<String>,
    /// Literal fragments of a runtime-built key (template, concatenation).
    pub fragments: Option<String>,
    /// HTTP method (routes, outbound HTTP).
    pub method: Option<String>,
    /// Route handler name (route_in).
    pub handler: Option<String>,
    /// ORM operation: the model names mentioned in the call.
    pub orm: bool,
    pub orm_text: Option<String>,
    /// Route prefix from the enclosing class (`@Controller('x')`).
    pub prefix: Option<String>,
}

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options", "all", "any"];

fn last_segment(callee: &str) -> &str {
    let c = callee.trim_end_matches("()").trim_end_matches('!');
    c.rsplit(['.', ':']).next().unwrap_or(c)
}

fn receiver(callee: &str) -> &str {
    let c = callee.trim_end_matches("()");
    match c.rfind(['.', ':']) {
        Some(i) => &c[..i],
        None => "",
    }
}

fn pick_arg<'a>(args: &'a [Arg], sel: &Option<ArgSel>) -> Option<&'a Arg> {
    let positional: Vec<&Arg> = args.iter().filter(|a| !matches!(a, Arg::Kw(..))).collect();
    match sel {
        None => None,
        Some(ArgSel::Index(i)) => positional.get(*i).copied(),
        Some(ArgSel::Named(n)) if n == "last" => positional.last().copied(),
        Some(ArgSel::Named(n)) if n == "sql" => positional
            .iter()
            .find(|a| a.text_fragments().is_some_and(sql::is_sql))
            .copied()
            .or_else(|| positional.first().copied()),
        Some(ArgSel::Named(n)) if n == "url" => positional
            .iter()
            .find(|a| a.text_fragments().is_some_and(|t| t.contains('/') || t.starts_with("http")))
            .copied()
            .or_else(|| positional.get(1).copied()),
        Some(ArgSel::Named(_)) => None,
    }
}

fn handler_of(call: &CallFact, how: &str) -> Option<String> {
    match how {
        "decorated" => call.decorates.as_ref().map(|(n, _)| n.clone()),
        "last" => match call.args.last() {
            Some(Arg::Name(n)) => Some(n.clone()),
            // an inline handler: its body belongs to the registering entity
            Some(Arg::Expr(e)) if is_inline_fn(e) => None,
            Some(Arg::Expr(e)) => inner_name(e),
            _ => None,
        },
        // `add_route(IndexView.as_view(self), regex)`: the leading dotted path
        "first" => match call.args.first() {
            Some(Arg::Name(n)) => Some(n.clone()),
            Some(Arg::Expr(e)) => {
                let head: String = e.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.').collect();
                let head = head.trim_end_matches(".as_view").trim_end_matches('.');
                (!head.is_empty()).then(|| head.to_string())
            }
            _ => None,
        },
        "to" | "inner" => call.args.iter().find_map(|a| match a {
            Arg::Expr(e) => inner_name(e),
            Arg::Name(n) => Some(n.clone()),
            _ => None,
        }),
        _ => None,
    }
}

fn is_inline_fn(e: &str) -> bool {
    e.contains("=>") || e.starts_with("function") || e.starts_with("async") || e.starts_with("func(") || e.starts_with("lambda")
}

/// `web::get().to(handler)` / `get(handler)` / `asyncHandler(h)` → `handler`.
fn inner_name(e: &str) -> Option<String> {
    let re = regex_cache();
    re.captures_iter(e)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|n| !n.is_empty() && !n.chars().next().unwrap().is_ascii_digit())
        .last()
}

fn regex_cache() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"\(([A-Za-z_][\w:.]*)\)").unwrap())
}

fn is_path_like(s: &str) -> bool {
    s.starts_with('/') || s.starts_with(':') || s.contains('/') || s.is_empty() || s.starts_with('^')
}

/// Match one file's facts against the models.
pub fn match_file(models: &Models, file: &str, lang: Lang, scan: &FileScan) -> Vec<BoundarySite> {
    let l = lang.as_str();
    let mut out: Vec<BoundarySite> = Vec::new();
    for call in &scan.calls {
        // a call chain `db.Model(x).Where(..).Count(..)` is one site: the
        // outermost call (visited first) owns every call starting at its byte
        if out.iter().any(|b| b.byte == call.byte) {
            continue;
        }
        for m in &models.call {
            if !m.langs.iter().any(|x| x == l) || !m.callee.iter().any(|p| glob(p, &call.callee)) {
                continue;
            }
            if let Some(d) = m.decorator {
                if d != call.decorates.is_some() {
                    continue;
                }
            } else if call.decorates.is_some() && m.kind != Kind::RouteIn {
                continue;
            }
            let positional = call.args.iter().filter(|a| !matches!(a, Arg::Kw(..))).count();
            if positional < m.min_args {
                continue;
            }
            if !m.receiver.is_empty() {
                let r = receiver(&call.callee).to_ascii_lowercase();
                if !m.receiver.iter().any(|x| r.contains(x.as_str())) {
                    continue;
                }
            }
            let a = pick_arg(&call.args, &m.arg);
            let key = a.and_then(|a| a.literal()).map(|s| s.to_string());
            let fragments = a.and_then(|a| match a {
                Arg::Tmpl(t) => Some(t.clone()),
                _ => None,
            });
            if m.require_sql && !a.and_then(|a| a.text_fragments()).is_some_and(sql::is_sql) {
                continue;
            }
            if m.dynamic_only && key.is_some() {
                continue;
            }
            // `options.BaseURL + "/trainer/calendar"`: the literal tail is the path
            let key = match (key, a) {
                (None, Some(Arg::Tmpl(t))) if m.path_like => {
                    t.split_whitespace().last().filter(|p| p.starts_with('/')).map(String::from)
                }
                (k, _) => k,
            };
            if m.path_like && !key.as_deref().is_some_and(is_path_like) {
                continue;
            }
            if m.kind == Kind::Dispatch && call.callee == "getattr" {
                // getattr(obj, "literal") is a static attribute read
                if call.args.get(1).and_then(|a| a.literal()).is_some() || call.args.len() < 2 {
                    continue;
                }
            }
            let seg = last_segment(&call.callee).to_ascii_lowercase();
            let method = m.method.clone().or_else(|| {
                if matches!(m.kind, Kind::RouteIn | Kind::HttpOut) {
                    if HTTP_METHODS.contains(&seg.as_str()) {
                        Some(seg.to_ascii_uppercase())
                    } else {
                        // Flask methods=[..] keyword
                        call.args.iter().find_map(|a| match a {
                            Arg::Kw(k, v) if k == "methods" => match v.as_ref() {
                                Arg::Expr(e) => Some(e.trim_matches(['[', ']', '(', ')']).replace(['"', '\''], "").to_ascii_uppercase()),
                                _ => None,
                            },
                            _ => None,
                        })
                    }
                } else {
                    None
                }
            });
            let handler = m.handler.as_deref().and_then(|h| handler_of(call, h));
            let prefix = m.prefix_class_decorator.as_ref().and_then(|d| {
                call.class_decorators.iter().find(|(c, _)| c == d).map(|(_, p)| p.clone().unwrap_or_default())
            });
            let orm_text = m.orm.then(|| {
                let mut t = call.callee.clone();
                for a in &call.args {
                    t.push(' ');
                    t.push_str(&format!("{a:?}"));
                }
                t
            });
            out.push(BoundarySite {
                file: file.to_string(),
                byte: call.byte,
                line: call.line,
                lang,
                kind: m.kind,
                text: call.callee.clone(),
                key: if m.orm { None } else { key },
                fragments,
                method,
                handler,
                orm: m.orm,
                orm_text,
                prefix,
            });
            break; // first matching model wins
        }
    }
    for mr in &scan.members {
        if let Some(m) = models
            .member
            .iter()
            .find(|m| m.langs.iter().any(|x| x == l) && m.text.iter().any(|p| glob(p, &mr.text)))
        {
            out.push(member_site(file, lang, m.kind, mr));
        }
    }
    out
}

fn member_site(file: &str, lang: Lang, kind: Kind, mr: &MemberRead) -> BoundarySite {
    BoundarySite {
        file: file.to_string(),
        byte: mr.byte,
        line: mr.line,
        lang,
        kind,
        text: mr.text.clone(),
        key: mr.key.clone(),
        fragments: None,
        method: None,
        handler: None,
        orm: false,
        orm_text: None,
        prefix: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::scan::scan_file;

    #[test]
    fn glob_matches() {
        assert!(glob("*.execute", "self.db.execute"));
        assert!(!glob("*.execute", "execute"));
        assert!(glob("requests.*", "requests.get"));
        assert!(glob("*prisma.*.find*", "this.prisma.user.findMany"));
        assert!(glob("fetch", "fetch"));
    }

    #[test]
    fn python_sites() {
        let src = r#"
import os
@app.route("/u/<id>", methods=["POST"])
def create(id):
    db.execute("INSERT INTO users (id) VALUES (?)", (id,))
    requests.post(f"https://api.x.com/{id}")
    subprocess.run(["ls"])
    return os.environ["HOME"], getattr(mod, name)()
"#;
        let (lang, scan) = scan_file("a.py", src).unwrap();
        let models = Models::load(None).unwrap();
        let sites = match_file(&models, "a.py", lang, &scan);
        let kinds: Vec<_> = sites.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, ["route_in", "sql", "http_out", "subprocess", "dispatch", "env"]);
        assert_eq!(sites[0].handler.as_deref(), Some("create"));
        assert_eq!(sites[0].method.as_deref(), Some("POST"));
        assert!(sites[2].key.is_none() && sites[2].fragments.is_some());
        assert_eq!(sites[5].key.as_deref(), Some("HOME"));
    }

    #[test]
    fn go_ts_rust_sites() {
        let models = Models::load(None).unwrap();
        let go = "package m\nfunc f() { r.GET(\"/articles/:slug\", ArticleRetrieve); db.Where(&ArticleModel{}).First(&a); os.Getenv(\"X\") }\n";
        let (lang, scan) = scan_file("m.go", go).unwrap();
        let s = match_file(&models, "m.go", lang, &scan);
        let k: Vec<_> = s.iter().map(|s| (s.kind.as_str(), s.handler.clone())).collect();
        assert!(k.contains(&("route_in", Some("ArticleRetrieve".into()))));
        assert!(s.iter().any(|s| s.kind == Kind::Sql && s.orm));
        let rs = "fn app() { App::new().route(\"/health\", web::get().to(health_check)); }\n";
        let (lang, scan) = scan_file("m.rs", rs).unwrap();
        let s = match_file(&models, "m.rs", lang, &scan);
        assert_eq!(s[0].handler.as_deref(), Some("health_check"));
        let ts = "router.get('/user', auth.required, async (req, res) => {}); router.post('/users/login', login); prisma.user.findUnique({})";
        let (lang, scan) = scan_file("r.ts", ts).unwrap();
        let s = match_file(&models, "r.ts", lang, &scan);
        assert!(s.iter().any(|s| s.kind == Kind::RouteIn && s.handler.as_deref() == Some("login")));
        assert!(s.iter().any(|s| s.kind == Kind::Sql && s.orm));
    }
}
