//! `ENTITYOWNERS`: who must approve a change, decided per entity instead of
//! per file.
//!
//! CODEOWNERS answers "who owns this path", which is the wrong question when
//! the thing that matters is one function: a change to `verify_token` needs
//! security review wherever the file lives, and a change to the rest of that
//! file does not. `sem certify` reads an optional `ENTITYOWNERS` file at the
//! root of the head tree and reports, for every changed entity, the owners
//! whose rules it falls under.
//!
//! The grammar is deliberately small. One rule per line, `#` starts a comment:
//!
//! ```text
//! verify_token          @security          # an entity name
//! Session::*            @auth              # a qualified name; `Session.*` also works
//! src/billing/**        @payments          # a path glob: every entity in those files
//! src/api/*.py:handle_* @api               # path glob, then a name glob
//! verify_token +callers @security          # also anything that calls it
//! ```
//!
//! - A pattern containing `/` (and no `:`) is a path glob; every changed
//!   entity in a matching file falls under it.
//! - `path:name` restricts a name glob to matching files.
//! - Anything else is a name glob, matched against the entity's name and its
//!   qualified name (`Parent::name` and `Parent.name`).
//! - `*` matches within one path segment, `**` across segments, `?` one char.
//! - `+callers` (after the pattern) extends the rule to every changed entity
//!   that statically calls an entity the pattern matches at head. Static
//!   only, like the rest of the certificate: a caller the resolver cannot pin
//!   is not reported.
//!
//! Every matching rule applies (owners accumulate); there is no
//! last-match-wins, because missing a required reviewer is the costly error.

/// One parsed `ENTITYOWNERS` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rule {
    /// The pattern as written, for reporting which rule fired.
    pub pattern: String,
    path: Option<String>,
    name: Option<String>,
    /// `+callers`: also changed entities that call a match.
    pub callers: bool,
    pub owners: Vec<String>,
    pub line: usize,
}

/// Parse an `ENTITYOWNERS` file. Lines that do not parse (a pattern with no
/// owner) are skipped and returned as warnings rather than failing the
/// certificate.
pub(crate) fn parse(text: &str) -> (Vec<Rule>, Vec<String>) {
    let mut rules = Vec::new();
    let mut warnings = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let pattern = words.next().unwrap_or_default().to_string();
        let mut callers = false;
        let mut owners = Vec::new();
        for w in words {
            if w == "+callers" {
                callers = true;
            } else {
                owners.push(w.to_string());
            }
        }
        if owners.is_empty() {
            warnings.push(format!("ENTITYOWNERS:{}: `{}` names no owner, ignored", i + 1, pattern));
            continue;
        }
        let (path, name) = match pattern.split_once(':') {
            // `Parent::name` is a qualified name, not `path:name`.
            Some((p, n)) if !pattern.contains("::") => (Some(p.to_string()), Some(n.to_string())),
            _ if pattern.contains('/') => (Some(pattern.clone()), None),
            _ => (None, Some(pattern.clone())),
        };
        rules.push(Rule { pattern, path, name, callers, owners, line: i + 1 });
    }
    (rules, warnings)
}

impl Rule {
    /// Does this rule's pattern match an entity (by name, optional parent
    /// name and file)?
    pub(crate) fn matches(&self, name: &str, parent: Option<&str>, file: &str) -> bool {
        if let Some(p) = &self.path {
            if !glob(p, file) {
                return false;
            }
        }
        match &self.name {
            None => true,
            Some(n) => {
                glob(n, name)
                    || parent.is_some_and(|p| glob(n, &format!("{p}::{name}")) || glob(n, &format!("{p}.{name}")))
            }
        }
    }
}

/// Glob match: `**` any run of characters, `*` any run without `/`, `?` one
/// character. A name has no `/`, so on names `*` and `**` agree.
pub(crate) fn glob(pat: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
                (0..=s.len()).any(|i| go(rest, &s[i..])) || (0..=s.len()).any(|i| go(&p[2..], &s[i..]))
            }
            Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| go(&p[1..], &s[i..])),
            Some(b'?') => !s.is_empty() && s[0] != b'/' && go(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(pat.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_and_matching() {
        let (rules, warnings) = parse(
            "# owners\nverify_token @security\nSession::* @auth\nsrc/billing/** @payments\n\
             src/api/*.py:handle_* @api\nrefund +callers @payments @finance\nlonely\n",
        );
        assert_eq!(rules.len(), 5);
        assert_eq!(warnings.len(), 1, "a pattern with no owner is reported, not applied");
        let m = |i: usize, n: &str, p: Option<&str>, f: &str| rules[i].matches(n, p, f);
        assert!(m(0, "verify_token", None, "any/where.py"));
        assert!(!m(0, "verify_tokens", None, "a.py"));
        assert!(m(1, "refresh", Some("Session"), "a.py"));
        assert!(!m(1, "refresh", Some("Cart"), "a.py"));
        assert!(m(2, "anything", None, "src/billing/stripe/charge.py"));
        assert!(!m(2, "anything", None, "src/billingx/a.py"));
        assert!(m(3, "handle_get", None, "src/api/users.py"));
        assert!(!m(3, "handle_get", None, "src/api/v2/users.py"), "`*` stays within one segment");
        assert!(rules[4].callers);
        assert_eq!(rules[4].owners, vec!["@payments", "@finance"]);
    }
}
