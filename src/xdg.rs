//! Minimal XDG Base Directory resolution — only what this app needs.

pub fn data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        return std::path::PathBuf::from(dir);
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home).join(".local/share")
}

/// Per-user, 0700 dir for the throwaway KWin scripts — not the shared
/// `/tmp`, where another user could pre-plant a symlink at our predictable
/// file names.
pub fn runtime_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}
