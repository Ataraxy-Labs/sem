//! Minimal path/name globs: `*` matches within one `/`-separated segment,
//! `**` matches any number of segments. Enough for workspace patterns
//! (`pkg/@*/*`), package selectors (`@scope/*`) and law selectors.

pub fn matches(pattern: &str, text: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let t: Vec<&str> = text.split('/').collect();
    match_segments(&p, &t)
}

fn match_segments(p: &[&str], t: &[&str]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some(&"**") => (0..=t.len()).any(|i| match_segments(&p[1..], &t[i..])),
        Some(seg) => !t.is_empty() && match_segment(seg.as_bytes(), t[0].as_bytes()) && match_segments(&p[1..], &t[1..]),
    }
}

fn match_segment(p: &[u8], t: &[u8]) -> bool {
    // iterative wildcard match with backtracking on the last `*`
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || p[pi] == b'?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn segment_and_deep_globs() {
        assert!(matches("pkg/@*/*", "pkg/@ui/model"));
        assert!(!matches("pkg/@*/*", "pkg/@ui/model/extra"));
        assert!(matches("pkg/**", "pkg/@ui/model/src/a.ts"));
        assert!(matches("@app-*/*", "@app-actors/core"));
        assert!(matches("**/*.test.ts", "pkg/a/b.test.ts"));
        assert!(matches("@ui/model", "@ui/model"));
        assert!(!matches("@ui/model", "@ui/modelx"));
    }
}
