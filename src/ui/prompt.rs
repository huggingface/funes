//! Questions a command puts to the user at a terminal, answered on stdin.

use std::io::Write;

/// Print `prompt` on stderr and read one line from stdin. `None` on EOF or a read error, so an
/// unattended run never proceeds on a wedged stdin.
pub fn ask(prompt: &str) -> Option<String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(n) if n > 0 => Some(answer),
        _ => None,
    }
}

/// A yes/no question. Enter takes `default_yes`; `y`/`yes` is yes; anything else, EOF or a read
/// error is no, so an unrecognized answer never creates a repo or publishes.
pub fn confirm(prompt: &str, default_yes: bool) -> bool {
    ask(prompt).is_some_and(|answer| parse_confirm(&answer, default_yes))
}

/// The answer to [`confirm`]: empty → the default; `y`/`yes` → yes; anything else → no.
pub(crate) fn parse_confirm(input: &str, default_yes: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_confirm;

    #[test]
    fn parse_confirm_honors_default_and_answers() {
        // Empty takes the default either way.
        assert!(parse_confirm("\n", true));
        assert!(!parse_confirm("  ", false));
        // Explicit yes/no, case- and whitespace-insensitive.
        assert!(parse_confirm("y", false));
        assert!(parse_confirm(" YES \n", false));
        assert!(!parse_confirm("n", true));
        // Anything unrecognized is a conservative no, even under a yes default.
        assert!(!parse_confirm("nope", true));
        assert!(!parse_confirm("maybe", true));
    }
}
