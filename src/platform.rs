//! Host filesystem conventions shared by the storage, transport, and integration layers.

use std::path::PathBuf;

/// The user's home directory. Rust uses the Windows profile on Windows and HOME/passwd on Unix.
/// Empty environment values never turn state or credentials into paths relative to the cwd.
pub fn user_home() -> Option<PathBuf> {
    std::env::home_dir().filter(|path| !path.as_os_str().is_empty())
}

/// Recognize Windows drive and rooted paths even when a memory identifier is read on Unix.
pub fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('\\') || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// Resolve native executables and npm's Windows command shims while retaining argv escaping.
/// Rust handles an explicitly named .cmd/.bat with its batch-file escaping rules; callers never
/// concatenate arguments into `cmd /c` or bypass those rules with raw arguments.
pub fn command(program: &str) -> std::process::Command {
    #[cfg(windows)]
    if std::path::Path::new(program).components().count() == 1 {
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                for suffix in [".exe", ".cmd", ".bat"] {
                    let candidate = dir.join(format!("{program}{suffix}"));
                    if candidate.is_file() {
                        return std::process::Command::new(candidate);
                    }
                }
            }
        }
    }
    std::process::Command::new(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_paths_are_not_hub_names() {
        for path in [
            r"C:\work\memory",
            "C:/work/memory",
            "C:memory",
            r"\\server\share\记忆",
            r"\memory",
        ] {
            assert!(is_windows_path(path), "{path}");
        }
        for path in ["acme/memory", "hf://datasets/acme/memory", "/tmp/memory", "./memory"] {
            assert!(!is_windows_path(path), "{path}");
        }
    }
}
