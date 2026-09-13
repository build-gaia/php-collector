//! Scratch-verify copy of the `.chronos`-file identity validators from
//! `context.rs` (`is_identifier_shaped` / `is_safe_spool_path`) — pure, std-only,
//! Zend-free logic, copied here for the same reason `messaging.rs` is: the real
//! crate's test binary cannot RUN outside a PHP host process (see
//! `chronos-collector-local-tests` memory / this crate's own README), so the
//! pure predicates these two `context.rs` functions ARE get proven here instead.
//!
//! Any change to the real functions in `src/context.rs` must be mirrored here —
//! this file is a copy, not a shared module, and drift between the two is
//! exactly the failure this workaround cannot catch automatically.

/// Whether a `.chronos`-file-sourced organisation/project/application id looks like an
/// identifier, rather than a copy-paste accident (a stray `;`, a whole INI line pasted
/// into the wrong slot, an empty string). The estate's own ids are `org_<uuid>` or
/// kebab-case names, so the allowed alphabet is deliberately generous — `[A-Za-z0-9._-]`,
/// 1 to 128 bytes — because this exists to catch garbage, not to enforce the estate's
/// naming convention on every future id shape.
pub fn is_identifier_shaped(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether a `.chronos`-file-sourced spool directory is safe to use as a filesystem
/// write target: an absolute path (so it never resolves against whatever cwd the
/// worker happens to have) with no `..` segment (so it can never walk the write outside
/// the intended spool tree, once `tenant_spool_directory` joins a tenant subdirectory
/// onto it).
pub fn is_safe_spool_path(value: &str) -> bool {
    value.starts_with('/') && !value.split('/').any(|segment| segment == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_shape_accepts_the_estate_s_real_ids() {
        assert!(is_identifier_shaped(
            "org_a2a69137-6d90-49cc-90b9-5d3e49f1ef96"
        ));
        assert!(is_identifier_shaped("qls-shipping"));
        assert!(is_identifier_shaped("project_qls"));
        assert!(is_identifier_shaped("a"));
    }

    #[test]
    fn identifier_shape_rejects_garbage() {
        assert!(!is_identifier_shaped(""));
        assert!(!is_identifier_shaped("bad;value"));
        assert!(!is_identifier_shaped("has space"));
        assert!(!is_identifier_shaped("quote\"d"));
        assert!(!is_identifier_shaped(&"x".repeat(129)));
        // Exactly at the boundary must still pass.
        assert!(is_identifier_shaped(&"x".repeat(128)));
    }

    #[test]
    fn spool_path_accepts_absolute_paths_without_traversal() {
        assert!(is_safe_spool_path("/var/lib/chronos/php"));
        assert!(is_safe_spool_path("/"));
    }

    #[test]
    fn spool_path_rejects_relative_and_traversal() {
        assert!(!is_safe_spool_path("relative/path"));
        assert!(!is_safe_spool_path(""));
        assert!(!is_safe_spool_path("/var/lib/../../etc"));
        assert!(!is_safe_spool_path("/var/lib/.."));
    }
}
