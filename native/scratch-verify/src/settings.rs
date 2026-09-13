//! Stub of `crate::settings::get` for scratch-verify: `messaging.rs` reads env
//! vars through this seam in the real crate (so PHP-side `.chronos`/ini config
//! can layer under it), but scratch-verify only needs `messaging.rs` to
//! COMPILE and run its pure unit tests — neither `capture_enabled` nor
//! `body_ceiling` is exercised by those tests, so a plain env passthrough is
//! enough to satisfy the reference without pulling the real settings.rs (and
//! everything ext-php-rs it drags in) into this PHP-free scratch crate.
pub fn get(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
