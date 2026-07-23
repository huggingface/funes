//! Small shared helpers for installing and removing agent integrations.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

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

/// What happened when an agent CLI was asked to remove one funes registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoveCommand {
    /// The command succeeded, including CLIs that report an already-absent registration as success.
    Removed,
    /// The command reported the registration was already absent.
    Absent,
    /// The agent CLI is not installed or not on `PATH`.
    MissingCli,
}

/// Run an agent CLI's remove command. Removal is idempotent: a non-zero status whose output
/// contains one of `absent_markers` is treated as already removed. Other failures retain their
/// diagnostics and fail rather than claiming a partial uninstall succeeded.
pub(crate) fn run_remove(program: &str, args: &[&str], absent_markers: &[&str]) -> Result<RemoveCommand> {
    let command = shell_command(program, args);
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RemoveCommand::MissingCli),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("running `{command}`"))),
    };
    if output.status.success() {
        return Ok(RemoveCommand::Removed);
    }

    let detail = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if absent_markers.iter().any(|marker| detail.contains(marker)) {
        return Ok(RemoveCommand::Absent);
    }
    let detail = detail.trim();
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    bail!("`{command}` failed (exit {:?}){suffix}", output.status.code())
}

/// Remove one exact funes-owned file. Missing is already removed.
pub(crate) fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("removing {}", path.display()))),
    }
}

/// Remove one exact funes-owned tree without following a symlink at the tree root.
pub(crate) fn remove_tree(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => remove_file(path),
        Ok(_) => std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("inspecting {}", path.display()))),
    }
}

/// Prune an integration parent only when it is now empty.
pub(crate) fn remove_empty_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!("removing empty {}", path.display()))),
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
