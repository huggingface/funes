//! `funes add <agent>` / `funes remove <agent>`: the registry ([`registry`]) that resolves an agent
//! id to an installed integration and runs it, plus the two helpers it needs. Every agent is an
//! integration script; funes's knowledge of one is the lookup.

pub mod registry;

use anyhow::{Context, Result};
use std::path::Path;

use crate::traces::spool;

/// What a read says when this funes and an install on this machine disagree, one `note:` line per
/// case, or `None`: a hook asked for a spool nothing writes (an install from before the spool), or a
/// registered integration speaks another contract (an install by another binary). The cure is the
/// same, `funes add <id>` again — and funes knows no agent here, only what asked and what is
/// registered.
pub fn stale_install_notice() -> Option<String> {
    let mut lines: Vec<String> = spool::missing()
        .into_iter()
        .map(|id| {
            format!(
                "the {id} integration does not match this version of funes. \
                 Re-run `funes add {id}` to update it."
            )
        })
        .collect();
    if let Ok(root) = registry::default_root() {
        lines.extend(registry::mismatched(&root).into_iter().map(|(id, _)| {
            format!(
                "the {id} integration does not match this version of funes. \
                 Re-run `funes add {id}` to update it."
            )
        }));
    }
    (!lines.is_empty()).then(|| lines.iter().map(|line| format!("note: {line}\n")).collect())
}

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
