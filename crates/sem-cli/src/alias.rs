//! Old command names kept as hidden aliases.
//!
//! Every renamed command still parses and behaves exactly as before. A person
//! at a terminal gets one line on stderr naming the new spelling; scripts and
//! agents get nothing: no notice in JSON mode, never on stdout, and never
//! when stdout is not a terminal.

use std::io::IsTerminal;

/// Whether the notice is printed: stdout is a terminal and the output is not
/// JSON. `SEM_NO_DEPRECATION` silences it everywhere.
pub fn should_note(stdout_is_terminal: bool, json: bool) -> bool {
    stdout_is_terminal && !json && std::env::var_os("SEM_NO_DEPRECATION").is_none()
}

/// The one-line notice for `sem <old>`, now `sem <new>`.
pub fn notice(old: &str, new: &str) -> String {
    format!("note: `sem {old}` is now `sem {new}` (the old name keeps working)")
}

/// Prints [`notice`] to stderr when [`should_note`] allows it.
pub fn note(old: &str, new: &str, json: bool) {
    if should_note(std::io::stdout().is_terminal(), json) {
        eprintln!("{}", notice(old, new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_only_for_a_terminal_and_never_in_json_mode() {
        if std::env::var_os("SEM_NO_DEPRECATION").is_some() {
            return;
        }
        assert!(should_note(true, false));
        assert!(!should_note(true, true));
        assert!(!should_note(false, false));
        assert!(!should_note(false, true));
    }

    #[test]
    fn notice_names_both_spellings_on_one_line() {
        let n = notice("callers", "find --callers");
        assert_eq!(
            n,
            "note: `sem callers` is now `sem find --callers` (the old name keeps working)"
        );
        assert!(!n.contains('\n'));
    }
}
