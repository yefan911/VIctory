//! Rule #1, enforced at runtime: game-derived data never lands in the repo.
//!
//! Everything VIctory produces from the user's files (converted assets,
//! prepared images, shader caches, crash dumps) is as much "from the game"
//! as the files themselves. Every tool that writes such output must call
//! [`ensure_outside_repo`] on the destination first. The static half of the
//! rule is `cargo xtask policy`, which checks what git tracks.

use std::path::{Component, Path, PathBuf};

/// Marker file at the root of the source tree.
pub const ROOT_MARKER: &str = ".victory-root";

/// The source tree this binary was compiled from.
fn compiled_workspace_root() -> PathBuf {
    // crates/victory-core -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "refusing to use {path} for game-derived data: it is inside the VIctory source tree \
         ({root}). Keep game files, caches and outputs outside the repository \
         (see docs/asset-policy.md)"
    )]
    InsideRepo { path: PathBuf, root: PathBuf },
    #[error("cannot resolve {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Returns `Ok` only if `path` is outside every VIctory source tree.
///
/// `path` does not have to exist yet. Two checks are made:
/// 1. it is not below the source tree this binary was built from;
/// 2. none of its ancestors contains the [`ROOT_MARKER`] file (catches
///    other clones/copies of the repository).
pub fn ensure_outside_repo(path: &Path) -> Result<(), PolicyError> {
    let resolved = resolve(path)?;

    if let Ok(root) = std::fs::canonicalize(compiled_workspace_root())
        && root.join(ROOT_MARKER).is_file()
        && resolved.starts_with(&root)
    {
        return Err(PolicyError::InsideRepo {
            path: path.to_path_buf(),
            root,
        });
    }

    for ancestor in resolved.ancestors() {
        if ancestor.join(ROOT_MARKER).is_file() {
            return Err(PolicyError::InsideRepo {
                path: path.to_path_buf(),
                root: ancestor.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// Canonicalizes the longest existing prefix of `path` and re-appends the
/// rest, so that paths which do not exist yet can still be compared.
fn resolve(path: &Path) -> Result<PathBuf, PolicyError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| PolicyError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };

    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canon) => return Ok(append_tail(canon, rest.iter().rev())),
            // The last component, not `file_name`: that is `None` for `..`,
            // which a path may well hold below a directory not created yet.
            Err(source) => match (existing.parent(), existing.components().next_back()) {
                (Some(parent), Some(name @ (Component::Normal(_) | Component::ParentDir))) => {
                    rest.push(name.as_os_str().to_os_string());
                    existing = parent;
                }
                _ => {
                    return Err(PolicyError::Io {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            },
        }
    }
}

/// Appends the components of a tail that did not resolve to the canonical
/// path `out`, one at a time.
///
/// A tail fails to resolve as a whole as soon as one of its directories is
/// missing, yet past a `..` it can name existing paths again:
/// `missing/../link/out` is `link/out` once `missing` is created (which
/// `create_dir_all` does), and `link` may be a symlink into the source tree.
/// Normalizing the tail lexically never looked at `link` and accepted the
/// path. So `..` drops the last component (one that does not exist, or the
/// canonical prefix, which holds no symlinks), and after every name that
/// exists the path is canonicalized again, following any symlink there.
fn append_tail<'a>(
    mut out: PathBuf,
    tail: impl IntoIterator<Item = &'a std::ffi::OsString>,
) -> PathBuf {
    for component in tail {
        if component == Component::ParentDir.as_os_str() {
            out.pop();
            continue;
        }
        out.push(component);
        if let Ok(canon) = std::fs::canonicalize(&out) {
            out = canon;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory outside every VIctory source tree.
    ///
    /// `tempfile::tempdir()` alone follows `TMPDIR`, and with `TMPDIR` inside
    /// the checkout (a scratch directory there is common) every directory it
    /// makes is inside the repository: the tests that need an outside
    /// directory then fail, and the marker test passes without the marker
    /// mattering. So `TMPDIR` is used only when it is outside; otherwise the
    /// nearest writable ancestor of the checkout that is outside, and failing
    /// that the test says what to do instead of failing on a misleading
    /// `InsideRepo`.
    fn outside_tempdir() -> tempfile::TempDir {
        let root = std::fs::canonicalize(compiled_workspace_root()).unwrap();
        let candidates = std::iter::once(std::env::temp_dir())
            .chain(root.ancestors().skip(1).map(Path::to_path_buf));
        for dir in candidates {
            let Ok(tmp) = tempfile::tempdir_in(&dir) else {
                continue;
            };
            if ensure_outside_repo(tmp.path()).is_ok() {
                return tmp;
            }
        }
        panic!(
            "no writable temporary directory outside the source tree ({}); \
             set TMPDIR to one",
            root.display()
        );
    }

    #[test]
    fn rejects_paths_inside_this_repo() {
        let inside = compiled_workspace_root().join("cache").join("textures");
        assert!(matches!(
            ensure_outside_repo(&inside),
            Err(PolicyError::InsideRepo { .. })
        ));
        let sneaky = std::env::temp_dir().join("..").join("x"); // sanity: no panic
        let _ = ensure_outside_repo(&sneaky);
    }

    #[test]
    fn rejects_dotdot_escapes_back_into_repo() {
        let root = compiled_workspace_root();
        let path = root
            .join("crates")
            .join("..")
            .join("not-yet-created")
            .join("out");
        assert!(ensure_outside_repo(&path).is_err());
    }

    #[test]
    fn resolves_dotdot_after_a_missing_directory() {
        // `Path::file_name` is `None` for a path ending in `..`, so the walk
        // up to an existing prefix has to keep that component too.
        let tmp = outside_tempdir();
        let path = tmp.path().join("missing").join("..").join("cache");
        ensure_outside_repo(&path).unwrap();

        let inside = compiled_workspace_root()
            .join("not-yet-created")
            .join("..")
            .join("out");
        assert!(matches!(
            ensure_outside_repo(&inside),
            Err(PolicyError::InsideRepo { .. })
        ));
    }

    /// `missing/../link/out` resolves to `link/out` once `missing` exists,
    /// and `link` here leads into a directory of another source tree. The
    /// tail after the missing directory used to be normalized lexically, so
    /// the symlink was never followed, no ancestor of the unresolved path
    /// held the marker, and the path was accepted.
    #[cfg(unix)]
    #[test]
    fn follows_symlinks_named_after_a_missing_directory() {
        let tree = outside_tempdir();
        std::fs::write(tree.path().join(ROOT_MARKER), "").unwrap();
        let cache = tree.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        let tmp = outside_tempdir();
        std::os::unix::fs::symlink(&cache, tmp.path().join("link")).unwrap();

        let path = tmp
            .path()
            .join("missing")
            .join("..")
            .join("link")
            .join("out");
        assert!(matches!(
            ensure_outside_repo(&path),
            Err(PolicyError::InsideRepo { .. })
        ));
        // And that is where a write to it lands.
        std::fs::create_dir_all(&path).unwrap();
        assert!(cache.join("out").is_dir());

        // `..` after a missing directory still steps out of it lexically.
        let path = tmp.path().join("missing").join("..").join("cache");
        ensure_outside_repo(&path).unwrap();
    }

    #[test]
    fn rejects_other_clones_via_marker() {
        let tmp = outside_tempdir();
        // Accepted before the marker is written, so the marker is what
        // makes the difference.
        ensure_outside_repo(&tmp.path().join("cache")).unwrap();
        std::fs::write(tmp.path().join(ROOT_MARKER), "").unwrap();
        assert!(matches!(
            ensure_outside_repo(&tmp.path().join("cache")),
            Err(PolicyError::InsideRepo { .. })
        ));
    }

    #[test]
    fn accepts_unrelated_directories() {
        let tmp = outside_tempdir();
        ensure_outside_repo(&tmp.path().join("does").join("not").join("exist")).unwrap();
    }
}
