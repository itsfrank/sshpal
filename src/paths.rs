use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    Push,
    Pull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub direction: SyncDirection,
    pub relative_path: PathBuf,
    pub local_path: PathBuf,
    pub remote_path: PathBuf,
    pub source_is_dir: bool,
}

pub fn relative_cwd(local_root: &Path, cwd: &Path) -> Result<PathBuf> {
    let local_root = local_root.canonicalize()?;
    let cwd = cwd.canonicalize()?;
    let rel = cwd.strip_prefix(&local_root).map_err(|_| {
        anyhow::anyhow!(
            "cwd {} is not under {}",
            cwd.display(),
            local_root.display()
        )
    })?;
    Ok(rel.to_path_buf())
}

pub fn resolve_relative_target(cwd_rel: &Path, arg: &Path) -> Result<PathBuf> {
    if arg.is_absolute() {
        bail!("absolute paths are not allowed: {}", arg.display());
    }

    let mut parts: Vec<PathBuf> = Vec::new();
    for base in [cwd_rel, arg] {
        for component in base.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(segment) => parts.push(PathBuf::from(segment)),
                Component::ParentDir => {
                    if parts.pop().is_none() {
                        bail!("path escapes project root");
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    bail!("unsupported path component in {}", arg.display());
                }
            }
        }
    }
    Ok(parts.into_iter().collect())
}

pub fn build_sync_plan(
    local_root: &Path,
    remote_root: &Path,
    cwd_rel: &Path,
    arg: &Path,
    direction: SyncDirection,
) -> Result<SyncPlan> {
    let relative_path = resolve_relative_target(cwd_rel, arg)?;
    let local_path = local_root.join(&relative_path);
    Ok(SyncPlan {
        direction,
        local_path: local_path.clone(),
        remote_path: remote_root.join(&relative_path),
        relative_path,
        source_is_dir: local_path.is_dir(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn resolves_nested_dot() {
        let rel = resolve_relative_target(Path::new("foo/bar"), Path::new(".")).unwrap();
        assert_eq!(rel, PathBuf::from("foo/bar"));
    }

    #[test]
    fn rejects_escape() {
        let err = resolve_relative_target(Path::new("foo"), Path::new("../../etc")).unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn rejects_absolute() {
        assert!(resolve_relative_target(Path::new("foo"), Path::new("/tmp")).is_err());
    }

    #[test]
    fn computes_relative_cwd() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        let rel = relative_cwd(&root, &sub).unwrap();
        assert_eq!(rel, PathBuf::from("a/b"));
    }

    #[test]
    fn builds_sync_plan() {
        let plan = build_sync_plan(
            Path::new("/local"),
            Path::new("/remote"),
            Path::new("foo"),
            Path::new("bar"),
            SyncDirection::Push,
        )
        .unwrap();
        assert_eq!(plan.local_path, PathBuf::from("/local/foo/bar"));
        assert_eq!(plan.remote_path, PathBuf::from("/remote/foo/bar"));
        assert!(!plan.source_is_dir);
    }

    #[test]
    fn marks_existing_directory_as_dir_source() {
        let dir = tempdir().unwrap();
        let local_root = dir.path().join("local");
        let remote_root = dir.path().join("remote");
        fs::create_dir_all(local_root.join("foo/bar")).unwrap();

        let plan = build_sync_plan(
            &local_root,
            &remote_root,
            Path::new("foo"),
            Path::new("bar"),
            SyncDirection::Push,
        )
        .unwrap();

        assert!(plan.source_is_dir);
    }
}
