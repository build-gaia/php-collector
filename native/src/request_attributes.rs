//! Extra attributes the userland SDK stamps on the request root span.
//!
//! The native collector emits the request root at `chronos_request_end`. Userland
//! cannot finish the synthetic SpanManager root (that would duplicate it), so
//! framework facts — route action, auth id, view/model counts — are handed
//! across this bag and merged just before the root is written.
//!
//! Caps match the rest of capture: bounded keys/values, last write wins, new
//! keys dropped once the map is full. Empty values are ignored.

use std::cell::RefCell;

const MAX_ATTRIBUTES: usize = 32;
const MAX_KEY_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 8192;

thread_local! {
    static BAG: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

pub fn reset() {
    BAG.with(|cell| cell.borrow_mut().clear());
}

pub fn merge(attributes: impl IntoIterator<Item = (String, String)>) {
    BAG.with(|cell| {
        let mut bag = cell.borrow_mut();
        for (key, value) in attributes {
            let key = cap(key.trim(), MAX_KEY_BYTES);
            if key.is_empty() {
                continue;
            }
            let value = cap(value.trim(), MAX_VALUE_BYTES);
            if value.is_empty() {
                continue;
            }
            if let Some(existing) = bag.iter_mut().find(|(k, _)| k == &key) {
                existing.1 = value;
                continue;
            }
            if bag.len() >= MAX_ATTRIBUTES {
                continue;
            }
            bag.push((key, value));
        }
    });
}

pub fn take() -> Vec<(String, String)> {
    BAG.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

fn cap(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear() {
        reset();
    }

    #[test]
    fn last_write_wins_and_empty_keys_are_ignored() {
        clear();
        merge([
            ("http.route.action".into(), "OldController@show".into()),
            ("http.route.action".into(), "UserController@show".into()),
            ("".into(), "nope".into()),
            ("enduser.id".into(), "".into()),
        ]);
        let bag = take();
        assert_eq!(
            bag,
            vec![("http.route.action".into(), "UserController@show".into())]
        );
    }

    #[test]
    fn new_keys_are_dropped_once_the_map_is_full() {
        clear();
        let attrs = (0..MAX_ATTRIBUTES + 4).map(|i| (format!("k{i}"), "v".into()));
        merge(attrs);
        let bag = take();
        assert_eq!(bag.len(), MAX_ATTRIBUTES);
        assert_eq!(bag[0], ("k0".into(), "v".into()));
        assert!(bag.iter().all(|(k, _)| k != "k32"));
    }

    #[test]
    fn overwriting_an_existing_key_still_works_when_full() {
        clear();
        merge((0..MAX_ATTRIBUTES).map(|i| (format!("k{i}"), "v".into())));
        merge([("k0".into(), "updated".into())]);
        let bag = take();
        assert_eq!(bag[0], ("k0".into(), "updated".into()));
        assert_eq!(bag.len(), MAX_ATTRIBUTES);
    }

    #[test]
    fn values_are_truncated_not_rejected() {
        clear();
        merge([("laravel.views".into(), "x".repeat(MAX_VALUE_BYTES + 50))]);
        let bag = take();
        assert_eq!(bag[0].1.len(), MAX_VALUE_BYTES);
    }
}
