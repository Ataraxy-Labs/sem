use std::path::{Path, PathBuf};

use sem_core::parser::plugins::code::top_level_imports;

pub struct ImportsOptions {
    pub cwd: String,
    pub path: String,
    pub json: bool,
}

/// `sem imports <file> [--json]`: the file's top-level import statements, as
/// the tree-sitter parser sees them (kind, 1-based line span, byte span, and
/// the statement's own source text). Internal helper for tooling that needs
/// import POSITIONS rather than a text scan -- notably `pi`'s `sem.addImport`,
/// which uses it to supersede/place imports without ever touching
/// import-shaped text inside a string, a comment, or a nested block. A file
/// this build cannot parse, or a language with no import kind registered,
/// exits unsuccessfully rather than masquerading as a valid empty import list.
pub fn imports_command(opts: ImportsOptions) {
    let root = Path::new(&opts.cwd);
    let candidate = Path::new(&opts.path);
    let full: PathBuf = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };

    let content = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", full.display());
            std::process::exit(2);
        }
    };

    let Some(imports) = top_level_imports(&full.to_string_lossy(), &content) else {
        eprintln!("error: imports unavailable: unsupported language or invalid syntax");
        std::process::exit(2);
    };

    if opts.json {
        match serde_json::to_string(&imports) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        }
    } else {
        for imp in &imports {
            println!(
                "{}:{} {} {}",
                imp.start_line,
                imp.end_line,
                imp.kind,
                imp.text.lines().next().unwrap_or("")
            );
        }
    }
}
