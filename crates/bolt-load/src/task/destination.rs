//! Where a task saves its output: the configured destination, its
//! best-effort checks at spawn, and its resolution during initialization.

use std::{
    io,
    path::{Path, PathBuf},
};

use super::{BuildError, DownloadError, IoOp};
use crate::adapter::BoltLoadAdapterMeta;

#[derive(Debug, Clone)]
pub(crate) enum Destination {
    /// `save_to`: this exact path.
    File(PathBuf),
    /// `save_into`: this directory, with the filename the adapter suggests.
    Dir(PathBuf),
}

const NOT_A_DIR: &str = "the directory does not exist or is not a directory";
const PARENT_NOT_A_DIR: &str = "the parent directory does not exist or is not a directory";
const IS_A_DIR: &str = "the path is a directory";
const NO_FILE_NAME: &str = "the path has no file name";

impl Destination {
    /// The synchronous checks of `spawn`: one `metadata` call per path.
    /// Initialization repeats them and is authoritative.
    pub(crate) fn precheck(&self) -> Result<(), BuildError> {
        let invalid = |path: &Path, reason| BuildError::InvalidDestination {
            path: path.to_path_buf(),
            reason,
        };
        match self {
            Self::File(path) => {
                if path.file_name().is_none() {
                    return Err(invalid(path, NO_FILE_NAME));
                }
                if !std::fs::metadata(parent_dir(path)).is_ok_and(|m| m.is_dir()) {
                    return Err(invalid(path, PARENT_NOT_A_DIR));
                }
                if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
                    return Err(invalid(path, IS_A_DIR));
                }
            }
            Self::Dir(dir) => {
                if !std::fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
                    return Err(invalid(dir, NOT_A_DIR));
                }
            }
        }
        Ok(())
    }

    /// The path to save to: `save_to` as given, `save_into` joined with the
    /// adapter's filename reduced to one safe component.
    pub(crate) fn resolve(&self, meta: &BoltLoadAdapterMeta) -> Result<PathBuf, DownloadError> {
        match self {
            Self::File(path) => Ok(path.clone()),
            Self::Dir(dir) => {
                let name = meta
                    .filename
                    .as_deref()
                    .ok_or(DownloadError::MissingFilename)?;
                Ok(dir.join(safe_file_name(name)?))
            }
        }
    }
}

/// The authoritative checks of a resolved destination: its directory exists
/// and it is not a directory itself.
pub(crate) async fn check(path: &Path) -> Result<(), DownloadError> {
    let invalid = |reason| DownloadError::InvalidDestination {
        path: path.to_path_buf(),
        reason,
    };
    if path.file_name().is_none() {
        return Err(invalid(NO_FILE_NAME));
    }
    let parent = parent_dir(path);
    match async_fs::metadata(parent).await {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(invalid(PARENT_NOT_A_DIR)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(invalid(PARENT_NOT_A_DIR)),
        Err(e) => return Err(DownloadError::io(IoOp::Inspect, parent, e)),
    }
    match async_fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => Err(invalid(IS_A_DIR)),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DownloadError::io(IoOp::Inspect, path, e)),
    }
}

/// The directory of `path`; a bare file name is in the current directory.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Reduce a suggested filename to its final segment, split on both `/` and
/// `\`, and reject it unless it is one safe path component.
pub(crate) fn safe_file_name(name: &str) -> Result<&str, DownloadError> {
    let segment = name.rsplit(['/', '\\']).next().unwrap_or_default();
    if is_safe_component(segment, cfg!(windows)) {
        Ok(segment)
    } else {
        Err(DownloadError::InvalidFilename {
            name: name.to_owned(),
        })
    }
}

/// Whether `segment` names a file inside its directory, with the Windows
/// rules when `windows` is set.
fn is_safe_component(segment: &str, windows: bool) -> bool {
    if matches!(segment, "" | "." | "..") || segment.chars().any(char::is_control) {
        return false;
    }
    if windows {
        // A drive prefix or an alternate data stream.
        if segment.contains(':') {
            return false;
        }
        // Windows strips trailing dots and spaces, so `.. ` would become `..`.
        if segment.ends_with(['.', ' ']) || is_reserved_device_name(segment) {
            return false;
        }
    }
    true
}

/// `CON`, `NUL`, `COM1`, … are devices on Windows, with any extension.
fn is_reserved_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or_default().trim_end();
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => match upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
        {
            Some(n) => {
                let mut chars = n.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '¹' | '²' | '³'), None)
                )
            }
            None => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn meta(filename: Option<&str>) -> BoltLoadAdapterMeta {
        BoltLoadAdapterMeta {
            content_size: 1,
            filename: filename.map(String::from),
        }
    }

    #[test]
    fn traversal_is_reduced_to_the_final_segment() {
        for (name, expected) in [
            ("file.bin", "file.bin"),
            ("../../.bashrc", ".bashrc"),
            ("/etc/passwd", "passwd"),
            ("..\\..\\evil.exe", "evil.exe"),
            ("C:\\Windows\\win.ini", "win.ini"),
            ("a/b\\c", "c"),
        ] {
            assert_eq!(safe_file_name(name).unwrap(), expected, "{name:?}");
        }
    }

    #[test]
    fn unsafe_names_are_rejected() {
        for name in [
            "",
            ".",
            "..",
            "dir/",
            "dir/..",
            "a\0b",
            "line\nbreak",
            "tab\t",
        ] {
            assert!(
                matches!(
                    safe_file_name(name),
                    Err(DownloadError::InvalidFilename { name: n }) if n == name
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn windows_rules() {
        for name in [
            "C:",
            "a:b",
            "file.txt:stream",
            "CON",
            "con.txt",
            "Nul",
            "COM1",
            "lpt9.log",
            "COM¹",
            "AUX .txt",
            "trailing.",
            "trailing ",
        ] {
            assert!(!is_safe_component(name, true), "{name:?}");
            assert!(is_safe_component(name, false), "{name:?}");
        }
        for name in ["COM10", "CONSOLE", "nul_device", "file.txt", ".hidden"] {
            assert!(is_safe_component(name, true), "{name:?}");
        }
    }

    #[test]
    fn save_into_needs_a_filename() {
        let dest = Destination::Dir("dir".into());
        assert!(matches!(
            dest.resolve(&meta(None)),
            Err(DownloadError::MissingFilename)
        ));
        assert_eq!(
            dest.resolve(&meta(Some("../x.bin"))).unwrap(),
            Path::new("dir").join("x.bin")
        );
    }

    #[test]
    fn save_to_is_used_as_given() {
        let dest = Destination::File("a/../b.bin".into());
        assert_eq!(
            dest.resolve(&meta(Some("other"))).unwrap(),
            PathBuf::from("a/../b.bin")
        );
    }

    #[test]
    fn precheck_rejects_obvious_mistakes() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("missing");
        for (dest, reason) in [
            (Destination::File(dir.path().to_path_buf()), IS_A_DIR),
            (
                Destination::File(missing.join("file.bin")),
                PARENT_NOT_A_DIR,
            ),
            (Destination::File("..".into()), NO_FILE_NAME),
            (Destination::Dir(missing.clone()), NOT_A_DIR),
        ] {
            match dest.precheck() {
                Err(BuildError::InvalidDestination { reason: r, .. }) => {
                    assert_eq!(r, reason, "{dest:?}")
                }
                other => panic!("{dest:?}: {other:?}"),
            }
        }
        Destination::File(dir.path().join("new.bin"))
            .precheck()
            .unwrap();
        Destination::File("bare-name.bin".into())
            .precheck()
            .unwrap();
        Destination::Dir(dir.path().to_path_buf())
            .precheck()
            .unwrap();
    }

    #[tokio::test]
    async fn check_matches_precheck() {
        let dir = tempfile::TempDir::new().unwrap();
        check(&dir.path().join("new.bin")).await.unwrap();
        assert!(matches!(
            check(dir.path()).await,
            Err(DownloadError::InvalidDestination {
                reason: IS_A_DIR,
                ..
            })
        ));
        assert!(matches!(
            check(&dir.path().join("missing").join("x")).await,
            Err(DownloadError::InvalidDestination {
                reason: PARENT_NOT_A_DIR,
                ..
            })
        ));
    }
}
