//! Per-plugin filesystem sandbox for the fs_* host calls.
//!
//! Every plugin gets one data directory
//! (`$XDG_DATA_HOME|~/.local/share` + `tmux/plugins/<plugin>/`), created
//! on first use. Paths crossing the ABI are RELATIVE to that root:
//! absolute paths and any `..` component are rejected, and the
//! canonicalized parent must stay inside the canonicalized root (defeats
//! symlinks planted inside the data dir).

use std::path::{Component, Path, PathBuf};

fn data_home() -> Result<PathBuf, String> {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x));
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "neither XDG_DATA_HOME nor HOME is set".to_string())?;
    Ok(PathBuf::from(home).join(".local/share"))
}

/// The plugin's sandbox root, created on first use.
pub fn plugin_data_dir(plugin: &str) -> Result<PathBuf, String> {
    if plugin.is_empty()
        || plugin.contains('/')
        || plugin.contains('\\')
        || plugin == "."
        || plugin == ".."
    {
        return Err(format!("bad plugin name {plugin:?}"));
    }
    let root = data_home()?.join("tmux/plugins").join(plugin);
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("create {}: {e}", root.display()))?;
    Ok(root)
}

/// Resolve a plugin-relative path inside `root`. `create_dirs` makes the
/// intermediate directories (write path); reads fail on missing parents.
pub fn sandboxed_path(
    root: &Path,
    rel: &str,
    create_dirs: bool,
) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("empty path".into());
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err("absolute paths are not allowed".into());
    }
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return Err(format!("path escapes the sandbox: {rel:?}")),
        }
    }
    let full = root.join(rel_path);
    let parent = full
        .parent()
        .ok_or_else(|| "path has no parent".to_string())?;
    if create_dirs {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // Canonicalize the PARENT (the file itself may not exist yet) and
    // require it to stay under the canonicalized root: a symlinked
    // directory inside the data dir cannot lead outside.
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize root: {e}"))?;
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| format!("no such directory {}: {e}", parent.display()))?;
    if !canon_parent.starts_with(&canon_root) {
        return Err(format!("path escapes the sandbox: {rel:?}"));
    }
    let name = full
        .file_name()
        .ok_or_else(|| "path has no file name".to_string())?;
    Ok(canon_parent.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmproot(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pgh-fsbox-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rejects_absolute_and_dotdot() {
        let root = tmproot("rej");
        assert!(sandboxed_path(&root, "/etc/passwd", false).is_err());
        assert!(sandboxed_path(&root, "a/../../x", false).is_err());
        assert!(sandboxed_path(&root, "..", false).is_err());
        assert!(sandboxed_path(&root, "", false).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn creates_nested_dirs_and_round_trips() {
        let root = tmproot("nest");
        let p = sandboxed_path(&root, "a/b/c.txt", true).unwrap();
        std::fs::write(&p, b"hi").unwrap();
        let q = sandboxed_path(&root, "a/b/c.txt", false).unwrap();
        assert_eq!(std::fs::read(&q).unwrap(), b"hi");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn symlink_escape_is_caught() {
        let root = tmproot("sym");
        let outside = tmproot("sym-outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let err = sandboxed_path(&root, "link/x.txt", false).unwrap_err();
        assert!(err.contains("escapes"), "{err}");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn bad_plugin_names_rejected() {
        assert!(plugin_data_dir("").is_err());
        assert!(plugin_data_dir("a/b").is_err());
        assert!(plugin_data_dir("..").is_err());
    }
}
