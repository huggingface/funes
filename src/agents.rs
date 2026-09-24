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
///
/// `memory` is the one the caller serves, when it knows it: the MCP server's own, since the agent
/// launched it with the binding `funes add` recorded. A bare `funes add <id>` would bind anew, so
/// the cure names the memory when it can and asks for it when it cannot.
pub fn stale_install_notice(memory: Option<&str>) -> Option<String> {
    let line = |id: &str| {
        let cure = match memory {
            Some(memory) => format!("Re-run `funes add {id} {memory}` to update it."),
            None => format!("Re-run `funes add {id}`, naming the memory it is bound to, to update it."),
        };
        format!("the {id} integration does not match this version of funes. {cure}")
    };
    let mut lines: Vec<String> = spool::missing().iter().map(|id| line(id)).collect();
    if let Ok(root) = registry::default_root() {
        lines.extend(registry::mismatched(&root).iter().map(|(id, _)| line(id)));
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
