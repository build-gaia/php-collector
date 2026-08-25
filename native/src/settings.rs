//! Unified configuration lookup: process env > INI (`chronos.*`) > `.chronos` file.
//!
//! Every configuration read in the crate goes through [`get`] (or the typed helpers)
//! so all three sources work uniformly. Sources are resolved ONCE per process and
//! cached — FPM workers inherit env and INI at fork, and a `.chronos` file is
//! deploy-time state, so nothing here is legitimately per-request.
//!
//! Naming: a setting has one canonical env name (`CHRONOS_PHP_APM_ENABLED`), from
//! which the other spellings derive by stripping the `CHRONOS_PHP_` / `CHRONOS_`
//! prefix and lowercasing: INI `chronos.apm_enabled`, `.chronos` file
//! `apm_enabled=...` (the full env spelling is also accepted there, so a `.chronos`
//! file can be `source`d as a dotenv without translation).
//!
//! The `.chronos` file is discovered by walking UP from the request's document
//! root / script directory / cwd until the filesystem root; the nearest file wins.
//! Format is dotenv-ish: `key=value` lines, `#` comments, optional `export `,
//! optional single/double quotes around the value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The full set of settings, by canonical env name. Also the INI registration list —
/// an unregistered `chronos.*` INI name can never be set from php.ini, so growing a
/// new setting means adding it here.
pub const SETTING_NAMES: &[&str] = &[
    "CHRONOS_PHP_ENABLED",
    "CHRONOS_PHP_CLI_ENABLED",
    "CHRONOS_PHP_ORGANISATION",
    "CHRONOS_PHP_PROJECT",
    "CHRONOS_PHP_APPLICATION",
    "CHRONOS_PHP_SPOOL_DIRECTORY",
    "CHRONOS_APP_VERSION",
    "CHRONOS_APP_COMMIT",
    "CHRONOS_APP_BRANCH",
    "CHRONOS_PHP_APP_LANGUAGE_VERSION",
    "CHRONOS_PHP_APP_FRAMEWORK",
    "CHRONOS_PHP_APP_FRAMEWORK_VERSION",
    "CHRONOS_PHP_APM_ENABLED",
    "CHRONOS_PHP_APM_SAMPLE_RATE",
    "CHRONOS_PHP_LOGS_ENABLED",
    "CHRONOS_PHP_PROFILER_ENABLED",
    "CHRONOS_PHP_PROFILE_SAMPLE_RATE",
    "CHRONOS_PHP_PROFILE_TYPES",
    "CHRONOS_PHP_PROFILE_SERIES_ID",
    "CHRONOS_PHP_PROFILE_IO_MIN_US",
    "CHRONOS_PHP_DST_ENABLED",
    "CHRONOS_PHP_ENV",
    "CHRONOS_PHP_DST_CALL_PATH_MAX",
    "CHRONOS_PHP_DST_CALL_PATH_MAX_DEPTH",
    "CHRONOS_PHP_RUNTIME_METRICS_ENABLED",
    "CHRONOS_PHP_LOCAL_RICH_TELEMETRY",
    "CHRONOS_PHP_SPAN_ALL_USERLAND",
    "CHRONOS_PHP_SPAN_MIN_DURATION_US",
    "CHRONOS_PHP_EXCLUDE_PATHS",
    "CHRONOS_PHP_HTTP_CAPTURE",
    "CHRONOS_PHP_HTTP_CAPTURE_BODIES",
    "CHRONOS_PHP_HTTP_CAPTURE_MAX_BODY",
    "CHRONOS_PHP_HTTP_CAPTURE_RESPONSE_BUFFER",
    "CHRONOS_PHP_HTTP_CAPTURE_REDACT",
    "CHRONOS_PHP_REDACT_PATTERNS",
    "CHRONOS_PHP_SPOOL_MAX_BYTES",
    "CHRONOS_PHP_INSTRUMENTATION_MANIFEST",
];

/// `CHRONOS_PHP_APM_ENABLED` -> `apm_enabled`; `CHRONOS_APP_VERSION` -> `app_version`.
pub fn short_key(env_name: &str) -> String {
    env_name
        .strip_prefix("CHRONOS_PHP_")
        .or_else(|| env_name.strip_prefix("CHRONOS_"))
        .unwrap_or(env_name)
        .to_ascii_lowercase()
}

/// INI directive name for a setting: `chronos.` + short key.
pub fn ini_name(env_name: &str) -> String {
    format!("chronos.{}", short_key(env_name))
}

struct Sources {
    ini: HashMap<String, String>,
    file: HashMap<String, String>,
}

static SOURCES: OnceLock<Sources> = OnceLock::new();

fn sources() -> &'static Sources {
    SOURCES.get_or_init(|| Sources {
        ini: read_ini(),
        file: read_chronos_file(),
    })
}

/// Resolve one setting by canonical env name: env > INI > `.chronos` file.
pub fn get(env_name: &str) -> Option<String> {
    if let Ok(value) = std::env::var(env_name) {
        if !value.is_empty() {
            return Some(value);
        }
    }
    let sources = sources();
    if let Some(value) = sources.ini.get(&ini_name(env_name)) {
        if !value.is_empty() {
            return Some(value.clone());
        }
    }
    let file = &sources.file;
    file.get(env_name)
        .or_else(|| file.get(&short_key(env_name)))
        .filter(|value| !value.is_empty())
        .cloned()
}

pub fn flag(env_name: &str, default: bool) -> bool {
    get(env_name)
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

pub fn u32_value(env_name: &str, default: u32) -> u32 {
    get(env_name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn u64_value(env_name: &str, default: u64) -> u64 {
    get(env_name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn string(env_name: &str, default: &str) -> String {
    get(env_name).unwrap_or_else(|| default.to_owned())
}

/// All registered `chronos.*` INI directives with non-empty values. INI entries are
/// registered by MINIT with empty defaults, so anything non-empty here was set by an
/// operator in php.ini / conf.d / FPM pool config.
fn read_ini() -> HashMap<String, String> {
    let globals = ext_php_rs::zend::ExecutorGlobals::get();
    globals
        .ini_values()
        .into_iter()
        .filter(|(name, _)| name.starts_with("chronos."))
        .filter_map(|(name, value)| value.map(|v| (name, v)))
        .collect()
}

/// Locate and parse the nearest `.chronos` file. Candidate start points cover the
/// three ways a PHP process knows where the application lives; each walks up so the
/// file can sit at the project root regardless of the docroot being `public/`.
fn read_chronos_file() -> HashMap<String, String> {
    let mut starts: Vec<PathBuf> = Vec::new();
    let server = crate::http_capture::server_vars();
    for key in ["DOCUMENT_ROOT", "SCRIPT_FILENAME"] {
        let value = crate::http_capture::lookup(&server, key);
        if !value.is_empty() {
            let path = Path::new(value);
            starts.push(if key == "SCRIPT_FILENAME" {
                path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.to_path_buf())
            } else {
                path.to_path_buf()
            });
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    for start in starts {
        let mut dir = Some(start.as_path());
        while let Some(current) = dir {
            let candidate = current.join(".chronos");
            if let Ok(body) = std::fs::read_to_string(&candidate) {
                return parse_chronos_file(&body);
            }
            dir = current.parent();
        }
    }
    HashMap::new()
}

fn parse_chronos_file(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        if key.is_empty() {
            continue;
        }
        // Stored under both an exact and a lowercased key so `ENABLED=1`,
        // `enabled=1` and `CHRONOS_PHP_ENABLED=1` all resolve.
        map.insert(key.to_owned(), value.to_owned());
        map.insert(key.to_ascii_lowercase(), value.to_owned());
    }
    map
}

/// Register every setting as a `chronos.*` INI directive (MINIT). Empty defaults:
/// "unset" must be distinguishable from "set to empty" so INI never masks env.
pub fn register_ini_entries(module_number: i32) {
    use ext_php_rs::flags::IniEntryPermission;
    use ext_php_rs::zend::IniEntryDef;
    let entries: Vec<IniEntryDef> = SETTING_NAMES
        .iter()
        .map(|name| IniEntryDef::new(ini_name(name), String::new(), &IniEntryPermission::All))
        .collect();
    IniEntryDef::register(entries, module_number);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_keys_strip_both_prefixes() {
        assert_eq!(short_key("CHRONOS_PHP_APM_ENABLED"), "apm_enabled");
        assert_eq!(short_key("CHRONOS_APP_VERSION"), "app_version");
        assert_eq!(ini_name("CHRONOS_PHP_ENABLED"), "chronos.enabled");
    }

    #[test]
    fn chronos_file_parses_dotenv_style() {
        let map = parse_chronos_file(
            "# identity\nCHRONOS_PHP_ORGANISATION=acme\nexport project = \"shop\"\napm_enabled='1'\n\nbroken line\n",
        );
        assert_eq!(map.get("CHRONOS_PHP_ORGANISATION").unwrap(), "acme");
        assert_eq!(map.get("project").unwrap(), "shop");
        assert_eq!(map.get("apm_enabled").unwrap(), "1");
        assert!(!map.contains_key("broken line"));
    }
}
