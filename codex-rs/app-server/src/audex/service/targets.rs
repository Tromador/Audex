//! Resolving a target never opens it or reads directory entries.
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targets {
    /// The operator's selection, in order, canonicalised but never expanded.
    pub paths: Vec<PathBuf>,
    pub working_directories: Vec<PathBuf>,
}

// Deliberately no content-reading or directory-listing operation in this interface.
trait PathLookup {
    fn canonicalise(&self, path: &Path) -> std::io::Result<PathBuf>;
    fn is_directory(&self, path: &Path) -> std::io::Result<bool>;
}

struct LivePaths;
impl PathLookup for LivePaths {
    fn canonicalise(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn is_directory(&self, path: &Path) -> std::io::Result<bool> {
        Ok(std::fs::metadata(path)?.is_dir())
    }
}

pub fn resolve(paths: &[PathBuf]) -> Result<Targets> {
    resolve_with(paths, &LivePaths)
}

fn resolve_with(paths: &[PathBuf], lookup: &impl PathLookup) -> Result<Targets> {
    ensure!(!paths.is_empty(), "at least one audit target is required");
    let mut targets = Targets {
        paths: Vec::new(),
        working_directories: Vec::new(),
    };
    for supplied in paths {
        let path = lookup.canonicalise(supplied)?;
        let directory = if lookup.is_directory(&path)? {
            path.clone()
        } else {
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("target has no containing directory"))?
                .to_path_buf()
        };
        targets.paths.push(path);
        if !targets.working_directories.contains(&directory) {
            targets.working_directories.push(directory);
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn target_resolution_only_canonicalises_and_stats_each_supplied_path() -> Result<()> {
        struct Lookup(RefCell<Vec<PathBuf>>);
        impl PathLookup for Lookup {
            fn canonicalise(&self, path: &Path) -> std::io::Result<PathBuf> {
                self.0.borrow_mut().push(path.to_path_buf());
                Ok(Path::new("/source").join(path))
            }
            fn is_directory(&self, path: &Path) -> std::io::Result<bool> {
                self.0.borrow_mut().push(path.to_path_buf());
                Ok(path == Path::new("/source/tree"))
            }
        }
        let lookup = Lookup(RefCell::new(Vec::new()));
        let supplied = vec!["tree".into(), "a.rs".into(), "b.rs".into(), "a.rs".into()];
        let result = resolve_with(&supplied, &lookup)?;
        assert_eq!(
            result.paths,
            supplied
                .iter()
                .map(|p| Path::new("/source").join(p))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            result.working_directories,
            vec![PathBuf::from("/source/tree"), PathBuf::from("/source")]
        );
        assert_eq!(
            *lookup.0.borrow(),
            supplied
                .iter()
                .flat_map(|p| [p.clone(), Path::new("/source").join(p)])
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn resolves_real_targets_when_directory_reads_are_denied() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("unreadable");
        std::fs::create_dir(&directory)?;
        let file = directory.join("named.rs");
        std::fs::write(&file, "operator-selected source")?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o111))?;
        // Restore permissions even if the assertions fail, so TempDir can clean up.
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let _restore = Restore(directory.clone());
        assert_eq!(
            std::fs::read_dir(&directory)
                .err()
                .expect("listing denied")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let resolved = resolve(&[directory.clone(), file.clone()])?;
        assert_eq!(resolved.paths, vec![directory.clone(), file]);
        assert_eq!(resolved.working_directories, vec![directory]);
        assert!(resolve(&[temp.path().join("missing")]).is_err());
        Ok(())
    }
}
