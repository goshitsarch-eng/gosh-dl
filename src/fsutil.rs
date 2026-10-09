//! Filesystem path hygiene shared by the HTTP, torrent, and engine
//! lifecycle code.
//!
//! Output names reach the engine from untrusted places: URL path segments,
//! `Content-Disposition` headers, torrent `name`/`path` fields, and magnet
//! `dn` parameters. Every path that is joined onto a save directory and then
//! created, read, or **deleted** must pass through [`check_relative_path`].

use crate::error::{EngineError, Result, StorageErrorKind};
use std::path::{Component, Path};

/// Why a relative output path was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathRejection {
    /// No normal component at all (`""`, `"."`, `"./"`).
    Empty,
    /// Contains a `..` component.
    ParentDir,
    /// Absolute path or Windows prefix (`/etc`, `C:\`).
    Absolute,
}

impl PathRejection {
    pub(crate) fn describe(self, what: &str) -> String {
        match self {
            Self::Empty => format!("{what} must name a file"),
            Self::ParentDir => format!("{what} contains parent directory reference (..)"),
            Self::Absolute => format!("{what} contains absolute path"),
        }
    }
}

/// Check that `path` stays inside whatever directory it is joined onto.
///
/// Relative sub-paths (`sub/file.bin`) are allowed; `..`, root, and prefix
/// components are not, and the path must contain at least one real name.
pub(crate) fn check_relative_path(path: &Path) -> std::result::Result<(), PathRejection> {
    let mut has_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir => return Err(PathRejection::ParentDir),
            Component::RootDir | Component::Prefix(_) => return Err(PathRejection::Absolute),
        }
    }
    if has_normal {
        Ok(())
    } else {
        Err(PathRejection::Empty)
    }
}

/// Reject output filenames that would escape the save directory, reporting
/// the problem as a storage error (the HTTP and engine lifecycle flavour).
pub(crate) fn validate_output_name(name: &str) -> Result<()> {
    match check_relative_path(Path::new(name)) {
        Ok(()) => Ok(()),
        Err(PathRejection::Empty) => Err(EngineError::invalid_input(
            "filename",
            "Filename must name a file",
        )),
        Err(rejection) => Err(EngineError::storage(
            StorageErrorKind::PathTraversal,
            Path::new(name),
            format!("Invalid filename: {}", rejection.describe("filename")),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_relative_names() {
        assert!(check_relative_path(Path::new("file.zip")).is_ok());
        assert!(check_relative_path(Path::new("sub/dir/file.zip")).is_ok());
        assert!(check_relative_path(Path::new("./file.zip")).is_ok());
    }

    #[test]
    fn rejects_escapes_and_empty() {
        assert_eq!(
            check_relative_path(Path::new("../evil")),
            Err(PathRejection::ParentDir)
        );
        assert_eq!(
            check_relative_path(Path::new("a/../../evil")),
            Err(PathRejection::ParentDir)
        );
        assert_eq!(
            check_relative_path(Path::new("/etc/passwd")),
            Err(PathRejection::Absolute)
        );
        assert_eq!(
            check_relative_path(Path::new("")),
            Err(PathRejection::Empty)
        );
        assert_eq!(
            check_relative_path(Path::new(".")),
            Err(PathRejection::Empty)
        );
        assert_eq!(
            check_relative_path(Path::new("sub/..")),
            Err(PathRejection::ParentDir)
        );
    }

    #[test]
    fn output_name_errors_are_typed() {
        assert!(matches!(
            validate_output_name("../x"),
            Err(EngineError::Storage {
                kind: StorageErrorKind::PathTraversal,
                ..
            })
        ));
        assert!(matches!(
            validate_output_name(""),
            Err(EngineError::InvalidInput {
                field: "filename",
                ..
            })
        ));
        assert!(validate_output_name("ok.bin").is_ok());
    }
}
