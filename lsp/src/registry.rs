use chrono::{DateTime, Utc};
use dashmap::DashMap;
use regex::Regex;
use semver::Version;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};
use tracing::debug;

use crate::settings::{DEFAULT_REGISTRY_URL, DateDisplaySettings};

const MAX_CHANGELOG_VERSIONS: usize = 15;

static VERSION_HEADER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^#{1,2}\s*\[?v?(\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?)\]?")
        .expect("valid version-header regex")
});
static HEADER_DATE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?ix)\b\d{4}-\d{1,2}-\d{1,2}\b|\b\d{1,2}/\d{1,2}/\d{4}\b|\b(?:jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?\s+\d{1,2}(?:st|nd|rd|th)?(?:,\s*|\s+)\d{4}\b")
        .expect("valid changelog-date regex")
});
static VERSION_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^v|@|^)(\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?)$")
        .expect("valid version-tag regex")
});
static VERSION_ANYWHERE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"v?(\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?)").expect("valid embedded-version regex")
});
static REVISION_TAG_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^r(\d+)$").expect("valid revision-tag regex"));
static ATOM_ENTRY_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<entry>.*?<title>([^<]+)</title>.*?<content[^>]*>(.*?)</content>.*?</entry>")
        .expect("valid Atom-entry regex")
});

/// Severity of version update
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSeverity {
    /// Major version bump (breaking changes) - e.g., 1.x.x -> 2.x.x
    Major,
    /// Minor version bump (new features) - e.g., 1.1.x -> 1.2.x
    Minor,
    /// Patch version bump (bug fixes) - e.g., 1.1.1 -> 1.1.2
    Patch,
}

impl UpdateSeverity {
    /// Get a descriptive label
    pub fn label(&self) -> &'static str {
        match self {
            UpdateSeverity::Major => "MAJOR",
            UpdateSeverity::Minor => "MINOR",
            UpdateSeverity::Patch => "PATCH",
        }
    }
}

/// Map prerelease channel names to display labels
fn prerelease_channel_label(channel: &str) -> Option<&'static str> {
    match channel {
        "alpha" => Some("ALPHA"),
        "beta" => Some("BETA"),
        "canary" => Some("CANARY"),
        "nightly" => Some("NIGHTLY"),
        "rc" | "candidate" => Some("CANDIDATE"),
        "experimental" | "exp" => Some("EXPERIMENTAL"),
        _ => None,
    }
}

/// Extract first prerelease channel token from a version string
fn prerelease_channel(version: &str) -> Option<String> {
    if let Ok(parsed) = Version::parse(version) {
        let pre = parsed.pre.as_str();
        if pre.is_empty() {
            return None;
        }

        let token = pre
            .split(['.', '-'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();

        if token.is_empty() { None } else { Some(token) }
    } else if version.contains('-') {
        let token = version
            .split('-')
            .nth(1)
            .unwrap_or("")
            .split(['.', '-'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();

        if token.is_empty() { None } else { Some(token) }
    } else {
        None
    }
}

/// Return display label for prerelease channel, if known
pub fn prerelease_track_label(version: &str) -> Option<&'static str> {
    prerelease_channel(version)
        .as_deref()
        .and_then(prerelease_channel_label)
}

/// Information about a specific track
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub name: String,
    pub version: String,
    pub release_date: Option<DateTime<Utc>>,
}

/// Information about an alternative track update
#[derive(Debug, Clone, PartialEq)]
pub struct TrackUpdate {
    pub name: String,
    pub version: String,
    pub release_date: Option<DateTime<Utc>>,
    pub is_newer: bool,
}

/// Release metadata for a specific version
#[derive(Debug, Clone, PartialEq)]
pub struct VersionRelease {
    pub version: String,
    pub release_date: DateTime<Utc>,
}

/// Result of comparing current version to latest
#[derive(Debug, Clone, PartialEq)]
pub enum VersionStatus {
    /// Package is up to date
    UpToDate {
        current_track: String,
        current_version: String,
        current_track_release_date: Option<DateTime<Utc>>,
        recent_current_track_releases: Vec<VersionRelease>,
        other_tracks: Vec<TrackUpdate>,
        changelog: Option<String>,
        repository_url: Option<String>,
    },
    /// Update available with severity, changelog, and repository URL
    UpdateAvailable {
        current_track: String,
        current_version: String,
        current_track_release_date: Option<DateTime<Utc>>,
        recent_current_track_releases: Vec<VersionRelease>,
        latest_on_track: String,
        other_tracks: Vec<TrackUpdate>,
        severity: UpdateSeverity,
        changelog: Option<String>,
        repository_url: Option<String>,
    },
    /// Could not determine (invalid version, fetch failed, etc.)
    Unknown {
        current_track: String,
        current_version: String,
        current_track_release_date: Option<DateTime<Utc>>,
        recent_current_track_releases: Vec<VersionRelease>,
        other_tracks: Vec<TrackUpdate>,
        changelog: Option<String>,
        repository_url: Option<String>,
    },
}

/// A single changelog entry with version and content
#[derive(Debug, Clone)]
struct ChangelogEntry {
    version: Version,
    version_string: String,
    body: String,
    release_date: Option<DateTime<Utc>>,
}

/// Cache key includes both package name and current version for changelog relevance
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    package_name: String,
    current_version: String,
    latest_version: String,
}

#[derive(Debug, Clone)]
struct CachedChangelog {
    changelog: Option<String>,
    fetched_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedPackageMetadata {
    dist_tags: DistTags,
    version_publish_dates: HashMap<String, DateTime<Utc>>,
    repository_url: Option<String>,
    repository_directory: Option<String>,
    fetched_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedHttpText {
    content: Option<Arc<str>>,
    fetched_at: Instant,
}

/// Lightweight package info without changelog (for fast initial display)
#[derive(Debug, Clone)]
pub struct PackageVersionInfo {
    pub current_track: String,
    pub latest_on_track: String,
    pub version_publish_dates: HashMap<String, DateTime<Utc>>,
    pub recent_current_track_releases: Vec<VersionRelease>,
    pub all_tracks: Vec<TrackInfo>,
    pub repository_url: Option<String>,
    pub repository_directory: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NpmPackageResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: Option<DistTags>,
    repository: Option<Repository>,
    time: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DistTags {
    latest: Option<String>,
    #[serde(flatten)]
    other_tags: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Repository {
    String(String),
    Object {
        url: Option<String>,
        #[serde(rename = "type")]
        _repo_type: Option<String>,
        directory: Option<String>,
    },
}

impl Repository {
    fn get_url(&self) -> Option<String> {
        match self {
            Repository::String(s) => Some(s.clone()),
            Repository::Object { url, .. } => url.clone(),
        }
    }

    fn get_directory(&self) -> Option<String> {
        match self {
            Repository::String(_) => None,
            Repository::Object { directory, .. } => directory.clone(),
        }
    }
}

pub struct NpmRegistry {
    client: Arc<RwLock<reqwest::Client>>,
    changelog_cache: Arc<DashMap<CacheKey, CachedChangelog>>,
    metadata_cache: Arc<DashMap<String, CachedPackageMetadata>>,
    http_text_cache: Arc<DashMap<String, CachedHttpText>>,
    package_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    changelog_locks: Arc<DashMap<CacheKey, Arc<Mutex<()>>>>,
    http_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    registry_semaphore: Arc<RwLock<Arc<Semaphore>>>,
    changelog_semaphore: Arc<RwLock<Arc<Semaphore>>>,
    config: Arc<RwLock<RegistryConfig>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryConfig {
    pub registry_url: String,
    pub cache_ttl: Duration,
    pub max_concurrent_requests: usize,
    pub max_concurrent_changelog_requests: usize,
    pub request_timeout: Duration,
    pub date_display: DateDisplaySettings,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            registry_url: DEFAULT_REGISTRY_URL.to_string(),
            cache_ttl: Duration::from_secs(300),
            max_concurrent_requests: 10,
            max_concurrent_changelog_requests: 4,
            request_timeout: Duration::from_secs(15),
            date_display: DateDisplaySettings::default(),
        }
    }
}

impl NpmRegistry {
    pub fn new(config: RegistryConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .user_agent("npm-package-json-checker-lsp")
            .build()
            .unwrap_or_default();

        Self {
            client: Arc::new(RwLock::new(client)),
            changelog_cache: Arc::new(DashMap::new()),
            metadata_cache: Arc::new(DashMap::new()),
            http_text_cache: Arc::new(DashMap::new()),
            package_locks: Arc::new(DashMap::new()),
            changelog_locks: Arc::new(DashMap::new()),
            http_locks: Arc::new(DashMap::new()),
            registry_semaphore: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                config.max_concurrent_requests,
            )))),
            changelog_semaphore: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                config.max_concurrent_changelog_requests,
            )))),
            config: Arc::new(RwLock::new(config)),
        }
    }

    pub fn apply_config(&self, new_config: RegistryConfig) {
        let mut config = self.config.write().expect("registry config lock poisoned");
        let old_config = config.clone();

        if old_config.request_timeout != new_config.request_timeout {
            let client = reqwest::Client::builder()
                .timeout(new_config.request_timeout)
                .user_agent("npm-package-json-checker-lsp")
                .build()
                .unwrap_or_default();

            *self.client.write().expect("registry client lock poisoned") = client;
        }

        if old_config.max_concurrent_requests != new_config.max_concurrent_requests {
            *self
                .registry_semaphore
                .write()
                .expect("registry semaphore lock poisoned") =
                Arc::new(Semaphore::new(new_config.max_concurrent_requests));
        }

        if old_config.max_concurrent_changelog_requests
            != new_config.max_concurrent_changelog_requests
        {
            *self
                .changelog_semaphore
                .write()
                .expect("changelog semaphore lock poisoned") =
                Arc::new(Semaphore::new(new_config.max_concurrent_changelog_requests));
        }

        if old_config.registry_url != new_config.registry_url {
            self.metadata_cache.clear();
            self.changelog_cache.clear();
        }

        if old_config.date_display != new_config.date_display {
            self.changelog_cache.clear();
        }

        *config = new_config;
    }

    pub fn config(&self) -> RegistryConfig {
        self.config
            .read()
            .expect("registry config lock poisoned")
            .clone()
    }

    fn client(&self) -> reqwest::Client {
        self.client
            .read()
            .expect("registry client lock poisoned")
            .clone()
    }

    fn registry_semaphore(&self) -> Arc<Semaphore> {
        self.registry_semaphore
            .read()
            .expect("registry semaphore lock poisoned")
            .clone()
    }

    fn changelog_semaphore(&self) -> Arc<Semaphore> {
        self.changelog_semaphore
            .read()
            .expect("changelog semaphore lock poisoned")
            .clone()
    }

    async fn fetch_http_text(&self, url: &str) -> Option<Arc<str>> {
        let config = self.config();
        if let Some(cached) = self.http_text_cache.get(url) {
            if cached.fetched_at.elapsed() < config.cache_ttl {
                return cached.content.clone();
            }
        }

        let request_lock = self
            .http_locks
            .entry(url.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _singleflight = request_lock.lock().await;

        if let Some(cached) = self.http_text_cache.get(url) {
            if cached.fetched_at.elapsed() < config.cache_ttl {
                return cached.content.clone();
            }
        }

        let semaphore = self.changelog_semaphore();
        let content = if let Ok(_permit) = semaphore.acquire().await {
            match self.client().get(url).send().await {
                Ok(response) if response.status().is_success() => {
                    response.text().await.ok().map(Arc::<str>::from)
                }
                Ok(response) => {
                    debug!(
                        "Changelog source returned {} for {}",
                        response.status(),
                        url
                    );
                    None
                }
                Err(error) => {
                    debug!("Failed to fetch changelog source {}: {}", url, error);
                    None
                }
            }
        } else {
            None
        };

        self.http_text_cache.insert(
            url.to_string(),
            CachedHttpText {
                content: content.clone(),
                fetched_at: Instant::now(),
            },
        );
        content
    }

    /// Fast version check - only fetches npm registry, no GitHub API calls
    /// Use this for initial display of updates, then fetch changelogs separately
    pub async fn get_package_version_info(
        &self,
        package_name: &str,
        current_version: &str,
    ) -> Option<PackageVersionInfo> {
        let metadata = self.get_package_metadata(package_name).await?;
        let current_track = detect_track(current_version, &metadata.dist_tags);
        let latest_on_track = get_version_for_track(&metadata.dist_tags, &current_track)?;
        let recent_current_track_releases = build_recent_current_track_releases(
            &current_track,
            current_version,
            &latest_on_track,
            &metadata.version_publish_dates,
        );
        let all_tracks =
            build_track_info(&metadata.dist_tags, Some(&metadata.version_publish_dates));

        Some(PackageVersionInfo {
            current_track,
            latest_on_track,
            version_publish_dates: metadata.version_publish_dates,
            recent_current_track_releases,
            all_tracks,
            repository_url: metadata.repository_url,
            repository_directory: metadata.repository_directory,
        })
    }

    async fn get_package_metadata(&self, package_name: &str) -> Option<CachedPackageMetadata> {
        let config = self.config();
        if let Some(cached) = self.metadata_cache.get(package_name) {
            if cached.fetched_at.elapsed() < config.cache_ttl {
                return Some(cached.clone());
            }
        }

        let package_lock = self
            .package_locks
            .entry(package_name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _singleflight = package_lock.lock().await;

        if let Some(cached) = self.metadata_cache.get(package_name) {
            if cached.fetched_at.elapsed() < config.cache_ttl {
                return Some(cached.clone());
            }
        }

        let semaphore = self.registry_semaphore();
        let _permit = semaphore.acquire().await.ok()?;
        let url = package_registry_url(&config.registry_url, package_name);
        let response = match self.client().get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                debug!(
                    "Registry returned {} for {}",
                    response.status(),
                    package_name
                );
                return None;
            }
            Err(error) => {
                debug!("Failed to fetch {}: {}", package_name, error);
                return None;
            }
        };

        let data: NpmPackageResponse = match response.json().await {
            Ok(data) => data,
            Err(error) => {
                debug!(
                    "Failed to parse registry response for {}: {}",
                    package_name, error
                );
                return None;
            }
        };
        let metadata = CachedPackageMetadata {
            dist_tags: data.dist_tags?,
            version_publish_dates: parse_version_publish_dates(data.time.as_ref()),
            repository_url: data.repository.as_ref().and_then(Repository::get_url),
            repository_directory: data.repository.as_ref().and_then(Repository::get_directory),
            fetched_at: Instant::now(),
        };
        self.metadata_cache
            .insert(package_name.to_string(), metadata.clone());
        Some(metadata)
    }

    /// Fetch changelog for a package (makes GitHub API calls - rate limited!)
    /// Call this after get_package_version_info, with delays between calls
    pub async fn fetch_changelog_for_package(
        &self,
        package_name: &str,
        current_version: &str,
        latest_version: &str,
        repo_url: &str,
        repo_directory: Option<&str>,
        version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
    ) -> Option<String> {
        let config = self.config();
        let cache_key = CacheKey {
            package_name: package_name.to_string(),
            current_version: current_version.to_string(),
            latest_version: latest_version.to_string(),
        };

        if let Some(cached) = self.changelog_cache.get(&cache_key) {
            if cached.fetched_at.elapsed() < config.cache_ttl {
                return cached.changelog.clone();
            }
        }

        let changelog_lock = self
            .changelog_locks
            .entry(cache_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _singleflight = changelog_lock.lock().await;

        if let Some(cached) = self.changelog_cache.get(&cache_key) {
            if cached.fetched_at.elapsed() < config.cache_ttl {
                return cached.changelog.clone();
            }
        }

        // Fetch changelog from GitHub
        let changelog = self
            .fetch_changelog(
                repo_url,
                repo_directory,
                current_version,
                latest_version,
                version_publish_dates,
                &config.date_display,
            )
            .await;

        self.changelog_cache.insert(
            cache_key,
            CachedChangelog {
                changelog: changelog.clone(),
                fetched_at: Instant::now(),
            },
        );

        changelog
    }

    /// Fetch changelog from GitHub releases and CHANGELOG.md, merge both sources
    async fn fetch_changelog(
        &self,
        repo_url: &str,
        directory: Option<&str>,
        current_version: &str,
        latest_version: &str,
        version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
        date_display: &DateDisplaySettings,
    ) -> Option<String> {
        let (owner, repo) = match parse_github_url(repo_url) {
            Some(parsed) => parsed,
            None => {
                debug!("Failed to parse GitHub URL: {}", repo_url);
                return None;
            }
        };

        debug!(
            "Fetching changelog for {}/{} (dir: {:?}): {} -> {}",
            owner, repo, directory, current_version, latest_version
        );

        // Try package-specific changelog first if in monorepo
        let monorepo_changelog = if let Some(dir) = directory {
            self.fetch_monorepo_changelog(
                &owner,
                &repo,
                dir,
                (current_version, latest_version),
                version_publish_dates,
                date_display,
            )
            .await
        } else {
            None
        };

        // Fetch root changelog as fallback
        let (releases_result, root_changelog_result) = tokio::join!(
            self.fetch_github_releases(
                &owner,
                &repo,
                current_version,
                latest_version,
                directory,
                version_publish_dates
            ),
            self.fetch_changelog_file(
                &owner,
                &repo,
                current_version,
                latest_version,
                version_publish_dates
            )
        );

        debug!(
            "GitHub releases found: {}, CHANGELOG.md sections found: {}",
            releases_result.len(),
            root_changelog_result.len()
        );

        // Merge all sources
        let merged = if let Some(ref mono_cl) = monorepo_changelog {
            // Use monorepo changelog if available
            mono_cl.clone()
        } else {
            merge_changelogs(
                releases_result,
                root_changelog_result,
                current_version,
                latest_version,
                date_display,
            )
        };

        if merged.is_empty() {
            debug!("No changelog content after merge for {}/{}", owner, repo);
            None
        } else {
            debug!(
                "Changelog content generated for {}/{}: {} chars",
                owner,
                repo,
                merged.len()
            );
            Some(merged)
        }
    }

    /// Fetch changelog from monorepo package directory
    async fn fetch_monorepo_changelog(
        &self,
        owner: &str,
        repo: &str,
        directory: &str,
        versions: (&str, &str),
        version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
        date_display: &DateDisplaySettings,
    ) -> Option<String> {
        let (current_version, latest_version) = versions;
        let files = ["CHANGELOG.md", "changelog.md", "HISTORY.md", "CHANGES.md"];
        // GitHub resolves HEAD to the repository's default branch, avoiding
        // duplicate main/master probes.
        let branches = ["HEAD"];

        // Try package-specific changelog paths
        let paths = [
            format!("packages/{}/{}", directory, files[0]),
            format!("packages/{}/{}", directory, files[1]),
            format!("packages/{}/{}", directory, files[2]),
            format!("packages/{}/{}", directory, files[3]),
            format!("{}/{}", directory, files[0]),
            format!("{}/{}", directory, files[1]),
            format!("{}/{}", directory, files[2]),
            format!("{}/{}", directory, files[3]),
        ];

        for path in &paths {
            for branch in &branches {
                let url = format!(
                    "https://raw.githubusercontent.com/{}/{}/{}/{}",
                    owner, repo, branch, path
                );

                if let Some(content) = self.fetch_http_text(&url).await {
                    let entries = extract_changelog_sections_since_version(
                        &content,
                        current_version,
                        latest_version,
                        version_publish_dates,
                    );
                    if !entries.is_empty() {
                        debug!("Found monorepo changelog at {}", url);
                        return Some(format_changelog_entries(entries, date_display));
                    }
                }
            }
        }

        // Try GitHub releases with package-specific tags
        let package_name = directory.split('/').next_back().unwrap_or(directory);
        let releases = self
            .fetch_github_releases_with_prefix(
                owner,
                repo,
                current_version,
                latest_version,
                package_name,
                version_publish_dates,
            )
            .await;

        if !releases.is_empty() {
            return Some(format_changelog_entries(releases, date_display));
        }

        None
    }

    /// Fetch GitHub releases with package name prefix (for monorepo tags like "package@version")
    async fn fetch_github_releases_with_prefix(
        &self,
        owner: &str,
        repo: &str,
        current_version: &str,
        latest_version: &str,
        package_prefix: &str,
        version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
    ) -> Vec<ChangelogEntry> {
        let current_semver = Version::parse(current_version).ok();
        let latest_semver = Version::parse(latest_version).ok();
        let use_version_filtering = current_semver.is_some() && latest_semver.is_some();

        let url = format!("https://github.com/{}/{}/releases.atom", owner, repo);

        let Some(content) = self.fetch_http_text(&url).await else {
            return vec![];
        };

        let mut entries = Vec::new();
        let prefix_patterns = [
            format!("{}@", package_prefix),
            format!("@scope/{}@", package_prefix), // Scoped packages
        ];

        for cap in ATOM_ENTRY_REGEX.captures_iter(&content) {
            let title = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let html_content = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let body = html_to_text(html_content);

            if body.trim().is_empty() {
                continue;
            }

            // Check if title starts with package prefix
            let matches_prefix = prefix_patterns.iter().any(|p| title.starts_with(p));
            if !matches_prefix {
                continue;
            }

            if let Some(version) = parse_version_from_tag(title) {
                if use_version_filtering {
                    if let (Some(current), Some(latest)) = (&current_semver, &latest_semver) {
                        if version > *current && version <= *latest {
                            entries.push(ChangelogEntry {
                                release_date: release_date_for_version(
                                    &version,
                                    version_publish_dates,
                                ),
                                version,
                                version_string: title.to_string(),
                                body,
                            });
                        }
                    }
                } else {
                    entries.push(ChangelogEntry {
                        release_date: release_date_for_version(&version, version_publish_dates),
                        version,
                        version_string: title.to_string(),
                        body,
                    });
                }

                if entries.len() >= MAX_CHANGELOG_VERSIONS {
                    break;
                }
            }
        }

        entries.sort_by(|a, b| b.version.cmp(&a.version));
        entries
    }

    /// Fetch multiple release notes from GitHub releases atom feed (no API rate limits!)
    async fn fetch_github_releases(
        &self,
        owner: &str,
        repo: &str,
        current_version: &str,
        latest_version: &str,
        _directory: Option<&str>,
        version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
    ) -> Vec<ChangelogEntry> {
        // Try to parse current and latest versions (may fail for non-semver like r182)
        let current_semver = Version::parse(current_version).ok();
        let latest_semver = Version::parse(latest_version).ok();

        let use_version_filtering = current_semver.is_some() && latest_semver.is_some();

        // Use the atom feed instead of API - no rate limits!
        let url = format!("https://github.com/{}/{}/releases.atom", owner, repo);

        debug!("Fetching GitHub releases atom feed for {}/{}", owner, repo);

        let Some(content) = self.fetch_http_text(&url).await else {
            return vec![];
        };

        let mut entries = Vec::new();
        let mut fallback_entries = Vec::new(); // For when version filtering yields nothing

        // Parse atom feed entries using regex
        for cap in ATOM_ENTRY_REGEX.captures_iter(&content) {
            let title = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let html_content = cap.get(2).map(|m| m.as_str()).unwrap_or("");

            // Convert HTML content to markdown-ish text
            let body = html_to_text(html_content);

            if body.trim().is_empty() {
                continue;
            }

            // Try to parse version from title
            let parsed_version = parse_version_from_tag(title);

            // Store in fallback entries (recent releases regardless of version)
            if fallback_entries.len() < MAX_CHANGELOG_VERSIONS {
                // Use a dummy version for fallback entries if we can't parse one
                let version = parsed_version
                    .clone()
                    .unwrap_or_else(|| Version::new(0, 0, 0));
                fallback_entries.push(ChangelogEntry {
                    release_date: parsed_version
                        .as_ref()
                        .and_then(|v| release_date_for_version(v, version_publish_dates)),
                    version,
                    version_string: title.to_string(),
                    body: body.clone(),
                });
            }

            // Try version-based filtering if we have valid semver versions
            if use_version_filtering {
                if let (Some(current), Some(latest), Some(version)) =
                    (&current_semver, &latest_semver, &parsed_version)
                {
                    // Filter: current <= version <= latest (include current version)
                    if version >= current && version <= latest {
                        entries.push(ChangelogEntry {
                            release_date: release_date_for_version(version, version_publish_dates),
                            version: version.clone(),
                            version_string: title.to_string(),
                            body,
                        });

                        if entries.len() >= MAX_CHANGELOG_VERSIONS {
                            break;
                        }
                    }
                }
            }
        }

        // If version filtering yielded results, use them; otherwise use fallback
        let final_entries = if !entries.is_empty() {
            // Sort by version descending (newest first)
            entries.sort_by(|a, b| b.version.cmp(&a.version));
            entries
        } else if !fallback_entries.is_empty() {
            debug!(
                "Version filtering failed, using {} most recent releases for {}/{}",
                fallback_entries.len(),
                owner,
                repo
            );
            // Fallback entries are already in order (newest first from atom feed)
            fallback_entries
        } else {
            vec![]
        };

        debug!(
            "Parsed {} releases from atom feed for {}/{}",
            final_entries.len(),
            owner,
            repo
        );

        final_entries
    }

    /// Fetch and extract relevant sections from CHANGELOG.md
    async fn fetch_changelog_file(
        &self,
        owner: &str,
        repo: &str,
        current_version: &str,
        latest_version: &str,
        version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
    ) -> Vec<ChangelogEntry> {
        // Try common changelog file names
        let files = ["CHANGELOG.md", "changelog.md", "HISTORY.md", "CHANGES.md"];
        let branches = ["HEAD"];

        for file in &files {
            for branch in &branches {
                let url = format!(
                    "https://raw.githubusercontent.com/{}/{}/{}/{}",
                    owner, repo, branch, file
                );

                if let Some(content) = self.fetch_http_text(&url).await {
                    let entries = extract_changelog_sections_since_version(
                        &content,
                        current_version,
                        latest_version,
                        version_publish_dates,
                    );
                    if !entries.is_empty() {
                        return entries;
                    }
                }
            }
        }

        vec![]
    }
}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new(RegistryConfig::default())
    }
}

fn package_registry_url(registry_url: &str, package_name: &str) -> String {
    let base = registry_url.trim_end_matches('/');
    let encoded_package_name = package_name.replace('/', "%2f");
    format!("{}/{}", base, encoded_package_name)
}

/// Detect which track the current version belongs to
fn detect_track(current_version: &str, dist_tags: &DistTags) -> String {
    // Check for exact match with any dist-tag
    for (tag, version) in &dist_tags.other_tags {
        if version == current_version {
            return tag.clone();
        }
    }

    // Check if latest matches
    if let Some(ref latest) = dist_tags.latest {
        if latest == current_version {
            return "latest".to_string();
        }
    }

    // Check if it's a pre-release version that might indicate the track
    if current_version.contains('-') {
        let prerelease_part = current_version.split('-').nth(1).unwrap_or("");
        let tag_name = prerelease_part.split('.').next().unwrap_or("");

        if !tag_name.is_empty() && dist_tags.other_tags.contains_key(tag_name) {
            return tag_name.to_string();
        }

        // If the prerelease identifier doesn't match a tag directly,
        // check if the version pattern matches any track's version pattern
        // e.g., 55.0.0-beta.1 should match next: 55.0.0-beta.3
        if let Ok(current_ver) = Version::parse(current_version) {
            let current_channel = prerelease_channel(current_version);
            for (tag, track_version) in &dist_tags.other_tags {
                if let Ok(track_ver) = Version::parse(track_version) {
                    let track_channel = prerelease_channel(track_version);
                    // Check if major.minor.patch match and both have prerelease
                    if current_ver.major == track_ver.major
                        && current_ver.minor == track_ver.minor
                        && current_ver.patch == track_ver.patch
                        && track_ver.pre.is_empty() == current_ver.pre.is_empty()
                        && track_channel == current_channel
                    {
                        return tag.clone();
                    }
                }
            }
        }
    }

    // Default to latest
    "latest".to_string()
}

/// Get version for a specific track
fn get_version_for_track(dist_tags: &DistTags, track: &str) -> Option<String> {
    if track == "latest" {
        dist_tags.latest.clone()
    } else {
        dist_tags.other_tags.get(track).cloned()
    }
}

/// Build track info from dist-tags and release times
fn build_track_info(
    dist_tags: &DistTags,
    time_data: Option<&HashMap<String, DateTime<Utc>>>,
) -> Vec<TrackInfo> {
    let mut tracks = Vec::new();

    // Add latest first
    if let Some(ref version) = dist_tags.latest {
        let release_date = time_data.and_then(|t| t.get(version).copied());

        tracks.push(TrackInfo {
            name: "latest".to_string(),
            version: version.clone(),
            release_date,
        });
    }

    // Add other tags
    for (name, version) in &dist_tags.other_tags {
        let release_date = time_data.and_then(|t| t.get(version).copied());

        tracks.push(TrackInfo {
            name: name.clone(),
            version: version.clone(),
            release_date,
        });
    }

    // Sort by release date (newest first), then by version
    tracks.sort_by(|a, b| {
        match (b.release_date, a.release_date) {
            (Some(b_date), Some(a_date)) => b_date.cmp(&a_date),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                // Fall back to version comparison
                let a_ver = Version::parse(&a.version).ok();
                let b_ver = Version::parse(&b.version).ok();
                match (b_ver, a_ver) {
                    (Some(bv), Some(av)) => bv.cmp(&av),
                    _ => std::cmp::Ordering::Equal,
                }
            }
        }
    });

    tracks
}

/// Parse npm version publish times into a semver-only map
fn parse_version_publish_dates(
    time_data: Option<&HashMap<String, String>>,
) -> HashMap<String, DateTime<Utc>> {
    let mut publish_dates = HashMap::new();

    if let Some(time_map) = time_data {
        for (version, date_str) in time_map {
            if Version::parse(version).is_err() {
                continue;
            }

            if let Ok(date) = DateTime::parse_from_rfc3339(date_str) {
                publish_dates.insert(version.clone(), date.with_timezone(&Utc));
            }
        }
    }

    publish_dates
}

fn release_date_for_version(
    version: &Version,
    version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
) -> Option<DateTime<Utc>> {
    version_publish_dates
        .and_then(|map| map.get(&version.to_string()))
        .copied()
}

/// Build fallback release targets on the current track (newer than current, older than latest)
fn build_recent_current_track_releases(
    current_track: &str,
    current_version: &str,
    latest_on_track: &str,
    version_publish_dates: &HashMap<String, DateTime<Utc>>,
) -> Vec<VersionRelease> {
    let current_semver = match Version::parse(current_version) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let latest_semver = match Version::parse(latest_on_track) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let track_channel = prerelease_channel(latest_on_track);

    let mut releases: Vec<(Version, VersionRelease)> = version_publish_dates
        .iter()
        .filter_map(|(version_string, release_date)| {
            let parsed = Version::parse(version_string).ok()?;

            let is_on_current_track = if current_track == "latest" {
                parsed.pre.is_empty()
            } else if let Some(ref channel) = track_channel {
                prerelease_channel(version_string).as_deref() == Some(channel.as_str())
            } else {
                parsed.pre.is_empty()
            };

            if !is_on_current_track || parsed <= current_semver || parsed >= latest_semver {
                return None;
            }

            Some((
                parsed,
                VersionRelease {
                    version: version_string.clone(),
                    release_date: *release_date,
                },
            ))
        })
        .collect();

    releases.sort_by(|a, b| {
        b.1.release_date
            .cmp(&a.1.release_date)
            .then_with(|| b.0.cmp(&a.0))
    });

    releases.into_iter().map(|(_, release)| release).collect()
}

/// Convert HTML content from atom feed to readable text
fn html_to_text(html: &str) -> String {
    // Decode HTML entities
    let text = html
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // Convert common HTML tags to markdown
    let text = text
        // Headings
        .replace("<h1>", "# ")
        .replace("</h1>", "\n")
        .replace("<h2>", "## ")
        .replace("</h2>", "\n")
        .replace("<h3>", "### ")
        .replace("</h3>", "\n")
        // Lists
        .replace("<ul>", "")
        .replace("</ul>", "")
        .replace("<ol>", "")
        .replace("</ol>", "")
        .replace("<li>", "- ")
        .replace("</li>", "\n")
        // Paragraphs and breaks
        .replace("<p>", "")
        .replace("</p>", "\n\n")
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        // Code
        .replace("<code>", "`")
        .replace("</code>", "`")
        .replace("<pre>", "```\n")
        .replace("</pre>", "\n```\n")
        // Bold/italic
        .replace("<strong>", "**")
        .replace("</strong>", "**")
        .replace("<b>", "**")
        .replace("</b>", "**")
        .replace("<em>", "*")
        .replace("</em>", "*")
        .replace("<i>", "*")
        .replace("</i>", "*");

    // Remove remaining HTML tags
    let mut result = String::new();
    let mut in_tag = false;

    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }

    // Clean up excessive whitespace
    let lines: Vec<&str> = result.lines().map(|l| l.trim()).collect();

    lines.join("\n")
}

/// Parse a GitHub URL to extract owner and repo
fn parse_github_url(url: &str) -> Option<(String, String)> {
    let url = url
        .trim()
        .trim_start_matches("git+")
        .trim_start_matches("git://")
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("ssh://git@")
        .trim_start_matches("git@")
        .trim_end_matches(".git")
        .trim_end_matches('/');

    // Handle github.com/owner/repo format
    if let Some(rest) = url
        .strip_prefix("github.com/")
        .or_else(|| url.strip_prefix("github.com:"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
}

/// Parse version from a changelog header line
fn parse_version_from_header(line: &str) -> Option<Version> {
    VERSION_HEADER_REGEX
        .captures(line)
        .and_then(|cap| cap.get(1))
        .and_then(|m| Version::parse(m.as_str()).ok())
}

/// Parse version from a git tag or release title
/// Handles various formats:
/// - Standard: v1.0.0, 1.0.0
/// - Monorepo: package@1.0.0, @scope/package@1.0.0
/// - Revision: r182 (three.js style) -> converts to 182.0.0
/// - Complex titles: "oxlint v1.2.3 & oxfmt v4.5.6" -> extracts first version
fn parse_version_from_tag(tag: &str) -> Option<Version> {
    // First try the regex for standard tags (v1.0.0 or package@1.0.0)
    if let Some(cap) = VERSION_TAG_REGEX.captures(tag) {
        if let Some(m) = cap.get(1) {
            if let Ok(v) = Version::parse(m.as_str()) {
                return Some(v);
            }
        }
    }

    // Handle monorepo tags like "package-name@1.0.0" or "@scope/package@1.0.0"
    if let Some(at_pos) = tag.rfind('@') {
        let version_part = &tag[at_pos + 1..];
        let cleaned = version_part.trim_start_matches('v');
        if let Ok(v) = Version::parse(cleaned) {
            return Some(v);
        }
    }

    // Handle revision-style tags like "r182" (three.js)
    if let Some(cap) = REVISION_TAG_REGEX.captures(tag) {
        if let Some(m) = cap.get(1) {
            if let Ok(num) = m.as_str().parse::<u64>() {
                // Convert r182 to 182.0.0 for comparison purposes
                return Version::parse(&format!("{}.0.0", num)).ok();
            }
        }
    }

    // Try to find a semver anywhere in the string (for complex titles)
    // e.g., "oxlint v1.2.3 & oxfmt v4.5.6" -> extracts "1.2.3"
    if let Some(cap) = VERSION_ANYWHERE_REGEX.captures(tag) {
        if let Some(m) = cap.get(1) {
            if let Ok(v) = Version::parse(m.as_str()) {
                return Some(v);
            }
        }
    }

    // Fall back to simple parsing (v1.0.0 or 1.0.0)
    let cleaned = tag.trim_start_matches('v');
    Version::parse(cleaned).ok()
}

/// Extract changelog sections for versions between current and latest
/// Falls back to returning the most recent sections if version parsing fails
fn extract_changelog_sections_since_version(
    content: &str,
    current_version: &str,
    latest_version: &str,
    version_publish_dates: Option<&HashMap<String, DateTime<Utc>>>,
) -> Vec<ChangelogEntry> {
    // Try to parse versions for filtering
    let current_semver = Version::parse(current_version).ok();
    let latest_semver = Version::parse(latest_version).ok();
    let use_version_filtering = current_semver.is_some() && latest_semver.is_some();

    let mut entries = Vec::new();
    let mut fallback_entries = Vec::new(); // For when version filtering yields nothing
    let mut current_entry: Option<(Option<Version>, String, Vec<String>)> = None;

    for line in content.lines() {
        // Check if this is a version header
        if line.starts_with("## ") || line.starts_with("# ") {
            // Save the previous entry
            if let Some((version_opt, header, lines)) = current_entry.take() {
                let body = lines.join("\n");
                if !body.trim().is_empty() || !header.is_empty() {
                    // Always add to fallback (up to limit)
                    if fallback_entries.len() < MAX_CHANGELOG_VERSIONS {
                        let version = version_opt.clone().unwrap_or_else(|| Version::new(0, 0, 0));
                        fallback_entries.push(ChangelogEntry {
                            release_date: version_opt
                                .as_ref()
                                .and_then(|v| release_date_for_version(v, version_publish_dates)),
                            version,
                            version_string: header.clone(),
                            body: body.clone(),
                        });
                    }

                    // Add to filtered entries if version is in range
                    if use_version_filtering {
                        if let (Some(current), Some(latest), Some(version)) =
                            (&current_semver, &latest_semver, &version_opt)
                        {
                            // Include current version too: current <= version <= latest
                            if version >= current && version <= latest {
                                entries.push(ChangelogEntry {
                                    release_date: release_date_for_version(
                                        version,
                                        version_publish_dates,
                                    ),
                                    version: version.clone(),
                                    version_string: header,
                                    body,
                                });
                            } else if version < current {
                                // We've gone past the relevant versions in filtered mode
                                break;
                            }
                        }
                    }

                    if entries.len() >= MAX_CHANGELOG_VERSIONS {
                        break;
                    }
                }
            }

            // Try to parse version from header, but continue even if it fails
            let version_opt = parse_version_from_header(line);
            current_entry = Some((version_opt, line.to_string(), Vec::new()));
        } else if let Some((_, _, ref mut lines)) = current_entry {
            lines.push(line.to_string());
        }
    }

    // Don't forget the last entry
    if let Some((version_opt, header, lines)) = current_entry {
        let body = lines.join("\n");
        if !body.trim().is_empty() || !header.is_empty() {
            if fallback_entries.len() < MAX_CHANGELOG_VERSIONS {
                let version = version_opt.clone().unwrap_or_else(|| Version::new(0, 0, 0));
                fallback_entries.push(ChangelogEntry {
                    release_date: version_opt
                        .as_ref()
                        .and_then(|v| release_date_for_version(v, version_publish_dates)),
                    version,
                    version_string: header.clone(),
                    body: body.clone(),
                });
            }

            if use_version_filtering {
                if let (Some(current), Some(latest), Some(version)) =
                    (&current_semver, &latest_semver, &version_opt)
                {
                    // Include current version too: current <= version <= latest
                    if version >= current
                        && version <= latest
                        && entries.len() < MAX_CHANGELOG_VERSIONS
                    {
                        entries.push(ChangelogEntry {
                            release_date: release_date_for_version(version, version_publish_dates),
                            version: version.clone(),
                            version_string: header,
                            body,
                        });
                    }
                }
            }
        }
    }

    // Use filtered entries if we got any, otherwise use fallback
    if !entries.is_empty() {
        entries.sort_by(|a, b| b.version.cmp(&a.version));
        entries
    } else {
        // Fallback entries are already in document order (typically newest first)
        fallback_entries
    }
}

/// Format changelog entries to string
fn format_changelog_entries(
    entries: Vec<ChangelogEntry>,
    date_display: &DateDisplaySettings,
) -> String {
    let mut output = Vec::new();

    for entry in entries {
        output.push(format_changelog_header(&entry, date_display));

        let body = entry.body.trim();
        if !body.is_empty() {
            output.push(body.to_string());
        }

        output.push(String::new());
    }

    output.join("\n").trim().to_string()
}

fn format_changelog_header(entry: &ChangelogEntry, date_display: &DateDisplaySettings) -> String {
    let base = if entry.version_string.starts_with('#') {
        entry.version_string.clone()
    } else {
        format!("## {}", entry.version_string)
    };

    if let Some(date) = entry.release_date {
        let normalized = remove_existing_date_from_header(&base);
        format!("{} ({})", normalized, date_display.format_date_tag(date))
    } else {
        base
    }
}

fn remove_existing_date_from_header(header: &str) -> String {
    let Some(date_match) = HEADER_DATE_REGEX.find(header) else {
        return header.to_string();
    };

    let mut remove_start = date_match.start();
    let mut remove_end = date_match.end();

    // If the date is inside (...) remove the whole parenthesized segment.
    if let Some(open_idx) = header[..date_match.start()].rfind('(') {
        if let Some(close_rel) = header[date_match.end()..].find(')') {
            let close_idx = date_match.end() + close_rel;
            remove_start = open_idx;
            remove_end = close_idx + 1;
        }
    }

    // Also trim surrounding separators like " - 2026-02-14".
    while remove_start > 0 {
        let prev_char = header[..remove_start].chars().last().unwrap();
        if prev_char.is_whitespace() || matches!(prev_char, '-' | '–' | '—' | ':' | ',') {
            remove_start -= prev_char.len_utf8();
        } else {
            break;
        }
    }

    while remove_end < header.len() {
        let next_char = header[remove_end..].chars().next().unwrap();
        if next_char.is_whitespace() {
            remove_end += next_char.len_utf8();
        } else {
            break;
        }
    }

    let left = header[..remove_start].trim_end();
    let right = header[remove_end..].trim_start();

    if left.is_empty() {
        right.to_string()
    } else if right.is_empty() {
        left.to_string()
    } else {
        format!("{} {}", left, right)
    }
}

/// Merge changelogs from GitHub releases and CHANGELOG.md file
fn merge_changelogs(
    releases: Vec<ChangelogEntry>,
    changelog_file: Vec<ChangelogEntry>,
    current_version: &str,
    _latest_version: &str,
    date_display: &DateDisplaySettings,
) -> String {
    // Build a map by version, preferring GitHub releases (usually more curated)
    let mut by_version: HashMap<String, ChangelogEntry> = HashMap::new();

    // Add changelog file entries first (lower priority)
    for entry in changelog_file {
        by_version.insert(entry.version.to_string(), entry);
    }

    // Add GitHub releases (higher priority, overwrites changelog file entries)
    for entry in releases {
        by_version.insert(entry.version.to_string(), entry);
    }

    if by_version.is_empty() {
        return String::new();
    }

    // Collect and sort entries
    let mut entries: Vec<ChangelogEntry> = by_version.into_values().collect();
    entries.sort_by(|a, b| b.version.cmp(&a.version));

    // Truncate to max versions
    let total_count = entries.len();
    entries.truncate(MAX_CHANGELOG_VERSIONS);

    // Format output
    let mut output = Vec::new();

    for entry in &entries {
        output.push(format_changelog_header(entry, date_display));

        // Add body (trimmed)
        let body = entry.body.trim();
        if !body.is_empty() {
            output.push(body.to_string());
        }

        output.push(String::new()); // Blank line separator
    }

    // Add truncation notice if needed
    if total_count > MAX_CHANGELOG_VERSIONS {
        output.push(format!(
            "*Showing {} of {} versions since {}*",
            MAX_CHANGELOG_VERSIONS, total_count, current_version
        ));
    }

    let result = output.join("\n").trim().to_string();

    // Final length limit to avoid massive tooltips
    // Zed has a nice scrolling view, so we can allow much more content
    if result.len() > 15000 {
        format!("{}...\n\n*[Truncated for display]*", &result[..15000])
    } else {
        result
    }
}

/// Build other tracks info for version status
fn build_other_tracks(
    current_track: &str,
    current_version: &str,
    all_tracks: &[TrackInfo],
) -> Vec<TrackUpdate> {
    let current_semver = Version::parse(current_version).ok();

    all_tracks
        .iter()
        .filter(|t| t.name != current_track)
        .map(|t| {
            let track_semver = Version::parse(&t.version).ok();
            let is_newer = match (&current_semver, &track_semver) {
                (Some(cv), Some(tv)) => tv > cv,
                _ => false,
            };

            TrackUpdate {
                name: t.name.clone(),
                version: t.version.clone(),
                release_date: t.release_date,
                is_newer,
            }
        })
        .collect()
}

/// Check version status with track awareness
pub fn check_version_status(
    current_version: &str,
    latest_on_track: &str,
    current_track: &str,
    all_tracks: &[TrackInfo],
    recent_current_track_releases: &[VersionRelease],
    changelog: Option<String>,
    repository_url: Option<String>,
) -> VersionStatus {
    let current_track_release_date = all_tracks
        .iter()
        .find(|t| t.name == current_track)
        .and_then(|t| t.release_date);

    let current_parsed = match Version::parse(current_version) {
        Ok(v) => v,
        Err(_) => {
            return VersionStatus::Unknown {
                current_track: current_track.to_string(),
                current_version: current_version.to_string(),
                current_track_release_date,
                recent_current_track_releases: recent_current_track_releases.to_vec(),
                other_tracks: build_other_tracks(current_track, current_version, all_tracks),
                changelog,
                repository_url,
            };
        }
    };

    let latest_parsed = match Version::parse(latest_on_track) {
        Ok(v) => v,
        Err(_) => {
            return VersionStatus::Unknown {
                current_track: current_track.to_string(),
                current_version: current_version.to_string(),
                current_track_release_date,
                recent_current_track_releases: recent_current_track_releases.to_vec(),
                other_tracks: build_other_tracks(current_track, current_version, all_tracks),
                changelog,
                repository_url,
            };
        }
    };

    let other_tracks = build_other_tracks(current_track, current_version, all_tracks);

    // Track diagnostics are based on the user's current track only.
    let current_track_has_update = latest_parsed > current_parsed;

    if !current_track_has_update {
        return VersionStatus::UpToDate {
            current_track: current_track.to_string(),
            current_version: current_version.to_string(),
            current_track_release_date,
            recent_current_track_releases: recent_current_track_releases.to_vec(),
            other_tracks,
            changelog,
            repository_url,
        };
    }

    let severity = if latest_parsed.major > current_parsed.major {
        UpdateSeverity::Major
    } else if latest_parsed.minor > current_parsed.minor {
        UpdateSeverity::Minor
    } else {
        UpdateSeverity::Patch
    };

    VersionStatus::UpdateAvailable {
        current_track: current_track.to_string(),
        current_version: current_version.to_string(),
        current_track_release_date,
        recent_current_track_releases: recent_current_track_releases.to_vec(),
        latest_on_track: latest_on_track.to_string(),
        other_tracks,
        severity,
        changelog,
        repository_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc_date(input: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(input)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn test_detect_track() {
        let dist_tags = DistTags {
            latest: Some("2.0.0".to_string()),
            other_tags: [
                ("next".to_string(), "2.1.0-beta.1".to_string()),
                ("canary".to_string(), "2.2.0-canary.1".to_string()),
            ]
            .into_iter()
            .collect(),
        };

        assert_eq!(detect_track("2.0.0", &dist_tags), "latest");
        assert_eq!(detect_track("2.1.0-beta.1", &dist_tags), "next");
        assert_eq!(detect_track("2.2.0-canary.1", &dist_tags), "canary");
        assert_eq!(detect_track("1.5.0", &dist_tags), "latest"); // Default
    }

    #[test]
    fn test_detect_track_expo_ui_scenario() {
        // Test the @expo/ui scenario where the tag name doesn't match the prerelease identifier
        // next: 55.0.0-beta.3, but user has 55.0.0-beta.1
        let dist_tags = DistTags {
            latest: Some("0.2.0-beta.9".to_string()),
            other_tags: [
                ("next".to_string(), "55.0.0-beta.3".to_string()),
                (
                    "canary".to_string(),
                    "55.0.0-canary-20260128-67ce8d5".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
        };

        // User has an older beta version that matches the next track pattern
        assert_eq!(detect_track("55.0.0-beta.1", &dist_tags), "next");
        assert_eq!(detect_track("55.0.0-beta.2", &dist_tags), "next");
        // Exact match
        assert_eq!(detect_track("55.0.0-beta.3", &dist_tags), "next");
        // Canary track
        assert_eq!(
            detect_track("55.0.0-canary-20260128-67ce8d5", &dist_tags),
            "canary"
        );
        // Latest track
        assert_eq!(detect_track("0.2.0-beta.9", &dist_tags), "latest");
    }

    #[test]
    fn test_format_time_ago() {
        let now = Utc::now();

        assert_eq!(
            crate::settings::format_time_ago(now - chrono::Duration::hours(2)),
            "2 hours ago"
        );
        assert_eq!(
            crate::settings::format_time_ago(now - chrono::Duration::days(1)),
            "1 day ago"
        );
        assert_eq!(
            crate::settings::format_time_ago(now - chrono::Duration::days(5)),
            "5 days ago"
        );
    }

    #[test]
    fn test_format_date_with_ago_uses_compact_date() {
        let formatted =
            DateDisplaySettings::default().format_date_tag(utc_date("2024-01-15T00:00:00Z"));
        assert!(formatted.starts_with("15/01/2024, "));
        assert!(formatted.contains("ago"));
    }

    #[test]
    fn test_build_recent_current_track_releases_latest_track() {
        let release_dates = HashMap::from([
            ("1.0.1".to_string(), utc_date("2026-01-01T00:00:00Z")),
            ("1.0.2".to_string(), utc_date("2026-01-02T00:00:00Z")),
            ("1.0.3".to_string(), utc_date("2026-01-03T00:00:00Z")),
            ("1.0.4".to_string(), utc_date("2026-01-04T00:00:00Z")),
            ("1.1.0-beta.1".to_string(), utc_date("2026-01-05T00:00:00Z")),
        ]);

        let releases =
            build_recent_current_track_releases("latest", "1.0.0", "1.0.4", &release_dates);

        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0].version, "1.0.3");
        assert_eq!(releases[1].version, "1.0.2");
        assert_eq!(releases[2].version, "1.0.1");
    }

    #[test]
    fn test_build_recent_current_track_releases_prerelease_track() {
        let release_dates = HashMap::from([
            ("2.0.0-beta.2".to_string(), utc_date("2026-01-01T00:00:00Z")),
            ("2.0.0-beta.3".to_string(), utc_date("2026-01-02T00:00:00Z")),
            ("2.0.0-beta.4".to_string(), utc_date("2026-01-03T00:00:00Z")),
            ("2.0.0-beta.5".to_string(), utc_date("2026-01-04T00:00:00Z")),
            ("2.0.0-rc.1".to_string(), utc_date("2026-01-05T00:00:00Z")),
            ("2.0.0".to_string(), utc_date("2026-01-06T00:00:00Z")),
        ]);

        let releases = build_recent_current_track_releases(
            "next",
            "2.0.0-beta.1",
            "2.0.0-beta.5",
            &release_dates,
        );

        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0].version, "2.0.0-beta.4");
        assert_eq!(releases[1].version, "2.0.0-beta.3");
        assert_eq!(releases[2].version, "2.0.0-beta.2");
    }

    #[test]
    fn test_format_changelog_entries_with_and_without_dates() {
        let entries = vec![
            ChangelogEntry {
                version: Version::parse("1.2.3").unwrap(),
                version_string: "1.2.3".to_string(),
                body: "- Added feature".to_string(),
                release_date: Some(utc_date("2026-01-03T00:00:00Z")),
            },
            ChangelogEntry {
                version: Version::parse("1.2.2").unwrap(),
                version_string: "1.2.2".to_string(),
                body: "- Fixed bug".to_string(),
                release_date: None,
            },
        ];

        let output = format_changelog_entries(entries, &DateDisplaySettings::default());

        assert!(output.contains("## 1.2.3 (03/01/2026, "));
        assert!(output.contains("## 1.2.2"));
        assert!(!output.contains("## 1.2.2 ("));
    }

    #[test]
    fn test_format_changelog_entries_avoids_duplicate_date_in_header() {
        let entries = vec![ChangelogEntry {
            version: Version::parse("3.3.0").unwrap(),
            version_string: "3.3.0 (2026-02-14)".to_string(),
            body: "- Notes".to_string(),
            release_date: Some(utc_date("2026-02-14T00:00:00Z")),
        }];

        let output = format_changelog_entries(entries, &DateDisplaySettings::default());

        assert!(output.contains("## 3.3.0 (14/02/2026, "));
        assert!(!output.contains("## 3.3.0 (2026-02-14)"));
    }

    #[test]
    fn test_format_changelog_entries_normalizes_dash_date_header() {
        let entries = vec![ChangelogEntry {
            version: Version::parse("4.18.2").unwrap(),
            version_string: "[4.18.2] - 2024-01-15".to_string(),
            body: "- Notes".to_string(),
            release_date: Some(utc_date("2024-01-15T00:00:00Z")),
        }];

        let output = format_changelog_entries(entries, &DateDisplaySettings::default());

        assert!(output.contains("## [4.18.2] (15/01/2024, "));
        assert!(!output.contains("## [4.18.2] - 2024-01-15"));
    }

    #[test]
    fn test_check_version_status() {
        let tracks = vec![
            TrackInfo {
                name: "latest".to_string(),
                version: "2.0.0".to_string(),
                release_date: None,
            },
            TrackInfo {
                name: "next".to_string(),
                version: "2.1.0-beta.1".to_string(),
                release_date: None,
            },
        ];

        match check_version_status("1.0.0", "2.0.0", "latest", &tracks, &[], None, None) {
            VersionStatus::UpdateAvailable {
                severity,
                recent_current_track_releases,
                other_tracks,
                ..
            } => {
                assert_eq!(severity, UpdateSeverity::Major);
                assert!(recent_current_track_releases.is_empty());
                assert_eq!(other_tracks.len(), 1);
                assert_eq!(other_tracks[0].name, "next");
            }
            _ => panic!("Expected UpdateAvailable"),
        }

        match check_version_status("2.0.0", "2.0.0", "latest", &tracks, &[], None, None) {
            VersionStatus::UpToDate { other_tracks, .. } => {
                assert_eq!(other_tracks.len(), 1);
            }
            _ => panic!("Expected UpToDate"),
        }
    }

    #[test]
    fn test_check_version_status_does_not_flag_other_track_only_updates() {
        let tracks = vec![
            TrackInfo {
                name: "latest".to_string(),
                version: "2.0.0".to_string(),
                release_date: None,
            },
            TrackInfo {
                name: "next".to_string(),
                version: "2.1.0-beta.1".to_string(),
                release_date: None,
            },
        ];

        match check_version_status("2.0.0", "2.0.0", "latest", &tracks, &[], None, None) {
            VersionStatus::UpToDate { .. } => {}
            _ => panic!("Expected UpToDate when only other tracks are newer"),
        }
    }

    #[test]
    fn test_prerelease_track_label() {
        assert_eq!(prerelease_track_label("1.0.0-alpha.1"), Some("ALPHA"));
        assert_eq!(prerelease_track_label("1.0.0-beta.2"), Some("BETA"));
        assert_eq!(
            prerelease_track_label("1.0.0-canary-20260128-abc"),
            Some("CANARY")
        );
        assert_eq!(prerelease_track_label("1.0.0-nightly.0"), Some("NIGHTLY"));
        assert_eq!(prerelease_track_label("1.0.0-rc.3"), Some("CANDIDATE"));
        assert_eq!(
            prerelease_track_label("1.0.0-experimental.0"),
            Some("EXPERIMENTAL")
        );
        assert_eq!(prerelease_track_label("1.0.0"), None);
    }

    #[test]
    fn test_parse_github_url() {
        assert_eq!(
            parse_github_url("https://github.com/expressjs/express"),
            Some(("expressjs".to_string(), "express".to_string()))
        );
        assert_eq!(
            parse_github_url("git+https://github.com/lodash/lodash.git"),
            Some(("lodash".to_string(), "lodash".to_string()))
        );
        assert_eq!(
            parse_github_url("git@github.com:facebook/react.git"),
            Some(("facebook".to_string(), "react".to_string()))
        );
        // Test zod's URL format
        assert_eq!(
            parse_github_url("git+https://github.com/colinhacks/zod.git"),
            Some(("colinhacks".to_string(), "zod".to_string()))
        );
        // Test react-native-screen-transitions URL format
        assert_eq!(
            parse_github_url("git+https://github.com/eds2002/react-native-screen-transitions.git"),
            Some((
                "eds2002".to_string(),
                "react-native-screen-transitions".to_string()
            ))
        );
    }

    #[test]
    fn test_html_to_text() {
        let html = "&lt;p&gt;Hello &amp; goodbye&lt;/p&gt;";
        let text = html_to_text(html);
        assert!(text.contains("Hello & goodbye"));

        let html_list = "<ul><li>Item 1</li><li>Item 2</li></ul>";
        let text = html_to_text(html_list);
        assert!(text.contains("- Item 1"));
        assert!(text.contains("- Item 2"));
    }

    #[test]
    fn test_parse_version_from_tag() {
        assert_eq!(
            parse_version_from_tag("v1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(
            parse_version_from_tag("1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(
            parse_version_from_tag("package@1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        // Monorepo style tags
        assert_eq!(
            parse_version_from_tag("react-native-screen-transitions@3.1.0"),
            Some(Version::parse("3.1.0").unwrap())
        );
        assert_eq!(
            parse_version_from_tag("react-native-screen-transitions@2.4.2-beta.0"),
            Some(Version::parse("2.4.2-beta.0").unwrap())
        );
        assert_eq!(
            parse_version_from_tag("@scope/package@1.2.3"),
            Some(Version::parse("1.2.3").unwrap())
        );
        // Test zod's tag format (v4.x.y)
        assert_eq!(
            parse_version_from_tag("v4.3.5"),
            Some(Version::parse("4.3.5").unwrap())
        );
        assert_eq!(
            parse_version_from_tag("v3.22.4"),
            Some(Version::parse("3.22.4").unwrap())
        );
        // Test three.js revision format (r182)
        assert_eq!(
            parse_version_from_tag("r182"),
            Some(Version::parse("182.0.0").unwrap())
        );
        assert_eq!(
            parse_version_from_tag("r99"),
            Some(Version::parse("99.0.0").unwrap())
        );
        // Test complex monorepo titles like oxlint
        assert_eq!(
            parse_version_from_tag("oxlint v1.2.3 & oxfmt v4.5.6"),
            Some(Version::parse("1.2.3").unwrap()) // Takes first version found
        );
        assert_eq!(
            parse_version_from_tag("Release 2.0.0 - Major Update"),
            Some(Version::parse("2.0.0").unwrap())
        );
    }

    #[test]
    fn test_parse_version_from_header() {
        assert_eq!(
            parse_version_from_header("## 19.2.1 (Dec 3, 2025)"),
            Some(Version::parse("19.2.1").unwrap())
        );
        assert_eq!(
            parse_version_from_header("## [4.18.2] - 2024-01-15"),
            Some(Version::parse("4.18.2").unwrap())
        );
        assert_eq!(
            parse_version_from_header("## v1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(
            parse_version_from_header("# 1.0.0"),
            Some(Version::parse("1.0.0").unwrap())
        );
        assert_eq!(
            parse_version_from_header("## [1.2.3-beta.1]"),
            Some(Version::parse("1.2.3-beta.1").unwrap())
        );
    }

    #[test]
    fn test_extract_changelog_sections_since_version() {
        let content = r#"# Changelog

## 19.2.1 (Dec 3, 2025)

### React Server Components

- Bring React Server Component fixes

## 19.2.0 (October 1st, 2025)

Below is a list of all new features

## 19.1.2 (Dec 3, 2025)

### React Server Components

- Another fix

## 19.1.1 (July 28, 2025)

### React
* Fixed Owner Stacks

## 19.1.0 (March 28, 2025)

misc.
"#;

        let entries = extract_changelog_sections_since_version(content, "19.1.0", "19.2.1", None);

        assert_eq!(entries.len(), 5); // 19.2.1, 19.2.0, 19.1.2, 19.1.1, 19.1.0
        assert_eq!(entries[0].version, Version::parse("19.2.1").unwrap());
        assert_eq!(entries[1].version, Version::parse("19.2.0").unwrap());
        assert_eq!(entries[2].version, Version::parse("19.1.2").unwrap());
        assert_eq!(entries[3].version, Version::parse("19.1.1").unwrap());
        assert_eq!(entries[4].version, Version::parse("19.1.0").unwrap());
    }

    #[test]
    fn test_extract_changelog_sections_truncation() {
        let content = r#"
## 10.0.0
Major release

## 9.0.0
Major release

## 8.0.0
Major release

## 7.0.0
Major release

## 6.0.0
Major release

## 5.0.0
Major release

## 4.0.0
Major release

## 3.0.0
Major release
"#;

        let entries = extract_changelog_sections_since_version(content, "1.0.0", "10.0.0", None);

        // All 8 versions should be included (10, 9, 8, 7, 6, 5, 4, 3) since MAX is now 15
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[0].version, Version::parse("10.0.0").unwrap());
        assert_eq!(entries[7].version, Version::parse("3.0.0").unwrap());
    }

    #[test]
    fn test_package_registry_url_encodes_scoped_packages() {
        assert_eq!(
            package_registry_url("https://registry.npmjs.org/", "@scope/pkg"),
            "https://registry.npmjs.org/@scope%2fpkg"
        );
    }

    #[test]
    fn test_apply_config_rebuilds_runtime_state_and_clears_cache_on_registry_change() {
        let registry = NpmRegistry::default();

        registry.metadata_cache.insert(
            "react".to_string(),
            CachedPackageMetadata {
                dist_tags: DistTags {
                    latest: Some("18.3.1".to_string()),
                    other_tags: HashMap::new(),
                },
                version_publish_dates: HashMap::new(),
                repository_url: None,
                repository_directory: None,
                fetched_at: Instant::now(),
            },
        );

        registry.apply_config(RegistryConfig {
            registry_url: "https://registry.company.test".to_string(),
            cache_ttl: Duration::from_secs(10),
            max_concurrent_requests: 3,
            max_concurrent_changelog_requests: 2,
            request_timeout: Duration::from_secs(22),
            date_display: DateDisplaySettings::default(),
        });

        assert!(registry.metadata_cache.is_empty());
        assert_eq!(
            registry.config().registry_url,
            "https://registry.company.test".to_string()
        );
        assert_eq!(registry.config().max_concurrent_requests, 3);
        assert_eq!(registry.config().max_concurrent_changelog_requests, 2);
        assert_eq!(registry.config().request_timeout, Duration::from_secs(22));
    }

    #[tokio::test]
    async fn cached_metadata_is_derived_for_each_requested_version() {
        let registry = NpmRegistry::default();
        registry.metadata_cache.insert(
            "example".to_string(),
            CachedPackageMetadata {
                dist_tags: DistTags {
                    latest: Some("2.0.0".to_string()),
                    other_tags: HashMap::from([("beta".to_string(), "3.0.0-beta.2".to_string())]),
                },
                version_publish_dates: HashMap::new(),
                repository_url: None,
                repository_directory: None,
                fetched_at: Instant::now(),
            },
        );

        let stable = registry
            .get_package_version_info("example", "1.0.0")
            .await
            .unwrap();
        let beta = registry
            .get_package_version_info("example", "3.0.0-beta.2")
            .await
            .unwrap();

        assert_eq!(stable.current_track, "latest");
        assert_eq!(stable.latest_on_track, "2.0.0");
        assert_eq!(beta.current_track, "beta");
        assert_eq!(beta.latest_on_track, "3.0.0-beta.2");
    }

    #[test]
    fn test_build_recent_current_track_releases_returns_all_candidates() {
        let release_dates = HashMap::from([
            ("1.0.1".to_string(), utc_date("2026-01-01T00:00:00Z")),
            ("1.0.2".to_string(), utc_date("2026-01-02T00:00:00Z")),
            ("1.0.3".to_string(), utc_date("2026-01-03T00:00:00Z")),
            ("1.0.4".to_string(), utc_date("2026-01-04T00:00:00Z")),
            ("1.0.5".to_string(), utc_date("2026-01-05T00:00:00Z")),
        ]);

        let releases =
            build_recent_current_track_releases("latest", "1.0.0", "1.0.5", &release_dates);

        assert_eq!(releases.len(), 4);
        assert_eq!(releases[0].version, "1.0.4");
        assert_eq!(releases[3].version, "1.0.1");
    }
}
