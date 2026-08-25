//! ADR 0021 — scalar Effect interception for replay.
//!
//! Swaps `zend_internal_function.handler` for time/random/getenv builtins when
//! `chronos_replay_arm()` is called (ReplayRuntime boot). The hooks call the
//! well-known userland function `chronos_replay_effect_delegate($kind, $selector)`
//! which routes through `Chronos\Collector\Replay\Effect`.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

#[cfg(feature = "zend-observer")]
use ext_php_rs::convert::IntoZval;
#[cfg(feature = "zend-observer")]
use ext_php_rs::ffi::{
    zend_execute_data, zend_fetch_function_str, zif_handler, zval, ZEND_INTERNAL_FUNCTION,
};
#[cfg(feature = "zend-observer")]
use ext_php_rs::types::{ZendCallable, Zval};

static ARMED: AtomicBool = AtomicBool::new(false);

struct HookSlot {
    name: &'static str,
    original: AtomicPtr<()>,
}

#[cfg(feature = "zend-observer")]
static SLOTS: [HookSlot; 9] = [
    HookSlot { name: "time", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "microtime", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "hrtime", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "mt_rand", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "rand", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "random_int", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "random_bytes", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "uniqid", original: AtomicPtr::new(std::ptr::null_mut()) },
    HookSlot { name: "getenv", original: AtomicPtr::new(std::ptr::null_mut()) },
];

const DELEGATE_NAME: &str = "chronos_replay_effect_delegate";

/// Arm internal-function overrides. Idempotent.
#[cfg(feature = "zend-observer")]
pub fn arm() {
    if ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        for slot in SLOTS.iter() {
            let func = zend_fetch_function_str(slot.name.as_ptr().cast(), slot.name.len());
            if func.is_null() || (*func).type_ != ZEND_INTERNAL_FUNCTION as u8 {
                continue;
            }
            let original = (*func).internal_function.handler;
            slot.original.store(
                original.map(|f| f as *mut ()).unwrap_or(std::ptr::null_mut()),
                Ordering::SeqCst,
            );
            (*func).internal_function.handler = handler_for(slot.name);
        }
    }
}

#[cfg(not(feature = "zend-observer"))]
pub fn arm() {}

#[cfg(feature = "zend-observer")]
pub fn disarm() {
    if !ARMED.swap(false, Ordering::SeqCst) {
        return;
    }
    unsafe {
        for slot in SLOTS.iter() {
            let func = zend_fetch_function_str(slot.name.as_ptr().cast(), slot.name.len());
            if func.is_null() {
                continue;
            }
            let ptr = slot.original.load(Ordering::SeqCst);
            (*func).internal_function.handler = if ptr.is_null() {
                None
            } else {
                Some(std::mem::transmute(ptr))
            };
        }
    }
}

#[cfg(not(feature = "zend-observer"))]
pub fn disarm() {
    ARMED.store(false, Ordering::SeqCst);
}

pub fn is_armed() -> bool {
    ARMED.load(Ordering::SeqCst)
}

pub fn kind_for_symbol(name: &str) -> Option<&'static str> {
    match name {
        "time" | "microtime" | "hrtime" | "date" | "gettimeofday" => Some("time"),
        "mt_rand" | "rand" | "random_int" | "random_bytes" | "uniqid" => Some("random"),
        "getenv" => Some("env"),
        _ => None,
    }
}

#[cfg(feature = "zend-observer")]
fn handler_for(name: &str) -> zif_handler {
    match name {
        "time" => Some(hook_time),
        "microtime" => Some(hook_microtime),
        "hrtime" => Some(hook_hrtime),
        "mt_rand" => Some(hook_mt_rand),
        "rand" => Some(hook_rand),
        "random_int" => Some(hook_random_int),
        "random_bytes" => Some(hook_random_bytes),
        "uniqid" => Some(hook_uniqid),
        "getenv" => Some(hook_getenv),
        _ => None,
    }
}

#[cfg(feature = "zend-observer")]
fn original_for(name: &str) -> zif_handler {
    for slot in SLOTS.iter() {
        if slot.name == name {
            let ptr = slot.original.load(Ordering::SeqCst);
            if ptr.is_null() {
                return None;
            }
            return Some(unsafe { std::mem::transmute(ptr) });
        }
    }
    None
}

#[cfg(feature = "zend-observer")]
fn call_original(name: &str, execute_data: *mut zend_execute_data, return_value: *mut zval) {
    if let Some(handler) = original_for(name) {
        unsafe { handler(execute_data, return_value) };
    }
}

#[cfg(feature = "zend-observer")]
fn try_delegate(kind: &str, selector: &str) -> Option<String> {
    let Ok(callable) = ZendCallable::try_from_name(DELEGATE_NAME) else {
        return None;
    };
    match callable.try_call(vec![&kind, &selector]) {
        Ok(result) => extract_result_string(&result),
        Err(_) => None,
    }
}

#[cfg(feature = "zend-observer")]
fn extract_result_string(zv: &Zval) -> Option<String> {
    if zv.is_null() {
        return None;
    }
    if let Some(arr) = zv.array() {
        if let Some(v) = arr.get("result") {
            if let Some(s) = v.string() {
                return Some(s);
            }
            if let Some(n) = v.long() {
                return Some(n.to_string());
            }
            if let Some(n) = v.double() {
                return Some(n.to_string());
            }
        }
        if let Some(v) = arr.get("value") {
            if let Some(s) = v.string() {
                return Some(s);
            }
            if let Some(n) = v.long() {
                return Some(n.to_string());
            }
            if let Some(n) = v.double() {
                return Some(n.to_string());
            }
        }
    }
    zv.string()
}

#[cfg(feature = "zend-observer")]
unsafe fn write_scalar_return(return_value: *mut zval, function: &str, raw: &str) {
    if return_value.is_null() {
        return;
    }
    let built: Zval = match function {
        "time" | "mt_rand" | "rand" | "random_int" => {
            if let Ok(n) = raw.parse::<i64>() {
                n.into_zval(false).unwrap_or_else(|_| raw.into_zval(false).unwrap_or_default())
            } else {
                raw.into_zval(false).unwrap_or_default()
            }
        }
        "microtime" | "hrtime" => {
            if let Ok(n) = raw.parse::<f64>() {
                n.into_zval(false).unwrap_or_else(|_| raw.into_zval(false).unwrap_or_default())
            } else {
                raw.into_zval(false).unwrap_or_default()
            }
        }
        _ => raw.into_zval(false).unwrap_or_default(),
    };
    std::ptr::write(return_value, built);
}

#[cfg(feature = "zend-observer")]
macro_rules! define_hook {
    ($fn_name:ident, $symbol:expr, $kind:expr) => {
        unsafe extern "C" fn $fn_name(execute_data: *mut zend_execute_data, return_value: *mut zval) {
            if let Some(raw) = try_delegate($kind, $symbol) {
                write_scalar_return(return_value, $symbol, &raw);
                return;
            }
            call_original($symbol, execute_data, return_value);
        }
    };
}

#[cfg(feature = "zend-observer")]
define_hook!(hook_time, "time", "time");
#[cfg(feature = "zend-observer")]
define_hook!(hook_microtime, "microtime", "time");
#[cfg(feature = "zend-observer")]
define_hook!(hook_hrtime, "hrtime", "time");
#[cfg(feature = "zend-observer")]
define_hook!(hook_mt_rand, "mt_rand", "random");
#[cfg(feature = "zend-observer")]
define_hook!(hook_rand, "rand", "random");
#[cfg(feature = "zend-observer")]
define_hook!(hook_random_int, "random_int", "random");
#[cfg(feature = "zend-observer")]
define_hook!(hook_random_bytes, "random_bytes", "random");
#[cfg(feature = "zend-observer")]
define_hook!(hook_uniqid, "uniqid", "random");

#[cfg(feature = "zend-observer")]
unsafe extern "C" fn hook_getenv(execute_data: *mut zend_execute_data, return_value: *mut zval) {
    let name = crate::observer::zend_helpers::arg_scalar_string(execute_data, 0, 4096).unwrap_or_default();
    if let Some(raw) = try_delegate("env", &name) {
        write_scalar_return(return_value, "getenv", &raw);
        return;
    }
    call_original("getenv", execute_data, return_value);
}

#[cfg(test)]
mod tests {
    use super::kind_for_symbol;

    #[test]
    fn maps_scalar_symbols_to_effect_channels() {
        assert_eq!(kind_for_symbol("time"), Some("time"));
        assert_eq!(kind_for_symbol("mt_rand"), Some("random"));
        assert_eq!(kind_for_symbol("getenv"), Some("env"));
        assert_eq!(kind_for_symbol("strlen"), None);
    }
}
