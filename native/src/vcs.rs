//! The revision serving this request, read from the checkout beside the spool.
//!
//! A PHP deployment has no linker to stamp a commit into a binary the way the Go
//! SDK does, so the only ground truth available at runtime is the `.git` directory
//! the code was deployed from. That is a filesystem walk, which is why the answer
//! is memoised for the life of the process: the revision cannot change under a
//! running php-fpm worker, and paying for the walk per request would tax every
//! request to learn a constant.
//!
//! Both fields are best-effort. A container built with `COPY --exclude=.git`, or a
//! release tarball, legitimately has no revision — the caller stamps nothing rather
//! than inventing a value, so "unknown commit" stays distinguishable from a real one.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What the checkout says it is serving. Empty strings mean "not resolvable here".
#[derive(Clone, Debug, Default)]
pub struct Revision {
    pub commit: String,
    pub branch: String,
}

static RESOLVED: OnceLock<Revision> = OnceLock::new();

/// The revision, resolved once per process.
///
/// `spool_directory` is the starting point rather than the cwd because it is the one
/// path the collector is already configured with that points INTO the application:
/// php-fpm's cwd is wherever the pool was started, which on most images is `/`.
pub fn revision(spool_directory: &str) -> Revision {
    RESOLVED.get_or_init(|| resolve(spool_directory)).clone()
}

fn resolve(spool_directory: &str) -> Revision {
    // An explicitly configured revision wins over anything on disk: a build pipeline
    // that knows what it shipped is a better source than a `.git` that may have been
    // left behind by an unrelated layer.
    let commit = crate::settings::get("CHRONOS_APP_COMMIT").unwrap_or_default();
    let branch = crate::settings::get("CHRONOS_APP_BRANCH").unwrap_or_default();
    if !commit.is_empty() {
        return Revision {
            commit: clip(&commit),
            branch,
        };
    }

    let Some(git) = find_git_dir(Path::new(spool_directory)) else {
        return Revision::default();
    };
    let Ok(head) = std::fs::read_to_string(git.join("HEAD")) else {
        return Revision::default();
    };
    let head = head.trim();

    // Detached HEAD: the file IS the commit, and there is no branch to name.
    let Some(reference) = head.strip_prefix("ref: ") else {
        return Revision {
            commit: clip(head),
            branch: String::new(),
        };
    };

    let branch = reference.rsplit('/').next().unwrap_or_default().to_owned();
    let commit = std::fs::read_to_string(git.join(reference))
        .map(|sha| clip(sha.trim()))
        // A repository that has been `git gc`'d keeps its refs in `packed-refs`
        // instead of as loose files, which is the normal state of a fresh clone —
        // exactly what a deployment is.
        .unwrap_or_else(|_| packed_ref(&git, reference));

    Revision { commit, branch }
}

/// Walk up from the spool directory looking for the checkout that contains it.
///
/// Bounded to a few levels: the spool lives inside the application tree by
/// convention (`storage/chronos`), and an unbounded walk on a miss would stat its
/// way to `/` on every cold process in the fleet.
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    for _ in 0..8 {
        let directory = current?;
        let candidate = directory.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        // A worktree or submodule checkout has `.git` as a FILE pointing elsewhere.
        if candidate.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&candidate) {
                if let Some(path) = contents.trim().strip_prefix("gitdir: ") {
                    let path = PathBuf::from(path);
                    return Some(if path.is_absolute() {
                        path
                    } else {
                        directory.join(path)
                    });
                }
            }
        }
        current = directory.parent();
    }
    None
}

fn packed_ref(git: &Path, reference: &str) -> String {
    let Ok(packed) = std::fs::read_to_string(git.join("packed-refs")) else {
        return String::new();
    };
    for line in packed.lines() {
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        if let Some((sha, name)) = line.split_once(' ') {
            if name.trim() == reference {
                return clip(sha.trim());
            }
        }
    }
    String::new()
}

/// A commit is 40 hex characters (or 64 under SHA-256). Anything longer is not a
/// commit, and truncating keeps a malformed HEAD from becoming a huge attribute.
fn clip(value: &str) -> String {
    value.chars().take(64).collect()
}
