use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::path::PathBuf;

/// Returns the path to the Audex configuration directory, which can be
/// specified by the `AUDEX_HOME` environment variable. If not set, defaults to
/// `~/.audex`.
///
/// - If `AUDEX_HOME` is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If `AUDEX_HOME` is not set, this function does not verify that the
///   directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let codex_home_env = std::env::var("AUDEX_HOME")
        .ok()
        .filter(|val| !val.is_empty());
    find_codex_home_from_env(codex_home_env.as_deref())
}

fn find_codex_home_from_env(codex_home_env: Option<&str>) -> std::io::Result<AbsolutePathBuf> {
    // Honor the `AUDEX_HOME` environment variable when it is set to allow users
    // (and tests) to override the default location.
    match codex_home_env {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("AUDEX_HOME points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read AUDEX_HOME {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("AUDEX_HOME points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize AUDEX_HOME {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path(canonical)
            }
        }
        None => {
            let mut p = home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            p.push(".audex");
            AbsolutePathBuf::from_absolute_path(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    #[test]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(missing_str)).expect_err("missing AUDEX_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("AUDEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(file_str)).expect_err("file AUDEX_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env(Some(temp_str)).expect("valid AUDEX_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_without_env_uses_default_home_dir() {
        let resolved =
            find_codex_home_from_env(/*codex_home_env*/ None).expect("default AUDEX_HOME");
        let mut expected = home_dir().expect("home dir");
        expected.push(".audex");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[cfg(unix)]
    #[test]
    fn application_home_environment_is_independent_of_codex_home() {
        let root = TempDir::new().expect("isolated HOME");
        let selected = root.path().join("selected");
        fs::create_dir(&selected).unwrap();
        std::os::unix::fs::symlink(&selected, root.path().join("alias")).unwrap();
        let upstream = root.path().join("upstream-must-remain-absent");
        let invalid_file = root.path().join("file");
        fs::write(&invalid_file, "not a directory").unwrap();
        for (audex, codex, expected, error) in [
            (None, None, ".audex", ""),
            (Some("selected"), None, "selected", ""),
            (None, Some(upstream.as_os_str()), ".audex", ""),
            (Some("selected"), Some(upstream.as_os_str()), "selected", ""),
            (Some(""), Some(upstream.as_os_str()), ".audex", ""),
            (Some("alias"), Some(upstream.as_os_str()), "selected", ""),
            (
                Some("missing"),
                Some(selected.as_os_str()),
                "",
                "does not exist",
            ),
            (
                Some("file"),
                Some(selected.as_os_str()),
                "",
                "not a directory",
            ),
        ] {
            let expected = root.path().join(expected);
            let expected = if error.is_empty() && audex.is_some_and(|value| !value.is_empty()) {
                expected.canonicalize().unwrap()
            } else {
                expected
            };
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "tests::application_home_environment_child"])
                .current_dir(root.path())
                .env("HOME", root.path())
                .env_remove("AUDEX_HOME")
                .env_remove("CODEX_HOME")
                .env("AUDEX_TEST_EXPECTED_HOME", expected)
                .env("AUDEX_TEST_EXPECTED_ERROR", error);
            if let Some(value) = audex {
                command.env("AUDEX_HOME", value);
            }
            if let Some(value) = codex {
                command.env("CODEX_HOME", value);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{audex:?}, {codex:?}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(!upstream.exists());
        assert!(
            !root.path().join(".audex").exists(),
            "default resolution must not create directories"
        );
    }

    #[cfg(unix)]
    #[test]
    fn application_home_environment_child() {
        let Some(expected) = std::env::var_os("AUDEX_TEST_EXPECTED_HOME") else {
            return;
        };
        let error = std::env::var("AUDEX_TEST_EXPECTED_ERROR").unwrap();
        let actual = super::find_codex_home();
        if error.is_empty() {
            assert_eq!(actual.unwrap().as_path(), std::path::Path::new(&expected));
        } else {
            let actual = actual.unwrap_err();
            assert!(actual.to_string().contains("AUDEX_HOME"));
            assert!(actual.to_string().contains(&error));
            assert!(!actual.to_string().contains("CODEX_HOME"));
        }
    }
}
