//! ADR 0021 Phase 2 — DST call-path retention.
//!
//! When a DST session is active the Zend observer already sees every call. This module
//! decides which of those calls are retained as `call` events: first-party only, depth-capped,
//! and hard-capped by event count so a framework request cannot explode the recording.
//!
//! Shape is an ordered visit list (not a full tree): enough for Phase 3 path-diff to name the
//! first divergent frame without storing parent pointers for tens of thousands of library calls.

use std::cell::RefCell;

thread_local! {
    static DEPTH: RefCell<u32> = const { RefCell::new(0) };
    static RETAINED: RefCell<u32> = const { RefCell::new(0) };
    static TRUNCATED: RefCell<bool> = const { RefCell::new(false) };
    static CAPS: RefCell<Caps> = RefCell::new(Caps {
        max_events: DEFAULT_MAX_EVENTS,
        max_depth: DEFAULT_MAX_DEPTH,
    });
}

/// Default hard cap on retained call-path events per request.
pub const DEFAULT_MAX_EVENTS: u32 = 4_096;

/// Default maximum nesting depth to retain (deeper frames are skipped, not errors).
pub const DEFAULT_MAX_DEPTH: u32 = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    pub max_events: u32,
    pub max_depth: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_events: DEFAULT_MAX_EVENTS,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

impl Caps {
    pub fn from_env() -> Self {
        Self {
            max_events: crate::settings::u32_value("CHRONOS_PHP_DST_CALL_PATH_MAX", DEFAULT_MAX_EVENTS)
                .max(1),
            max_depth: crate::settings::u32_value(
                "CHRONOS_PHP_DST_CALL_PATH_MAX_DEPTH",
                DEFAULT_MAX_DEPTH,
            )
            .max(1),
        }
    }
}

pub fn reset() {
    DEPTH.with(|d| *d.borrow_mut() = 0);
    RETAINED.with(|r| *r.borrow_mut() = 0);
    TRUNCATED.with(|t| *t.borrow_mut() = false);
    CAPS.with(|c| *c.borrow_mut() = Caps::default());
}

/// Arm call-path capture for a DST session (resolves caps once per request).
pub fn arm() {
    reset();
    CAPS.with(|c| *c.borrow_mut() = Caps::from_env());
}

pub fn caps() -> Caps {
    CAPS.with(|c| c.borrow().clone())
}

/// Enter a first-party call. Returns the depth to record, or `None` when the frame is dropped
/// by the depth or event budget (still tracks depth so leave stays balanced).
pub fn on_enter(caps: &Caps, first_party: bool) -> Option<u32> {
    let depth = DEPTH.with(|d| {
        let mut depth = d.borrow_mut();
        *depth = depth.saturating_add(1);
        *depth
    });
    if !first_party {
        return None;
    }
    if depth > caps.max_depth {
        return None;
    }
    let retain = RETAINED.with(|r| {
        let mut retained = r.borrow_mut();
        if *retained >= caps.max_events {
            TRUNCATED.with(|t| *t.borrow_mut() = true);
            return false;
        }
        *retained = retained.saturating_add(1);
        true
    });
    if retain {
        Some(depth)
    } else {
        None
    }
}

pub fn on_leave() {
    DEPTH.with(|d| {
        let mut depth = d.borrow_mut();
        *depth = depth.saturating_sub(1);
    });
}

pub fn retained_count() -> u32 {
    RETAINED.with(|r| *r.borrow())
}

pub fn was_truncated() -> bool {
    TRUNCATED.with(|t| *t.borrow())
}

/// Whether a defining file is first-party for call-path capture. Unknown/empty files are
/// retained (same posture as span path exclusion: drop only what we positively identify as
/// dependency code).
pub fn is_first_party(defining_file: Option<&str>, excluded_path_fragments: &[String]) -> bool {
    match defining_file {
        None | Some("") => true,
        Some(file) => !excluded_path_fragments
            .iter()
            .any(|fragment| file.contains(fragment.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_dependency_frames_but_tracks_depth() {
        reset();
        let caps = Caps {
            max_events: 10,
            max_depth: 8,
        };
        assert_eq!(on_enter(&caps, true), Some(1));
        assert_eq!(on_enter(&caps, false), None);
        assert_eq!(on_enter(&caps, true), Some(3));
        on_leave();
        on_leave();
        on_leave();
        assert_eq!(retained_count(), 2);
    }

    #[test]
    fn respects_event_budget_and_marks_truncation() {
        reset();
        let caps = Caps {
            max_events: 2,
            max_depth: 8,
        };
        assert!(on_enter(&caps, true).is_some());
        assert!(on_enter(&caps, true).is_some());
        assert!(on_enter(&caps, true).is_none());
        assert!(was_truncated());
        assert_eq!(retained_count(), 2);
    }

    #[test]
    fn respects_depth_cap() {
        reset();
        let caps = Caps {
            max_events: 100,
            max_depth: 2,
        };
        assert_eq!(on_enter(&caps, true), Some(1));
        assert_eq!(on_enter(&caps, true), Some(2));
        assert_eq!(on_enter(&caps, true), None);
        assert_eq!(retained_count(), 2);
    }

    #[test]
    fn first_party_uses_excluded_path_fragments() {
        let excluded = vec!["/vendor/".to_owned(), "/node_modules/".to_owned()];
        assert!(is_first_party(None, &excluded));
        assert!(is_first_party(Some("/app/Http/Kernel.php"), &excluded));
        assert!(!is_first_party(Some("/app/vendor/laravel/framework/x.php"), &excluded));
    }
}
