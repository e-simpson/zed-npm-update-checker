use chrono::{DateTime, Utc};
use dashmap::DashMap;
use lazy_static::lazy_static;
use regex::Regex;
use semver::Version;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org";
const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes
const MAX_CONCURRENT_REQUESTS: usize = 10;
const MAX_CHANGELOG_VERSIONS: usize = 15;

lazy_static! {
    /// Regex to extract version numbers from changelog headers
    /// Matches patterns like:
    /// - "## 19.2.1 (Dec 3, 2025)"
    /// - "## [4.18.2] - 2024-01-15"
    /// - "## v1.0.0"
    /// - "# 1.0.0"
    static ref VERSION_HEADER_REGEX: Regex = Regex::new(
        r"(?i)^#{1,2}\s*\[?v?(\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?)\]?"
    ).unwrap();

    /// Regex to extract standard semver from git tags
    /// Matches various tag formats:
    /// - v1.0.0
    /// - 1.0.0
    /// - package@1.0.0 (monorepo style)
    /// - package-name@1.0.0-beta.1
    /// - @scope/package@1.0.0
    static ref VERSION_TAG_REGEX: Regex = Regex::new(
        r"(?:^v|@|^)(\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?)$"
    ).unwrap();

    /// Regex to extract semver from anywhere in a string (for monorepo titles)
    /// Matches: "oxlint v1.2.3 & oxfmt v4.5.6" -> extracts "1.2.3"
    static ref VERSION_ANYWHERE_REGEX: Regex = Regex::new(
        r"v?(\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?)"
    ).unwrap();

    /// Regex for non-standard version formats like three.js "r182"
    static ref REVISION_TAG_REGEX: Regex = Regex::new(
        r"^r(\d+)$"
    ).unwrap();

    /// Regex to extract entries from GitHub releases atom feed
    /// Captures: title (version tag) and content (release body in HTML)
    static ref ATOM_ENTRY_REGEX: Regex = Regex::new(
        r"(?s)<entry>.*?<title>([^<]+)</title>.*?<content[^>]*>(.*?)</content>.*?</entry>"
    ).unwrap();
}

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

/// Result of comparing current version to latest
#[derive(Debug, Clone, PartialEq)]
pub enum VersionStatus {
    /// Currently checking version
    Checking,
    /// Package is up to date
    UpToDate {
        current_track: String,
        current_version: String,
        other_tracks: Vec<TrackUpdate>,
        changelog: Option<String>,
        repository_url: Option<String>,
    },
    /// Update available with severity, changelog, and repository URL
    UpdateAvailable {
        current_track: String,
        current_version: String,
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
}

/// Cache key includes both package name and current version for changelog relevance
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    package_name: String,
    current_version: String,
}

/// Simple cache key for version-only info (no changelog)
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct VersionCacheKey {
    package_name: String,
}

/// Full package info with track information
#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub name: String,
    pub current_track: String,
    pub current_version: String,
    pub latest_on_track: String,
    pub all_tracks: Vec<TrackInfo>,
    pub repository_url: Option<String>,
    pub repository_directory: Option<String>,
    pub changelog: Option<String>,
    pub fetched_at: Instant,
}

/// Lightweight package info without changelog (for fast initial display)
#[derive(Debug, Clone)]
pub struct PackageVersionInfo {
    pub name: String,
    pub current_track: String,
    pub current_version: String,
    pub latest_on_track: String,
    pub all_tracks: Vec<TrackInfo>,
    pub repository_url: Option<String>,
    pub repository_directory: Option<String>,
    pub fetched_at: Instant,
}

#[derive(Debug, Deserialize)]
struct NpmPackageResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: Option<DistTags>,
    repository: Option<Repository>,
    time: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
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
        repo_type: Option<String>,
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
    client: reqwest::Client,
    /// Cache for full package info (with changelog)
    cache: Arc<DashMap<CacheKey, PackageInfo>>,
    /// Cache for version-only info (fast, no changelog)
    version_cache: Arc<DashMap<VersionCacheKey, PackageVersionInfo>>,
    /// Semaphore for limiting concurrent npm requests
    semaphore: Arc<Semaphore>,
}

impl NpmRegistry {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("npm-package-json-checker-lsp")
            .build()
            .unwrap_or_default();

        Self {
            client,
            cache: Arc::new(DashMap::new()),
            version_cache: Arc::new(DashMap::new()),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        }
    }

    /// Fast version check - only fetches npm registry, no GitHub API calls
    /// Use this for initial display of updates, then fetch changelogs separately
    pub async fn get_package_version_info(
        &self,
        package_name: &str,
        current_version: &str,
    ) -> Option<PackageVersionInfo> {
        let cache_key = VersionCacheKey {
            package_name: package_name.to_string(),
        };

        // Check cache first
        if let Some(cached) = self.version_cache.get(&cache_key) {
            if cached.fetched_at.elapsed() < CACHE_TTL {
                return Some(cached.clone());
            }
        }

        // Acquire semaphore permit for rate limiting npm requests
        let _permit = self.semaphore.acquire().await.ok()?;

        // Fetch from npm registry only
        let url = format!("{}/{}", NPM_REGISTRY_URL, package_name);

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                debug!("Failed to fetch {}: {}", package_name, e);
                return None;
            }
        };

        if !response.status().is_success() {
            debug!("Non-success status for {}: {}", package_name, response.status());
            return None;
        }

        let data: NpmPackageResponse = match response.json().await {
            Ok(d) => d,
            Err(e) => {
                debug!("Failed to parse response for {}: {}", package_name, e);
                return None;
            }
        };

        let dist_tags = data.dist_tags?;
        let current_track = detect_track(current_version, &dist_tags);
        let latest_on_track = get_version_for_track(&dist_tags, &current_track)?;

        let all_tracks = build_track_info(&dist_tags, data.time.as_ref());

        let repo_url = data.repository.as_ref().and_then(|r| r.get_url());
        let repo_directory = data.repository.as_ref().and_then(|r| r.get_directory());

        let info = PackageVersionInfo {
            name: package_name.to_string(),
            current_track,
            current_version: current_version.to_string(),
            latest_on_track,
            all_tracks,
            repository_url: repo_url,
            repository_directory: repo_directory,
            fetched_at: Instant::now(),
        };

        // Update cache
        self.version_cache.insert(cache_key, info.clone());

        Some(info)
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
    ) -> Option<String> {
        let cache_key = CacheKey {
            package_name: package_name.to_string(),
            current_version: current_version.to_string(),
        };

        // Check if we already have this changelog cached
        if let Some(cached) = self.cache.get(&cache_key) {
            if cached.fetched_at.elapsed() < CACHE_TTL && cached.changelog.is_some() {
                return cached.changelog.clone();
            }
        }

        // Fetch changelog from GitHub
        let changelog = self.fetch_changelog(repo_url, repo_directory, current_version, latest_version).await;

        // Update full cache with changelog
        let info = PackageInfo {
            name: package_name.to_string(),
            current_track: String::new(),
            current_version: current_version.to_string(),
            latest_on_track: latest_version.to_string(),
            all_tracks: vec![],
            repository_url: Some(repo_url.to_string()),
            repository_directory: repo_directory.map(|s| s.to_string()),
            changelog: changelog.clone(),
            fetched_at: Instant::now(),
        };
        self.cache.insert(cache_key, info);

        changelog
    }

    /// Get full package info including changelog filtered by current version
    pub async fn get_package_info(
        &self,
        package_name: &str,
        current_version: &str,
    ) -> Option<PackageInfo> {
        let cache_key = CacheKey {
            package_name: package_name.to_string(),
            current_version: current_version.to_string(),
        };

        // Check cache first
        if let Some(cached) = self.cache.get(&cache_key) {
            if cached.fetched_at.elapsed() < CACHE_TTL {
                debug!("Cache hit for {}@{}", package_name, current_version);
                return Some(cached.clone());
            }
        }

        // Acquire semaphore permit for rate limiting
        let _permit = self.semaphore.acquire().await.ok()?;

        // Fetch from registry
        let url = format!("{}/{}", NPM_REGISTRY_URL, package_name);

        debug!("Fetching {}", url);

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to fetch {}: {}", package_name, e);
                return None;
            }
        };

        if !response.status().is_success() {
            warn!("Non-success status for {}: {}", package_name, response.status());
            return None;
        }

        let data: NpmPackageResponse = match response.json().await {
            Ok(d) => d,
            Err(e) => {
                warn!("Failed to parse response for {}: {}", package_name, e);
                return None;
            }
        };

        let dist_tags = data.dist_tags?;
        let current_track = detect_track(current_version, &dist_tags);
        let latest_on_track = get_version_for_track(&dist_tags, &current_track)?;

        let all_tracks = build_track_info(&dist_tags, data.time.as_ref());

        let repo_url = data.repository.as_ref().and_then(|r| r.get_url());
        let repo_directory = data.repository.as_ref().and_then(|r| r.get_directory());

        // Try to fetch changelog from GitHub with version range
        let changelog = if let Some(ref url) = repo_url {
            self.fetch_changelog(url, repo_directory.as_deref(), current_version, &latest_on_track).await
        } else {
            None
        };

        let info = PackageInfo {
            name: package_name.to_string(),
            current_track,
            current_version: current_version.to_string(),
            latest_on_track,
            all_tracks,
            repository_url: repo_url,
            repository_directory: repo_directory,
            changelog,
            fetched_at: Instant::now(),
        };

        // Update cache
        self.cache.insert(cache_key, info.clone());

        Some(info)
    }

    /// Fetch changelog from GitHub releases and CHANGELOG.md, merge both sources
    async fn fetch_changelog(
        &self,
        repo_url: &str,
        directory: Option<&str>,
        current_version: &str,
        latest_version: &str,
    ) -> Option<String> {
        let (owner, repo) = match parse_github_url(repo_url) {
            Some(parsed) => parsed,
            None => {
                debug!("Failed to parse GitHub URL: {}", repo_url);
                return None;
            }
        };

        debug!("Fetching changelog for {}/{} (dir: {:?}): {} -> {}", owner, repo, directory, current_version, latest_version);

        // Try package-specific changelog first if in monorepo
        let monorepo_changelog = if let Some(dir) = directory {
            self.fetch_monorepo_changelog(&owner, &repo, dir, current_version, latest_version)
                .await
        } else {
            None
        };

        // Fetch root changelog as fallback
        let (releases_result, root_changelog_result) = tokio::join!(
            self.fetch_github_releases(&owner, &repo, current_version, latest_version, directory),
            self.fetch_changelog_file(&owner, &repo, current_version, latest_version)
        );

        debug!("GitHub releases found: {}, CHANGELOG.md sections found: {}",
               releases_result.len(), root_changelog_result.len());

        // Merge all sources
        let merged = if let Some(ref mono_cl) = monorepo_changelog {
            // Use monorepo changelog if available
            mono_cl.clone()
        } else {
            merge_changelogs(releases_result, root_changelog_result, current_version, latest_version)
        };

        if merged.is_empty() {
            debug!("No changelog content after merge for {}/{}", owner, repo);
            None
        } else {
            debug!("Changelog content generated for {}/{}: {} chars", owner, repo, merged.len());
            Some(merged)
        }
    }

    /// Fetch changelog from monorepo package directory
    async fn fetch_monorepo_changelog(
        &self,
        owner: &str,
        repo: &str,
        directory: &str,
        current_version: &str,
        latest_version: &str,
    ) -> Option<String> {
        let files = ["CHANGELOG.md", "changelog.md", "HISTORY.md", "CHANGES.md"];
        let branches = ["main", "master"];

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

                if let Ok(response) = self.client.get(&url).send().await {
                    if response.status().is_success() {
                        if let Ok(content) = response.text().await {
                            let entries = extract_changelog_sections_since_version(
                                &content,
                                current_version,
                                latest_version,
                            );
                            if !entries.is_empty() {
                                debug!("Found monorepo changelog at {}", url);
                                return Some(format_changelog_entries(entries));
                            }
                        }
                    }
                }
            }
        }

        // Try GitHub releases with package-specific tags
        let package_name = directory.split('/').last().unwrap_or(directory);
        let releases = self.fetch_github_releases_with_prefix(
            owner, repo, current_version, latest_version, package_name
        ).await;

        if !releases.is_empty() {
            return Some(format_changelog_entries(releases));
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
    ) -> Vec<ChangelogEntry> {
        let current_semver = Version::parse(current_version).ok();
        let latest_semver = Version::parse(latest_version).ok();
        let use_version_filtering = current_semver.is_some() && latest_semver.is_some();

        let url = format!("https://github.com/{}/{}/releases.atom", owner, repo);

        let response = match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return vec![],
        };

        let content = match response.text().await {
            Ok(c) => c,
            Err(_) => return vec![],
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
                    if let (Some(ref current), Some(ref latest)) = (&current_semver, &latest_semver) {
                        if version > *current && version <= *latest {
                            entries.push(ChangelogEntry {
                                version,
                                version_string: title.to_string(),
                                body,
                            });
                        }
                    }
                } else {
                    entries.push(ChangelogEntry {
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
    ) -> Vec<ChangelogEntry> {
        // Try to parse current and latest versions (may fail for non-semver like r182)
        let current_semver = Version::parse(current_version).ok();
        let latest_semver = Version::parse(latest_version).ok();

        let use_version_filtering = current_semver.is_some() && latest_semver.is_some();

        // Use the atom feed instead of API - no rate limits!
        let url = format!("https://github.com/{}/{}/releases.atom", owner, repo);

        debug!("Fetching GitHub releases atom feed for {}/{}", owner, repo);

        let response = match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                debug!("GitHub releases atom feed returned status {} for {}/{}", r.status(), owner, repo);
                return vec![];
            }
            Err(e) => {
                debug!("Failed to fetch GitHub releases atom feed for {}/{}: {}", owner, repo, e);
                return vec![];
            }
        };

        let content = match response.text().await {
            Ok(c) => c,
            Err(e) => {
                debug!("Failed to read atom feed response for {}/{}: {}", owner, repo, e);
                return vec![];
            }
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
                let version = parsed_version.clone().unwrap_or_else(|| Version::new(0, 0, 0));
                fallback_entries.push(ChangelogEntry {
                    version,
                    version_string: title.to_string(),
                    body: body.clone(),
                });
            }

            // Try version-based filtering if we have valid semver versions
            if use_version_filtering {
                if let (Some(ref current), Some(ref latest), Some(ref version)) =
                    (&current_semver, &latest_semver, &parsed_version)
                {
                    // Filter: current <= version <= latest (include current version)
                    if version >= current && version <= latest {
                        entries.push(ChangelogEntry {
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
            debug!("Version filtering failed, using {} most recent releases for {}/{}",
                   fallback_entries.len(), owner, repo);
            // Fallback entries are already in order (newest first from atom feed)
            fallback_entries
        } else {
            vec![]
        };

        debug!("Parsed {} releases from atom feed for {}/{}", final_entries.len(), owner, repo);

        final_entries
    }

    /// Fetch and extract relevant sections from CHANGELOG.md
    async fn fetch_changelog_file(
        &self,
        owner: &str,
        repo: &str,
        current_version: &str,
        latest_version: &str,
    ) -> Vec<ChangelogEntry> {
        // Try common changelog file names
        let files = ["CHANGELOG.md", "changelog.md", "HISTORY.md", "CHANGES.md"];
        let branches = ["main", "master"];

        for file in &files {
            for branch in &branches {
                let url = format!(
                    "https://raw.githubusercontent.com/{}/{}/{}/{}",
                    owner, repo, branch, file
                );

                if let Ok(response) = self.client.get(&url).send().await {
                    if response.status().is_success() {
                        if let Ok(content) = response.text().await {
                            let entries = extract_changelog_sections_since_version(
                                &content,
                                current_version,
                                latest_version,
                            );
                            if !entries.is_empty() {
                                return entries;
                            }
                        }
                    }
                }
            }
        }

        vec![]
    }

}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
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
            for (tag, track_version) in &dist_tags.other_tags {
                if let Ok(track_ver) = Version::parse(track_version) {
                    // Check if major.minor.patch match and both have prerelease
                    if current_ver.major == track_ver.major
                        && current_ver.minor == track_ver.minor
                        && current_ver.patch == track_ver.patch
                        && track_ver.pre.is_empty() == current_ver.pre.is_empty()
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
    time_data: Option<&HashMap<String, String>>,
) -> Vec<TrackInfo> {
    let mut tracks = Vec::new();

    // Add latest first
    if let Some(ref version) = dist_tags.latest {
        let release_date = time_data.and_then(|t| {
            t.get(version)
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                .map(|dt| dt.with_timezone(&Utc))
        });

        tracks.push(TrackInfo {
            name: "latest".to_string(),
            version: version.clone(),
            release_date,
        });
    }

    // Add other tags
    for (name, version) in &dist_tags.other_tags {
        let release_date = time_data.and_then(|t| {
            t.get(version)
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                .map(|dt| dt.with_timezone(&Utc))
        });

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

/// Format time ago string
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
        format!("{} day{} ago", duration.num_days(), if duration.num_days() == 1 { "" } else { "s" })
    } else if duration.num_hours() > 0 {
        format!("{} hour{} ago", duration.num_hours(), if duration.num_hours() == 1 { "" } else { "s" })
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

/// Format exact date for older releases
pub fn format_exact_date(date: DateTime<Utc>) -> String {
    date.format("%b %d, %Y").to_string()
}

/// Format date with time ago for display
pub fn format_date_with_ago(date: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(date);

    if duration.num_days() > 90 {
        // Older than 3 months, show exact date
        format_exact_date(date)
    } else {
        format_time_ago(date)
    }
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
    let lines: Vec<&str> = result.lines()
        .map(|l| l.trim())
        .collect();

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
    if let Some(rest) = url.strip_prefix("github.com/").or_else(|| url.strip_prefix("github.com:")) {
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
                            version,
                            version_string: header.clone(),
                            body: body.clone(),
                        });
                    }

                    // Add to filtered entries if version is in range
                    if use_version_filtering {
                        if let (Some(ref current), Some(ref latest), Some(ref version)) =
                            (&current_semver, &latest_semver, &version_opt)
                        {
                            // Include current version too: current <= version <= latest
                            if version >= current && version <= latest {
                                entries.push(ChangelogEntry {
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
                    version,
                    version_string: header.clone(),
                    body: body.clone(),
                });
            }

            if use_version_filtering {
                if let (Some(ref current), Some(ref latest), Some(ref version)) =
                    (&current_semver, &latest_semver, &version_opt)
                {
                    // Include current version too: current <= version <= latest
                    if version >= current && version <= latest && entries.len() < MAX_CHANGELOG_VERSIONS {
                        entries.push(ChangelogEntry {
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
    let final_entries = if !entries.is_empty() {
        entries.sort_by(|a, b| b.version.cmp(&a.version));
        entries
    } else {
        // Fallback entries are already in document order (typically newest first)
        fallback_entries
    };

    final_entries
}

/// Format changelog entries to string
fn format_changelog_entries(entries: Vec<ChangelogEntry>) -> String {
    let mut output = Vec::new();

    for entry in entries {
        let header = if entry.version_string.starts_with('#') {
            entry.version_string.clone()
        } else {
            format!("## {}", entry.version_string)
        };

        output.push(header);

        let body = entry.body.trim();
        if !body.is_empty() {
            output.push(body.to_string());
        }

        output.push(String::new());
    }

    output.join("\n").trim().to_string()
}

/// Merge changelogs from GitHub releases and CHANGELOG.md file
fn merge_changelogs(
    releases: Vec<ChangelogEntry>,
    changelog_file: Vec<ChangelogEntry>,
    current_version: &str,
    _latest_version: &str,
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
        // Add version header
        let header = if entry.version_string.starts_with('#') {
            entry.version_string.clone()
        } else {
            format!("## {}", entry.version_string)
        };

        output.push(header);

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
            MAX_CHANGELOG_VERSIONS,
            total_count,
            current_version
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
    changelog: Option<String>,
    repository_url: Option<String>,
) -> VersionStatus {
    let current_parsed = match Version::parse(current_version) {
        Ok(v) => v,
        Err(_) => {
            return VersionStatus::Unknown {
                current_track: current_track.to_string(),
                current_version: current_version.to_string(),
                other_tracks: build_other_tracks(current_track, current_version, all_tracks),
                changelog,
                repository_url,
            }
        }
    };

    let latest_parsed = match Version::parse(latest_on_track) {
        Ok(v) => v,
        Err(_) => {
            return VersionStatus::Unknown {
                current_track: current_track.to_string(),
                current_version: current_version.to_string(),
                other_tracks: build_other_tracks(current_track, current_version, all_tracks),
                changelog,
                repository_url,
            }
        }
    };

    let other_tracks = build_other_tracks(current_track, current_version, all_tracks);

    // Check if there's a newer version on any track (including current track)
    let current_track_has_update = latest_parsed > current_parsed;
    let newer_on_other_track = other_tracks.iter().any(|t| t.is_newer);

    if !current_track_has_update && !newer_on_other_track {
        return VersionStatus::UpToDate {
            current_track: current_track.to_string(),
            current_version: current_version.to_string(),
            other_tracks,
            changelog,
            repository_url,
        };
    }

    // Determine which version is the "latest" we should highlight
    // Priority: current track update > newest on other tracks
    let (effective_latest, effective_track) = if current_track_has_update {
        (latest_on_track.to_string(), current_track.to_string())
    } else {
        // Find the newest version among other tracks
        let newest = other_tracks
            .iter()
            .filter(|t| t.is_newer)
            .max_by(|a, b| {
                let a_ver = Version::parse(&a.version).ok();
                let b_ver = Version::parse(&b.version).ok();
                match (a_ver, b_ver) {
                    (Some(av), Some(bv)) => av.cmp(&bv),
                    _ => std::cmp::Ordering::Equal,
                }
            });

        if let Some(newest) = newest {
            (newest.version.clone(), newest.name.clone())
        } else {
            // Fallback (shouldn't happen since we checked is_newer above)
            (latest_on_track.to_string(), current_track.to_string())
        }
    };

    let effective_latest_parsed = match Version::parse(&effective_latest) {
        Ok(v) => v,
        Err(_) => {
            return VersionStatus::Unknown {
                current_track: current_track.to_string(),
                current_version: current_version.to_string(),
                other_tracks,
                changelog,
                repository_url,
            }
        }
    };

    let severity = if effective_latest_parsed.major > current_parsed.major {
        UpdateSeverity::Major
    } else if effective_latest_parsed.minor > current_parsed.minor {
        UpdateSeverity::Minor
    } else {
        UpdateSeverity::Patch
    };

    VersionStatus::UpdateAvailable {
        current_track: current_track.to_string(),
        current_version: current_version.to_string(),
        latest_on_track: effective_latest,
        other_tracks,
        severity,
        changelog,
        repository_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                ("canary".to_string(), "55.0.0-canary-20260128-67ce8d5".to_string()),
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
        assert_eq!(detect_track("55.0.0-canary-20260128-67ce8d5", &dist_tags), "canary");
        // Latest track
        assert_eq!(detect_track("0.2.0-beta.9", &dist_tags), "latest");
    }

    #[test]
    fn test_format_time_ago() {
        let now = Utc::now();

        assert_eq!(format_time_ago(now - chrono::Duration::hours(2)), "2 hours ago");
        assert_eq!(format_time_ago(now - chrono::Duration::days(1)), "1 day ago");
        assert_eq!(format_time_ago(now - chrono::Duration::days(5)), "5 days ago");
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

        match check_version_status("1.0.0", "2.0.0", "latest", &tracks, None, None) {
            VersionStatus::UpdateAvailable { severity, other_tracks, .. } => {
                assert_eq!(severity, UpdateSeverity::Major);
                assert_eq!(other_tracks.len(), 1);
                assert_eq!(other_tracks[0].name, "next");
            }
            _ => panic!("Expected UpdateAvailable"),
        }

        match check_version_status("2.0.0", "2.0.0", "latest", &tracks, None, None) {
            VersionStatus::UpToDate { other_tracks, .. } => {
                assert_eq!(other_tracks.len(), 1);
            }
            _ => panic!("Expected UpToDate"),
        }
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
            Some(("eds2002".to_string(), "react-native-screen-transitions".to_string()))
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

        let entries = extract_changelog_sections_since_version(content, "19.1.0", "19.2.1");

        assert_eq!(entries.len(), 4); // 19.2.1, 19.2.0, 19.1.2, 19.1.1 (not 19.1.0)
        assert_eq!(entries[0].version, Version::parse("19.2.1").unwrap());
        assert_eq!(entries[1].version, Version::parse("19.2.0").unwrap());
        assert_eq!(entries[2].version, Version::parse("19.1.2").unwrap());
        assert_eq!(entries[3].version, Version::parse("19.1.1").unwrap());
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

        let entries = extract_changelog_sections_since_version(content, "1.0.0", "10.0.0");

        // All 8 versions should be included (10, 9, 8, 7, 6, 5, 4, 3) since MAX is now 15
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[0].version, Version::parse("10.0.0").unwrap());
        assert_eq!(entries[7].version, Version::parse("3.0.0").unwrap());
    }
}
