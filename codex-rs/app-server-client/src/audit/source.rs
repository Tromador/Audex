//! Shared explicit file selection. Resolving paths never discovers or opens files.
use codex_app_server_protocol::{AuditSource, AuditSourceFile};
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::{Path, PathBuf};

/// Keep the single-file basename contract. For a list, retain paths below the
/// common parent so equal basenames remain distinct, independent of input order.
pub fn file_source(paths: &[PathBuf], cwd: &Path) -> Result<AuditSource, String> {
    if paths.is_empty() {
        return Err("select at least one file".to_owned());
    }
    let absolute: Vec<_> = paths
        .iter()
        .map(|path| AbsolutePathBuf::resolve_path_against_base(path, cwd))
        .collect();
    let mut parent = absolute[0]
        .parent()
        .ok_or("file needs a parent directory")?;
    for path in &absolute {
        while !path.starts_with(&parent) {
            parent = parent.parent().ok_or("selected files need a common filesystem root; use typed file names for separate roots")?;
        }
    }
    let files = absolute
        .iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&parent)
                .map_err(|error| error.to_string())?;
            let name = relative
                .components()
                .map(|part| part.as_os_str().to_str().ok_or("file path must be UTF-8"))
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            Ok(AuditSourceFile {
                name,
                path: path.to_str().ok_or("source path must be UTF-8")?.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if files.len() == 1 {
        let file = files.into_iter().next().ok_or("select a file")?;
        Ok(AuditSource::File {
            name: file.name,
            path: file.path,
        })
    } else {
        Ok(AuditSource::FileList { files })
    }
}

/// Model tools may supply relative inputs; names are explicit and stay intact.
pub fn resolve_source_paths(source: &mut AuditSource, cwd: &Path) -> Result<(), String> {
    let resolve = |path: &mut String| -> Result<(), String> {
        *path = AbsolutePathBuf::resolve_path_against_base(Path::new(path), cwd)
            .to_str()
            .ok_or("source path must be UTF-8")?
            .to_owned();
        Ok(())
    };
    match source {
        AuditSource::File { path, .. } => resolve(path),
        AuditSource::FileList { files } => {
            for file in files {
                resolve(&mut file.path)?;
            }
            Ok(())
        }
        AuditSource::Snippet { .. } => Ok(()),
    }
}
