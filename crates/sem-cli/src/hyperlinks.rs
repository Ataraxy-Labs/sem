//! Optional OSC8 terminal hyperlinks for entity references.
//!
//! When enabled, entity names in the terminal output are wrapped in OSC8
//! hyperlink escape sequences pointing at `file:line`, so a supporting terminal
//! (kitty, WezTerm, iTerm2, Ghostty, ...) renders them clickable and can open
//! the definition in your editor.
//!
//! Off by default. Enable by setting `SEM_HYPERLINK` to an editor preset
//! (`vscode`, `cursor`, `windsurf`, `zed`, `idea`, `file`) or a raw URI template
//! using `{file}` and `{line}` placeholders, e.g.
//! `SEM_HYPERLINK="vscode://file/{file}:{line}"`.
//!
//! Strictly gated: links are emitted only when stdout is a TTY, so pipes, JSON
//! output, and MCP/agent sessions never see escape codes. Force off with
//! `SEM_NO_HYPERLINKS=1`.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Resolved link template (`None` = hyperlinks disabled). Computed once: honors
/// `SEM_NO_HYPERLINKS`, requires a TTY on stdout, and requires `SEM_HYPERLINK`.
static LINK_TEMPLATE: OnceLock<Option<String>> = OnceLock::new();

fn resolve() -> Option<&'static String> {
    LINK_TEMPLATE
        .get_or_init(|| {
            if std::env::var("SEM_NO_HYPERLINKS").is_ok_and(|v| !v.is_empty() && v != "0") {
                return None;
            }
            // Escape codes would corrupt anything that isn't a live terminal
            // (pipes, files, JSON consumers, agent/MCP sessions).
            if !std::io::stdout().is_terminal() {
                return None;
            }
            let raw = std::env::var("SEM_HYPERLINK").ok()?;
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            Some(expand_preset(raw))
        })
        .as_ref()
}

/// Expand a known editor preset to a URI template, or pass through a raw
/// template unchanged.
fn expand_preset(raw: &str) -> String {
    match raw {
        "file" => "file://{file}".to_string(),
        "vscode" => "vscode://file/{file}:{line}".to_string(),
        "vscode-insiders" => "vscode-insiders://file/{file}:{line}".to_string(),
        "cursor" => "cursor://file/{file}:{line}".to_string(),
        "windsurf" => "windsurf://file/{file}:{line}".to_string(),
        "zed" => "zed://file/{file}:{line}".to_string(),
        "idea" | "jetbrains" => "idea://open?file={file}&line={line}".to_string(),
        other => other.to_string(),
    }
}

/// Whether hyperlinks are active for this run.
pub fn enabled() -> bool {
    resolve().is_some()
}

/// Best-effort absolute path (no filesystem IO, so it stays cheap per entity).
fn absolutize(file_path: &str) -> String {
    let p = std::path::Path::new(file_path);
    if p.is_absolute() {
        return file_path.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p).to_string_lossy().into_owned(),
        Err(_) => file_path.to_string(),
    }
}

fn render_uri(template: &str, file_abs: &str, line: usize) -> String {
    template
        .replace("{file}", file_abs)
        .replace("{line}", &line.to_string())
}

fn osc8(text: &str, uri: &str) -> String {
    // OSC 8 ; params ; URI ST  <text>  OSC 8 ; ; ST
    format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Wrap `text` in an OSC8 hyperlink to `file:line` when hyperlinks are enabled;
/// otherwise return `text` unchanged. `text` may already contain ANSI color
/// codes (OSC8 nests cleanly with them).
pub fn link(text: &str, file_path: &str, line: usize) -> String {
    match resolve() {
        Some(template) => {
            let abs = absolutize(file_path);
            osc8(text, &render_uri(template, &abs, line))
        }
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_expand() {
        assert_eq!(expand_preset("vscode"), "vscode://file/{file}:{line}");
        assert_eq!(expand_preset("file"), "file://{file}");
        assert_eq!(expand_preset("idea"), "idea://open?file={file}&line={line}");
        // Raw templates pass through untouched.
        assert_eq!(expand_preset("my://{file}#{line}"), "my://{file}#{line}");
    }

    #[test]
    fn uri_substitutes_placeholders() {
        assert_eq!(
            render_uri("vscode://file/{file}:{line}", "/abs/foo.rs", 42),
            "vscode://file//abs/foo.rs:42"
        );
    }

    #[test]
    fn osc8_wraps_text_and_uri() {
        assert_eq!(
            osc8("drain", "file:///abs/foo.rs"),
            "\x1b]8;;file:///abs/foo.rs\x1b\\drain\x1b]8;;\x1b\\"
        );
    }
}
