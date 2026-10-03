//! Database objects from SQL text: schemas (migrations, dumps) and the
//! tables a query touches.
//!
//! Deliberately a tolerant scanner, not a parser: statements are split on
//! `;` outside quotes and dollar-quoted bodies, and each is matched against
//! the few DDL shapes that create named objects. A query's tables are the
//! names after `FROM`/`JOIN`/`INTO`/`UPDATE`; a function or procedure it
//! calls is any known routine name followed by `(`.

use std::collections::BTreeMap;

use regex::Regex;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Table,
    View,
    Function,
    Procedure,
    Trigger,
}

/// A database object with the objects its body reads or writes.
#[derive(Clone, Debug, Serialize)]
pub struct DbObject {
    pub kind: DbKind,
    pub name: String,
    /// File that (last) defined it.
    pub file: String,
    /// Tables/views its body touches (views, routines, triggers).
    pub touches: Vec<String>,
    /// For triggers: the table it fires on, and the routine it executes.
    pub on_table: Option<String>,
    pub executes: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Schema {
    pub objects: BTreeMap<String, DbObject>,
}

impl Schema {
    pub fn has_relation(&self, name: &str) -> bool {
        self.objects
            .get(&norm(name))
            .is_some_and(|o| matches!(o.kind, DbKind::Table | DbKind::View))
    }
    pub fn routine(&self, name: &str) -> Option<&DbObject> {
        self.objects
            .get(&norm(name))
            .filter(|o| matches!(o.kind, DbKind::Function | DbKind::Procedure))
    }
    /// Triggers that fire on `table`.
    pub fn triggers_on<'a>(&'a self, table: &'a str) -> impl Iterator<Item = &'a DbObject> + 'a {
        let t = norm(table);
        self.objects
            .values()
            .filter(move |o| o.kind == DbKind::Trigger && o.on_table.as_deref() == Some(t.as_str()))
    }
    pub fn add_table(&mut self, name: &str, file: &str) {
        let n = norm(name);
        if n.is_empty() {
            return;
        }
        self.objects.entry(n.clone()).or_insert(DbObject {
            kind: DbKind::Table,
            name: n,
            file: file.to_string(),
            touches: Vec::new(),
            on_table: None,
            executes: None,
        });
    }
}

/// Lowercase, unquoted, schema-stripped identifier.
pub fn norm(name: &str) -> String {
    let n = name
        .trim()
        .trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']' || c == '\'');
    let n = n.rsplit('.').next().unwrap_or(n);
    n.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
        .to_ascii_lowercase()
}

/// Split SQL into statements on `;` outside quotes and `$tag$` bodies.
pub fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut dollar: Option<String> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(tag) = &dollar {
            if sql[i..].starts_with(tag.as_str()) {
                cur.push_str(tag);
                i += tag.len();
                dollar = None;
                continue;
            }
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if c == b'\'' || c == b'"' || c == b'`' {
            quote = Some(c);
        } else if c == b'$' {
            if let Some(end) = sql[i + 1..].find('$') {
                let tag = &sql[i..i + end + 2];
                if tag[1..tag.len() - 1].chars().all(|c| c.is_alphanumeric() || c == '_') {
                    cur.push_str(tag);
                    i += tag.len();
                    dollar = Some(tag.to_string());
                    continue;
                }
            }
        } else if c == b';' {
            // MySQL/SQLite trigger bodies: BEGIN ... END;
            let upper = cur.to_ascii_uppercase();
            let opens = upper.matches("BEGIN").count();
            let closes = upper.matches("END").count();
            if !(upper.contains("CREATE") && upper.contains("TRIGGER") && opens > closes) {
                out.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
        }
        // push the whole UTF-8 char
        let ch_len = utf8_len(c);
        cur.push_str(&sql[i..(i + ch_len).min(sql.len())]);
        i += ch_len;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

const IDENT: &str = r#"((?:[A-Za-z_][\w$]*|"[^"]+"|`[^`]+`|\[[^\]]+\])(?:\.(?:[A-Za-z_][\w$]*|"[^"]+"|`[^`]+`))?)"#;

struct Res {
    create: Regex,
    trigger: Regex,
    rename: Regex,
    drop: Regex,
    refs: Regex,
    call: Regex,
    is_sql: Regex,
}

fn res() -> &'static Res {
    static R: std::sync::OnceLock<Res> = std::sync::OnceLock::new();
    R.get_or_init(|| Res {
        create: Regex::new(&format!(
            r"(?is)^\s*CREATE\s+(?:OR\s+REPLACE\s+)?(?:(?:GLOBAL|LOCAL)\s+)?(?:TEMP(?:ORARY)?\s+|UNLOGGED\s+|MATERIALIZED\s+|DEFINER\s*=\s*\S+\s+)*(TABLE|VIEW|FUNCTION|PROCEDURE)\s+(?:IF\s+NOT\s+EXISTS\s+)?{IDENT}"
        ))
        .unwrap(),
        trigger: Regex::new(&format!(
            r"(?is)^\s*CREATE\s+(?:OR\s+REPLACE\s+)?(?:CONSTRAINT\s+)?(?:DEFINER\s*=\s*\S+\s+)?TRIGGER\s+(?:IF\s+NOT\s+EXISTS\s+)?{IDENT}.*?\bON\s+{IDENT}(?:.*?\bEXECUTE\s+(?:FUNCTION|PROCEDURE)\s+{IDENT})?"
        ))
        .unwrap(),
        rename: Regex::new(&format!(r"(?is)^\s*ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?{IDENT}\s+RENAME\s+TO\s+{IDENT}")).unwrap(),
        drop: Regex::new(&format!(r"(?is)^\s*DROP\s+(TABLE|VIEW|FUNCTION|PROCEDURE|TRIGGER)\s+(?:IF\s+EXISTS\s+)?{IDENT}")).unwrap(),
        refs: Regex::new(&format!(r"(?i)\b(?:FROM|JOIN|INTO|UPDATE|TABLE)\s+(?:ONLY\s+|IF\s+(?:NOT\s+)?EXISTS\s+)?{IDENT}")).unwrap(),
        call: Regex::new(r"(?i)\b([A-Za-z_][\w$]*)\s*\(").unwrap(),
        is_sql: Regex::new(
            r"(?is)^\s*(?:\(\s*)?(?:SELECT\b.+\bFROM\b|SELECT\s+[\w$]+\s*\(|INSERT\s+(?:OR\s+\w+\s+)?INTO\b|UPDATE\s+\S+\s+SET\b|DELETE\s+FROM\b|WITH\s+\w+(?:\s*\([^)]*\))?\s+AS\s*\(|CREATE\s+(?:UNIQUE\s+)?(?:TABLE|VIEW|INDEX|TRIGGER|FUNCTION)\b|ALTER\s+TABLE\b|DROP\s+(?:TABLE|VIEW|INDEX)\b|CALL\s+\w+|REPLACE\s+INTO\b|PRAGMA\s+\w+)",
        )
        .unwrap(),
    })
}

/// Does this string literal look like SQL?
pub fn is_sql(s: &str) -> bool {
    if !res().is_sql.is_match(s) {
        return false;
    }
    // English prose also starts "Select ... from ..." / "Update ... set":
    // demand an upper-case keyword or a token prose rarely has.
    let head: String = s.trim_start().chars().take(6).collect();
    let upper_kw = head.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_ascii_uppercase());
    let lower = s.to_ascii_lowercase();
    upper_kw
        || s.contains(['*', '=', '?', '$', '(', ','])
        || [" where ", " join ", " values", " order by ", " limit "]
            .iter()
            .any(|k| lower.contains(k))
}

const NOT_TABLES: &[&str] = &[
    "select", "where", "set", "values", "lateral", "unnest", "dual", "only", "json_each",
    "json_tree", "generate_series", "pragma_table_info", "sqlite_master", "sqlite_schema",
];

/// Tables (and views) a SQL text touches, normalized; placeholders excluded.
pub fn table_refs(sql: &str) -> Vec<String> {
    // MySQL upserts: `ON DUPLICATE KEY UPDATE col = ...` names a column
    let upper = sql.to_ascii_uppercase();
    let sql = match upper.find("ON DUPLICATE KEY") {
        Some(i) => &sql[..i],
        None => sql,
    };
    let mut out: Vec<String> = res()
        .refs
        .captures_iter(sql)
        .map(|c| norm(&c[1]))
        .filter(|n| !n.is_empty() && !NOT_TABLES.contains(&n.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Routine names called in a SQL text that `schema` knows.
pub fn routine_calls(sql: &str, schema: &Schema) -> Vec<String> {
    let mut out: Vec<String> = res()
        .call
        .captures_iter(sql)
        .map(|c| norm(&c[1]))
        .filter(|n| schema.routine(n).is_some())
        .collect();
    out.sort();
    out.dedup();
    out
}

impl Schema {
    /// Apply one SQL file (migration or dump), in order.
    pub fn apply_sql(&mut self, file: &str, sql: &str) {
        for stmt in split_statements(sql) {
            let r = res();
            if let Some(c) = r.trigger.captures(&stmt) {
                let name = norm(&c[1]);
                let on = norm(&c[2]);
                let exec = c.get(3).map(|m| norm(m.as_str()));
                let body_touches = match stmt.to_ascii_uppercase().find("BEGIN") {
                    Some(i) => table_refs(&stmt[i..]),
                    None => Vec::new(),
                };
                self.objects.insert(
                    name.clone(),
                    DbObject {
                        kind: DbKind::Trigger,
                        name,
                        file: file.into(),
                        touches: body_touches,
                        on_table: Some(on),
                        executes: exec,
                    },
                );
            } else if let Some(c) = r.create.captures(&stmt) {
                let kind = match c[1].to_ascii_uppercase().as_str() {
                    "TABLE" => DbKind::Table,
                    "VIEW" => DbKind::View,
                    "FUNCTION" => DbKind::Function,
                    _ => DbKind::Procedure,
                };
                let name = norm(&c[2]);
                let body = &stmt[c.get(0).unwrap().end()..];
                let touches = if kind == DbKind::Table {
                    Vec::new()
                } else {
                    table_refs(body).into_iter().filter(|t| *t != name).collect()
                };
                self.objects.insert(
                    name.clone(),
                    DbObject { kind, name, file: file.into(), touches, on_table: None, executes: None },
                );
            } else if let Some(c) = r.rename.captures(&stmt) {
                let (from, to) = (norm(&c[1]), norm(&c[2]));
                if let Some(mut o) = self.objects.remove(&from) {
                    o.name = to.clone();
                    self.objects.insert(to, o);
                }
            } else if let Some(c) = r.drop.captures(&stmt) {
                self.objects.remove(&norm(&c[2]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_objects_and_triggers() {
        let mut s = Schema::default();
        s.apply_sql(
            "m1.sql",
            r#"CREATE TABLE IF NOT EXISTS "public"."users" (id int);
               CREATE TABLE audit(x text);
               CREATE OR REPLACE FUNCTION log_user() RETURNS trigger AS $$
               BEGIN INSERT INTO audit VALUES (NEW.id); RETURN NEW; END; $$ LANGUAGE plpgsql;
               CREATE TRIGGER t_users AFTER INSERT ON users FOR EACH ROW EXECUTE FUNCTION log_user();
               CREATE TABLE tmp(a int); DROP TABLE tmp;
               ALTER TABLE audit RENAME TO audit_log;"#,
        );
        assert!(s.has_relation("users"));
        assert!(s.has_relation("AUDIT_LOG"));
        assert!(!s.has_relation("tmp"));
        assert_eq!(s.routine("log_user").unwrap().touches, ["audit"]);
        let t: Vec<_> = s.triggers_on("users").map(|t| t.executes.clone()).collect();
        assert_eq!(t, [Some("log_user".to_string())]);
    }

    #[test]
    fn sqlite_trigger_body_is_one_statement() {
        let mut s = Schema::default();
        s.apply_sql(
            "d.sql",
            "CREATE TABLE a(x); CREATE TABLE b(y);\nCREATE TRIGGER tr AFTER INSERT ON a BEGIN INSERT INTO b VALUES (1); UPDATE b SET y=2; END;",
        );
        let tr = &s.objects["tr"];
        assert_eq!(tr.on_table.as_deref(), Some("a"));
        assert_eq!(tr.touches, ["b"]);
    }

    #[test]
    fn detects_sql_and_refs() {
        assert!(is_sql("select id from users where x = ?"));
        assert!(is_sql("INSERT INTO t (a) VALUES ($1)"));
        assert!(!is_sql("Select your file from the list"));
        assert!(!is_sql("update the docs"));
        assert_eq!(
            table_refs("SELECT * FROM a JOIN \"b\" ON 1 WHERE x IN (SELECT y FROM c)"),
            ["a", "b", "c"]
        );
        assert_eq!(table_refs("INSERT INTO hours (a) VALUES (1) ON DUPLICATE KEY UPDATE availability = 2"), ["hours"]);
    }
}
