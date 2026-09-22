//! `funes add <agent>` / `funes remove <agent>`: the registry ([`registry`]) that resolves an agent
//! id to an installed integration and runs it, plus the two helpers it needs. Every agent is an
//! integration script; funes's knowledge of one is the lookup.

pub mod registry;

use anyhow::{Context, Result};
use std::path::Path;

/// Render an argv as a copy/paste-safe POSIX shell command.
pub(crate) fn shell_command<S: AsRef<str>>(program: &str, args: &[S]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(AsRef::as_ref))
        .map(shell_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

/// Remove one exact funes-owned tree without following a symlink at the tree root.
pub(crate) fn remove_tree(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::Error::new(e).context(format!("removing {}", path.display()))),
        },
        Ok(_) => std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("inspecting {}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::shell_command;

    #[test]
    fn shell_command_quotes_only_unsafe_arguments() {
        assert_eq!(
            shell_command(
                "codex",
                &["mcp", "add", "funes", "--", "/Applications/Funes Bin/funes", "mcp"]
            ),
            "codex mcp add funes -- '/Applications/Funes Bin/funes' mcp"
        );
        assert_eq!(
            shell_command("pi", &["remove", "/Users/O'Brien/funes"]),
            "pi remove '/Users/O'\"'\"'Brien/funes'"
        );
        assert_eq!(shell_command("agent", &[""]), "agent ''");
    }
}
