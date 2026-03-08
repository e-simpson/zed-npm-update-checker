use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

pub const SETTINGS_SERVER_KEY: &str = "npm-package-json-checker-lsp";
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.npmjs.org";
pub const SETTINGS_ENV_VAR: &str = "NPM_PACKAGE_JSON_CHECKER_SETTINGS";
pub const INITIALIZATION_OPTIONS_ENV_VAR: &str = "NPM_PACKAGE_JSON_CHECKER_INITIALIZATION_OPTIONS";
const DEFAULT_CACHE_TTL_SECONDS: u64 = 300;
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 10;
const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 15;
const DEFAULT_RECENT_RELEASES_IN_CODE_ACTIONS: usize = 2;
const DEFAULT_DATE_FORMAT: &str = "%d/%m/%Y";

const KNOWN_SETTING_KEYS: [&str; 8] = [
    "registry_url",
    "cache_ttl_seconds",
    "max_concurrent_requests",
    "request_timeout_seconds",
    "show_experimental_tracks",
    "recent_releases_in_code_actions",
    "date_tag_mode",
    "date_format",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTagMode {
    Date,
    TimeAgo,
    DateAndTimeAgo,
}

impl Default for DateTagMode {
    fn default() -> Self {
        Self::DateAndTimeAgo
    }
}

impl DateTagMode {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "date" => Self::Date,
            "timeago" => Self::TimeAgo,
            "date+timeago" => Self::DateAndTimeAgo,
            _ => Self::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateDisplaySettings {
    pub mode: DateTagMode,
    pub format: String,
}

impl Default for DateDisplaySettings {
    fn default() -> Self {
        Self {
            mode: DateTagMode::default(),
            format: DEFAULT_DATE_FORMAT.to_string(),
        }
    }
}

impl DateDisplaySettings {
    pub fn format_date_tag(&self, date: DateTime<Utc>) -> String {
        let date_str = date.format(&self.format).to_string();
        let ago = format_time_ago(date);

        match self.mode {
            DateTagMode::Date => date_str,
            DateTagMode::TimeAgo => ago,
            DateTagMode::DateAndTimeAgo => format!("{}, {}", date_str, ago),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSettings {
    pub registry_url: String,
    pub cache_ttl_seconds: u64,
    pub max_concurrent_requests: usize,
    pub request_timeout_seconds: u64,
    pub show_experimental_tracks: bool,
    pub recent_releases_in_code_actions: usize,
    pub date_display: DateDisplaySettings,
}

impl Default for ExtensionSettings {
    fn default() -> Self {
        Self {
            registry_url: DEFAULT_REGISTRY_URL.to_string(),
            cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
            show_experimental_tracks: false,
            recent_releases_in_code_actions: DEFAULT_RECENT_RELEASES_IN_CODE_ACTIONS,
            date_display: DateDisplaySettings::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ExtensionSettingsPatch {
    registry_url: Option<String>,
    cache_ttl_seconds: Option<u64>,
    max_concurrent_requests: Option<usize>,
    request_timeout_seconds: Option<u64>,
    show_experimental_tracks: Option<bool>,
    recent_releases_in_code_actions: Option<usize>,
    date_tag_mode: Option<DateTagMode>,
    date_format: Option<String>,
}

impl ExtensionSettingsPatch {
    pub fn from_value(value: &Value) -> Option<Self> {
        let settings_object = find_settings_object(value)?;

        Some(Self {
            registry_url: settings_object
                .get("registry_url")
                .and_then(Value::as_str)
                .map(normalize_registry_url),
            cache_ttl_seconds: settings_object.get("cache_ttl_seconds").and_then(read_u64),
            max_concurrent_requests: settings_object
                .get("max_concurrent_requests")
                .and_then(read_usize),
            request_timeout_seconds: settings_object
                .get("request_timeout_seconds")
                .and_then(read_u64),
            show_experimental_tracks: settings_object
                .get("show_experimental_tracks")
                .and_then(Value::as_bool),
            recent_releases_in_code_actions: settings_object
                .get("recent_releases_in_code_actions")
                .and_then(read_usize),
            date_tag_mode: settings_object
                .get("date_tag_mode")
                .and_then(Value::as_str)
                .map(DateTagMode::parse),
            date_format: settings_object
                .get("date_format")
                .and_then(Value::as_str)
                .map(normalize_date_format),
        })
    }
}

impl ExtensionSettings {
    pub fn from_env_or_default() -> Self {
        let mut settings = Self::default();

        if let Ok(raw) = std::env::var(SETTINGS_ENV_VAR) {
            if let Some(patch) = patch_from_json_string(&raw) {
                settings.apply_patch(patch);
            }
        }

        if let Ok(raw) = std::env::var(INITIALIZATION_OPTIONS_ENV_VAR) {
            if let Some(patch) = patch_from_json_string(&raw) {
                settings.apply_patch(patch);
            }
        }

        settings
    }

    pub fn from_sources_or_default(initialization_options: Option<&Value>) -> Self {
        let mut settings = Self::from_env_or_default();

        if let Some(value) = initialization_options {
            if let Some(patch) = ExtensionSettingsPatch::from_value(value) {
                settings.apply_patch(patch);
            }
        }

        settings
    }

    pub fn from_value_or_default(value: Option<&Value>) -> Self {
        let mut settings = Self::default();

        if let Some(value) = value {
            if let Some(patch) = ExtensionSettingsPatch::from_value(value) {
                settings.apply_patch(patch);
            }
        }

        settings
    }

    pub fn apply_patch(&mut self, patch: ExtensionSettingsPatch) {
        if let Some(registry_url) = patch.registry_url {
            self.registry_url = registry_url;
        }

        if let Some(cache_ttl_seconds) = patch.cache_ttl_seconds {
            self.cache_ttl_seconds = cache_ttl_seconds;
        }

        if let Some(max_concurrent_requests) = patch.max_concurrent_requests {
            self.max_concurrent_requests = max_concurrent_requests.max(1);
        }

        if let Some(request_timeout_seconds) = patch.request_timeout_seconds {
            self.request_timeout_seconds = request_timeout_seconds.max(1);
        }

        if let Some(show_experimental_tracks) = patch.show_experimental_tracks {
            self.show_experimental_tracks = show_experimental_tracks;
        }

        if let Some(recent_releases_in_code_actions) = patch.recent_releases_in_code_actions {
            self.recent_releases_in_code_actions = recent_releases_in_code_actions;
        }

        if let Some(date_tag_mode) = patch.date_tag_mode {
            self.date_display.mode = date_tag_mode;
        }

        if let Some(date_format) = patch.date_format {
            self.date_display.format = date_format;
        }
    }
}

fn patch_from_json_string(raw: &str) -> Option<ExtensionSettingsPatch> {
    let value: Value = serde_json::from_str(raw).ok()?;
    ExtensionSettingsPatch::from_value(&value)
}

pub fn format_time_ago(date: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(date);

    if duration.num_days() > 365 {
        let years = duration.num_days() / 365;
        format!("{} year{} ago", years, if years == 1 { "" } else { "s" })
    } else if duration.num_days() > 30 {
        let months = duration.num_days() / 30;
        format!("{} month{} ago", months, if months == 1 { "" } else { "s" })
    } else if duration.num_days() > 7 {
        let weeks = duration.num_days() / 7;
        format!("{} week{} ago", weeks, if weeks == 1 { "" } else { "s" })
    } else if duration.num_days() > 0 {
        format!(
            "{} day{} ago",
            duration.num_days(),
            if duration.num_days() == 1 { "" } else { "s" }
        )
    } else if duration.num_hours() > 0 {
        format!(
            "{} hour{} ago",
            duration.num_hours(),
            if duration.num_hours() == 1 { "" } else { "s" }
        )
    } else if duration.num_minutes() > 0 {
        format!(
            "{} minute{} ago",
            duration.num_minutes(),
            if duration.num_minutes() == 1 { "" } else { "s" }
        )
    } else {
        "just now".to_string()
    }
}

fn find_settings_object<'a>(value: &'a Value) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;

    if is_settings_object(object) {
        return Some(object);
    }

    if let Some(settings) = object.get("settings").and_then(Value::as_object) {
        if is_settings_object(settings) {
            return Some(settings);
        }
    }

    if let Some(server) = object
        .get("lsp")
        .and_then(Value::as_object)
        .and_then(|lsp| lsp.get(SETTINGS_SERVER_KEY))
        .and_then(Value::as_object)
    {
        if is_settings_object(server) {
            return Some(server);
        }

        if let Some(settings) = server.get("settings").and_then(Value::as_object) {
            if is_settings_object(settings) {
                return Some(settings);
            }
        }
    }

    if let Some(server) = object.get(SETTINGS_SERVER_KEY).and_then(Value::as_object) {
        if is_settings_object(server) {
            return Some(server);
        }

        if let Some(settings) = server.get("settings").and_then(Value::as_object) {
            if is_settings_object(settings) {
                return Some(settings);
            }
        }
    }

    None
}

fn is_settings_object(object: &Map<String, Value>) -> bool {
    KNOWN_SETTING_KEYS
        .iter()
        .any(|key| object.contains_key(*key))
}

fn read_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().map(|value| value.max(0) as u64))
}

fn read_usize(value: &Value) -> Option<usize> {
    read_u64(value).and_then(|value| usize::try_from(value).ok())
}

fn normalize_registry_url(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');

    if trimmed.is_empty() {
        return DEFAULT_REGISTRY_URL.to_string();
    }

    if reqwest::Url::parse(trimmed).is_ok() {
        trimmed.to_string()
    } else {
        DEFAULT_REGISTRY_URL.to_string()
    }
}

fn normalize_date_format(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        DEFAULT_DATE_FORMAT.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn utc_date(input: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(input)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parses_defaults_when_missing() {
        let settings = ExtensionSettings::from_value_or_default(None);

        assert_eq!(settings.registry_url, DEFAULT_REGISTRY_URL);
        assert_eq!(settings.cache_ttl_seconds, DEFAULT_CACHE_TTL_SECONDS);
        assert_eq!(
            settings.max_concurrent_requests,
            DEFAULT_MAX_CONCURRENT_REQUESTS
        );
        assert_eq!(
            settings.request_timeout_seconds,
            DEFAULT_REQUEST_TIMEOUT_SECONDS
        );
        assert!(!settings.show_experimental_tracks);
        assert_eq!(
            settings.recent_releases_in_code_actions,
            DEFAULT_RECENT_RELEASES_IN_CODE_ACTIONS
        );
        assert_eq!(settings.date_display.mode, DateTagMode::DateAndTimeAgo);
        assert_eq!(settings.date_display.format, DEFAULT_DATE_FORMAT);
    }

    #[test]
    fn parses_nested_lsp_shape() {
        let value = json!({
            "lsp": {
                "npm-package-json-checker-lsp": {
                    "settings": {
                        "registry_url": "https://registry.company.test/",
                        "cache_ttl_seconds": 12,
                        "max_concurrent_requests": 6,
                        "request_timeout_seconds": 20,
                        "show_experimental_tracks": true,
                        "recent_releases_in_code_actions": 4,
                        "date_tag_mode": "timeago",
                        "date_format": "%Y-%m-%d"
                    }
                }
            }
        });

        let settings = ExtensionSettings::from_value_or_default(Some(&value));

        assert_eq!(settings.registry_url, "https://registry.company.test");
        assert_eq!(settings.cache_ttl_seconds, 12);
        assert_eq!(settings.max_concurrent_requests, 6);
        assert_eq!(settings.request_timeout_seconds, 20);
        assert!(settings.show_experimental_tracks);
        assert_eq!(settings.recent_releases_in_code_actions, 4);
        assert_eq!(settings.date_display.mode, DateTagMode::TimeAgo);
        assert_eq!(settings.date_display.format, "%Y-%m-%d");
    }

    #[test]
    fn clamps_and_falls_back_on_invalid_values() {
        let value = json!({
            "settings": {
                "registry_url": "not_a_url",
                "cache_ttl_seconds": -5,
                "max_concurrent_requests": 0,
                "request_timeout_seconds": 0,
                "recent_releases_in_code_actions": -1,
                "date_tag_mode": "invalid",
                "date_format": ""
            }
        });

        let settings = ExtensionSettings::from_value_or_default(Some(&value));

        assert_eq!(settings.registry_url, DEFAULT_REGISTRY_URL);
        assert_eq!(settings.cache_ttl_seconds, 0);
        assert_eq!(settings.max_concurrent_requests, 1);
        assert_eq!(settings.request_timeout_seconds, 1);
        assert_eq!(settings.recent_releases_in_code_actions, 0);
        assert_eq!(settings.date_display.mode, DateTagMode::DateAndTimeAgo);
        assert_eq!(settings.date_display.format, DEFAULT_DATE_FORMAT);
    }

    #[test]
    fn applies_partial_patch() {
        let mut settings = ExtensionSettings::default();
        let patch = ExtensionSettingsPatch::from_value(&json!({
            "settings": {
                "max_concurrent_requests": 5,
                "show_experimental_tracks": true
            }
        }))
        .unwrap();

        settings.apply_patch(patch);

        assert_eq!(settings.max_concurrent_requests, 5);
        assert!(settings.show_experimental_tracks);
        assert_eq!(settings.cache_ttl_seconds, DEFAULT_CACHE_TTL_SECONDS);
    }

    #[test]
    fn formats_date_tag_modes() {
        let date = utc_date("2026-01-15T00:00:00Z");

        let date_only = DateDisplaySettings {
            mode: DateTagMode::Date,
            format: "%d/%m/%Y".to_string(),
        }
        .format_date_tag(date);

        let ago_only = DateDisplaySettings {
            mode: DateTagMode::TimeAgo,
            format: "%d/%m/%Y".to_string(),
        }
        .format_date_tag(date);

        let combined = DateDisplaySettings::default().format_date_tag(date);

        assert_eq!(date_only, "15/01/2026");
        assert!(ago_only.contains("ago") || ago_only == "just now");
        assert!(combined.starts_with("15/01/2026"));
    }
}
