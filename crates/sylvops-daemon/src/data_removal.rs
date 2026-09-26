//! Explicit, containment-checked removal of SylvOps-owned user data.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::runtime::{OWNERSHIP_MARKER, RuntimePaths};

pub const DATA_REMOVAL_CONFIRMATION: &str = "DELETE SYLVOPS USER DATA";

#[derive(Clone, Debug)]
pub struct DataRemovalPlan {
    roots: Vec<PathBuf>,
    protected_paths: Vec<PathBuf>,
}

impl DataRemovalPlan {
    /// Validates a one-shot removal plan against authoritative runtime and protected-content paths.
    ///
    /// # Errors
    ///
    /// Refuses weak confirmation, broad/unowned targets, links, and any target overlapping user content.
    pub fn prepare(
        paths: &RuntimePaths,
        confirmation: &str,
        protected_paths: &[PathBuf],
    ) -> Result<Self, DataRemovalError> {
        if confirmation != DATA_REMOVAL_CONFIRMATION {
            return Err(DataRemovalError::ConfirmationRequired);
        }
        let protected_paths = protected_paths
            .iter()
            .map(|path| canonical_if_present(path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for path in [
            &paths.data_directory,
            &paths.config_directory,
            &paths.runtime_directory,
        ] {
            let canonical = validate_owned_root(path, &protected_paths)?;
            if let Some(canonical) = canonical
                && seen.insert(canonical.clone())
            {
                roots.push(canonical);
            }
        }
        roots.sort_by_key(|path| path.components().count());
        let mut collapsed: Vec<PathBuf> = Vec::new();
        for root in roots {
            if !collapsed.iter().any(|parent| root.starts_with(parent)) {
                collapsed.push(root);
            }
        }
        if collapsed.is_empty() {
            return Err(DataRemovalError::UnownedTarget);
        }
        Ok(Self {
            roots: collapsed,
            protected_paths,
        })
    }

    /// Revalidates and removes only the owned roots in this plan. Repeating a completed plan is safe.
    ///
    /// # Errors
    ///
    /// Stops before a target whose identity, marker, or containment changed.
    pub async fn execute(&self) -> Result<(), DataRemovalError> {
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            let Some(current) = validate_owned_root(root, &self.protected_paths)? else {
                continue;
            };
            if current != *root {
                return Err(DataRemovalError::IdentityChanged);
            }
            let protected = self.protected_paths.clone();
            tokio::task::spawn_blocking(move || remove_preserving_protected(&current, &protected))
                .await
                .map_err(|_| DataRemovalError::RemovalFailed)??;
        }
        Ok(())
    }
}

fn validate_owned_root(
    path: &Path,
    protected_paths: &[PathBuf],
) -> Result<Option<PathBuf>, DataRemovalError> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(DataRemovalError::UnsafeTarget);
    }
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| DataRemovalError::UnsafeTarget)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DataRemovalError::UnsafeTarget);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| DataRemovalError::UnsafeTarget)?;
    if canonical.parent().is_none() || is_home_directory(&canonical) {
        return Err(DataRemovalError::UnsafeTarget);
    }
    let marker = std::fs::read_to_string(canonical.join(OWNERSHIP_MARKER))
        .map_err(|_| DataRemovalError::UnownedTarget)?;
    if marker != sylvops_core::APPLICATION_ID {
        return Err(DataRemovalError::UnownedTarget);
    }
    if protected_paths
        .iter()
        .any(|protected| canonical.starts_with(protected))
    {
        return Err(DataRemovalError::ProtectedContent);
    }
    Ok(Some(canonical))
}

fn remove_preserving_protected(
    path: &Path,
    protected_paths: &[PathBuf],
) -> Result<(), DataRemovalError> {
    if protected_paths.iter().any(|protected| protected == path) {
        return Ok(());
    }
    let contains_protected = protected_paths
        .iter()
        .any(|protected| protected.starts_with(path));
    if !contains_protected {
        std::fs::remove_dir_all(path).map_err(|_| DataRemovalError::RemovalFailed)?;
        return Ok(());
    }
    for entry in std::fs::read_dir(path).map_err(|_| DataRemovalError::RemovalFailed)? {
        let entry = entry.map_err(|_| DataRemovalError::RemovalFailed)?;
        let child = entry.path();
        if contains_protected && entry.file_name() == OWNERSHIP_MARKER {
            continue;
        }
        if protected_paths.iter().any(|protected| protected == &child) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|_| DataRemovalError::RemovalFailed)?;
        if file_type.is_dir() && !file_type.is_symlink() {
            remove_preserving_protected(&child, protected_paths)?;
        } else {
            std::fs::remove_file(&child).map_err(|_| DataRemovalError::RemovalFailed)?;
        }
    }
    Ok(())
}

fn canonical_if_present(path: &Path) -> Result<PathBuf, DataRemovalError> {
    if path.exists() {
        std::fs::canonicalize(path).map_err(|_| DataRemovalError::ProtectedContent)
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(DataRemovalError::ProtectedContent)
    }
}

fn is_home_directory(path: &Path) -> bool {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .and_then(|home| std::fs::canonicalize(home).ok())
        .is_some_and(|home| home == path)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DataRemovalError {
    #[error("exact user-data removal confirmation is required")]
    ConfirmationRequired,
    #[error("user-data removal target is unsafe")]
    UnsafeTarget,
    #[error("user-data removal target is not SylvOps-owned")]
    UnownedTarget,
    #[error("user-data removal would overlap a repository or worktree")]
    ProtectedContent,
    #[error("user-data removal target changed after validation")]
    IdentityChanged,
    #[error("user-data removal did not complete")]
    RemovalFailed,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::runtime::RuntimePaths;

    use super::{DATA_REMOVAL_CONFIRMATION, DataRemovalPlan};

    #[tokio::test]
    async fn removal_requires_strong_confirmation_and_preserves_user_content() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let repository = temporary.path().join("repository");
        let worktree = temporary.path().join("worktree");
        tokio::fs::create_dir_all(repository.join(".git"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(&worktree).await.unwrap();
        tokio::fs::write(worktree.join("keep.txt"), b"keep")
            .await
            .unwrap();

        assert!(
            DataRemovalPlan::prepare(&paths, "wrong", &[repository.clone(), worktree.clone()])
                .is_err()
        );
        let plan = DataRemovalPlan::prepare(
            &paths,
            DATA_REMOVAL_CONFIRMATION,
            &[repository.clone(), worktree.clone()],
        )
        .unwrap();
        plan.execute().await.unwrap();
        plan.execute().await.unwrap();
        plan.execute().await.unwrap();

        assert!(!paths.data_directory.exists());
        assert!(!paths.config_directory.exists());
        assert!(repository.join(".git").exists());
        assert_eq!(
            tokio::fs::read(worktree.join("keep.txt")).await.unwrap(),
            b"keep"
        );
    }

    #[tokio::test]
    async fn removal_preserves_a_managed_worktree_inside_an_owned_root() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        let managed_worktree = paths.data_directory.join("worktrees/project/task");
        std::fs::create_dir_all(managed_worktree.join(".git")).unwrap();
        std::fs::write(managed_worktree.join("keep.txt"), b"keep").unwrap();
        std::fs::write(paths.data_directory.join("sylvops.db"), b"metadata").unwrap();

        let plan = DataRemovalPlan::prepare(
            &paths,
            DATA_REMOVAL_CONFIRMATION,
            std::slice::from_ref(&managed_worktree),
        )
        .unwrap();
        plan.execute().await.unwrap();

        assert_eq!(
            std::fs::read(managed_worktree.join("keep.txt")).unwrap(),
            b"keep"
        );
        assert!(!paths.data_directory.join("sylvops.db").exists());
        assert!(!paths.config_directory.exists());
        assert!(!paths.runtime_directory.exists());
    }

    #[test]
    fn removal_refuses_broad_or_unowned_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let mut paths = RuntimePaths::discover(Some(&temporary.path().join("state"))).unwrap();
        paths.prepare().unwrap();
        paths.data_directory = if cfg!(windows) {
            PathBuf::from(r"C:\")
        } else {
            PathBuf::from("/")
        };

        assert!(DataRemovalPlan::prepare(&paths, DATA_REMOVAL_CONFIRMATION, &[]).is_err());
    }
}
