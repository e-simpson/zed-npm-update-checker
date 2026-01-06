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
const GITHUB_API_URL: &str = "https://api.github.com";
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
    
    /// Regex to extract version from git tags
    /// Matches various tag formats:
    /// - v1.0.0
    /// - 1.0.0
    /// - package@1.0.0 (monorepo style)
    /// - package-name@1.0.0-beta.1
    /// - @scope/package@1.0.0
    static ref VERSION_TAG_REGEX: Regex = Regex::new(
        r"(?:^v|@|^)(\d+\.\d+\.\d+(?:-[\w.]+)?(?:\+[\w.]+)?)$"
    ).unwrap();
}

/// Severity of version update
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSeverity {
    /// Major version bump (breaking changes) - e.g., 1.x.x → 2.x.x
    Major,
    /// Minor version bump (new features) - e.g., 1.1.x → 1.2.x
    Minor,
    /// Patch version bump (bug fixes) - e.g., 1.1.1 → 1.1.2
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

/// Result of comparing current version to latest
#[derive(Debug, Clone, PartialEq)]
pub enum VersionStatus {
    /// Currently checking version
    Checking,
    /// Package is up to date
    UpToDate,
    /// Update available with severity, changelog, and repository URL
    UpdateAvailable {
        latest: String,
        severity: UpdateSeverity,
        changelog: Option<String>,
        repository_url: Option<String>,
    },
    /// Could not determine (invalid version, fetch failed, etc.)
    Unknown,
}

/// A single changelog entry with version and content
#[derive(Debug, Clone)]
struct ChangelogEntry {
    version: Version,
    version_string: String,
    body: String,
}

#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub name: String,
    pub latest_version: String,
    pub repository_url: Option<String>,
    pub changelog: Option<String>,
    pub fetched_at: Instant,
}

/// Cache key includes both package name and current version for changelog relevance
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    package_name: String,
    current_version: String,
}

#[derive(Debug, Deserialize)]
struct NpmPackageResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: Option<DistTags>,
    repository: Option<Repository>,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Repository {
    String(String),
    Object { url: Option<String>, #[serde(rename = "type")] repo_type: Option<String> },
}

impl Repository {
    fn get_url(&self) -> Option<String> {
        match self {
            Repository::String(s) => Some(s.clone()),
            Repository::Object { url, .. } => url.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    published_at: Option<String>,
}

pub struct NpmRegistry {
    client: reqwest::Client,
    cache: Arc<DashMap<CacheKey, PackageInfo>>,
    semaphore: Arc<Semaphore>,
}

impl NpmRegistry {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("npm-update-checker-lsp")
            .build()
            .unwrap_or_default();

        Self {
            client,
            cache: Arc::new(DashMap::new()),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        }
    }

    /// Get full package info including changelog filtered by current version
    pub async fn get_package_info(&self, package_name: &str, current_version: &str) -> Option<PackageInfo> {
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

        let latest = data.dist_tags.and_then(|t| t.latest)?;
        let repo_url = data.repository.and_then(|r| r.get_url());
        
        // Try to fetch changelog from GitHub with version range
        let changelog = if let Some(ref url) = repo_url {
            self.fetch_changelog(url, current_version, &latest).await
        } else {
            None
        };

        let info = PackageInfo {
            name: package_name.to_string(),
            latest_version: latest,
            repository_url: repo_url,
            changelog,
            fetched_at: Instant::now(),
        };

        // Update cache
        self.cache.insert(cache_key, info.clone());

        Some(info)
    }

    /// Fetch changelog from GitHub releases and CHANGELOG.md, merge both sources
    async fn fetch_changelog(&self, repo_url: &str, current_version: &str, latest_version: &str) -> Option<String> {
        let (owner, repo) = match parse_github_url(repo_url) {
            Some(parsed) => parsed,
            None => {
                debug!("Failed to parse GitHub URL: {}", repo_url);
                return None;
            }
        };
        
        debug!("Fetching changelog for {}/{}: {} -> {}", owner, repo, current_version, latest_version);
        
        // Fetch from both sources in parallel
        let (releases_result, changelog_file_result) = tokio::join!(
            self.fetch_github_releases(&owner, &repo, current_version, latest_version),
            self.fetch_changelog_file(&owner, &repo, current_version, latest_version)
        );
        
        debug!("GitHub releases found: {}, CHANGELOG.md sections found: {}", 
               releases_result.len(), changelog_file_result.len());
        
        // Merge both sources
        let merged = merge_changelogs(releases_result, changelog_file_result, current_version, latest_version);
        
        if merged.is_empty() {
            debug!("No changelog content after merge for {}/{}", owner, repo);
            None
        } else {
            debug!("Changelog content generated for {}/{}: {} chars", owner, repo, merged.len());
            Some(merged)
        }
    }

    /// Fetch multiple release notes from GitHub releases API
    async fn fetch_github_releases(
        &self,
        owner: &str,
        repo: &str,
        current_version: &str,
        latest_version: &str,
    ) -> Vec<ChangelogEntry> {
        let current = match Version::parse(current_version) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to parse current version '{}': {}", current_version, e);
                return vec![];
            }
        };
        let latest = match Version::parse(latest_version) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to parse latest version '{}': {}", latest_version, e);
                return vec![];
            }
        };

        // Fetch releases list (GitHub returns up to 30 by default, which is plenty)
        let url = format!("{}/repos/{}/{}/releases?per_page=30", GITHUB_API_URL, owner, repo);
        
        debug!("Fetching GitHub releases for {}/{}: current={}, latest={}", owner, repo, current_version, latest_version);
        
        let response = match self.client
            .get(&url)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                let status = r.status();
                // Log rate limiting specifically
                if status.as_u16() == 403 {
                    warn!("GitHub API rate limit likely exceeded for {}/{}", owner, repo);
                } else {
                    debug!("GitHub releases API returned status {} for {}/{}", status, owner, repo);
                }
                return vec![];
            }
            Err(e) => {
                debug!("Failed to fetch GitHub releases for {}/{}: {}", owner, repo, e);
                return vec![];
            }
        };

        let releases: Vec<GitHubRelease> = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                debug!("Failed to parse GitHub releases for {}/{}: {}", owner, repo, e);
                return vec![];
            }
        };
        
        debug!("Fetched {} releases for {}/{}", releases.len(), owner, repo);

        let mut entries = Vec::new();

        for release in releases {
            // Parse version from tag
            let version = match parse_version_from_tag(&release.tag_name) {
                Some(v) => v,
                None => continue,
            };

            // Filter: current < version <= latest
            if version <= current || version > latest {
                continue;
            }

            if let Some(body) = release.body {
                if !body.trim().is_empty() {
                    let version_string = release.name.unwrap_or_else(|| release.tag_name.clone());
                    entries.push(ChangelogEntry {
                        version: version.clone(),
                        version_string,
                        body,
                    });
                }
            }

            // Stop if we have enough
            if entries.len() >= MAX_CHANGELOG_VERSIONS {
                break;
            }
        }

        // Sort by version descending (newest first)
        entries.sort_by(|a, b| b.version.cmp(&a.version));

        entries
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

/// Parse version from a git tag
fn parse_version_from_tag(tag: &str) -> Option<Version> {
    // First try the regex for complex tags
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
    
    // Fall back to simple parsing (v1.0.0 or 1.0.0)
    let cleaned = tag.trim_start_matches('v');
    Version::parse(cleaned).ok()
}

/// Extract changelog sections for versions between current and latest
fn extract_changelog_sections_since_version(
    content: &str,
    current_version: &str,
    latest_version: &str,
) -> Vec<ChangelogEntry> {
    let current = match Version::parse(current_version) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let latest = match Version::parse(latest_version) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut entries = Vec::new();
    let mut current_entry: Option<(Version, String, Vec<String>)> = None;
    
    for line in content.lines() {
        // Check if this is a version header
        if line.starts_with("## ") || line.starts_with("# ") {
            // Save the previous entry if it's in range
            if let Some((version, header, lines)) = current_entry.take() {
                if version > current && version <= latest {
                    entries.push(ChangelogEntry {
                        version,
                        version_string: header,
                        body: lines.join("\n"),
                    });
                    
                    if entries.len() >= MAX_CHANGELOG_VERSIONS {
                        break;
                    }
                }
            }
            
            // Start a new entry if we can parse the version
            if let Some(version) = parse_version_from_header(line) {
                // Only track versions that could be relevant
                if version > current && version <= latest {
                    current_entry = Some((version, line.to_string(), Vec::new()));
                } else if version <= current {
                    // We've gone past the relevant versions, stop
                    break;
                }
            }
        } else if let Some((_, _, ref mut lines)) = current_entry {
            lines.push(line.to_string());
        }
    }
    
    // Don't forget the last entry
    if let Some((version, header, lines)) = current_entry {
        if version > current && version <= latest && entries.len() < MAX_CHANGELOG_VERSIONS {
            entries.push(ChangelogEntry {
                version,
                version_string: header,
                body: lines.join("\n"),
            });
        }
    }

    // Sort by version descending (newest first)
    entries.sort_by(|a, b| b.version.cmp(&a.version));

    entries
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

/// Compare versions and determine update severity
pub fn check_version_status(current: &str, latest: &str, changelog: Option<String>, repository_url: Option<String>) -> VersionStatus {
    let current_parsed = match Version::parse(current) {
        Ok(v) => v,
        Err(_) => return VersionStatus::Unknown,
    };

    let latest_parsed = match Version::parse(latest) {
        Ok(v) => v,
        Err(_) => return VersionStatus::Unknown,
    };

    if latest_parsed <= current_parsed {
        return VersionStatus::UpToDate;
    }

    // Determine severity
    let severity = if latest_parsed.major > current_parsed.major {
        UpdateSeverity::Major
    } else if latest_parsed.minor > current_parsed.minor {
        UpdateSeverity::Minor
    } else {
        UpdateSeverity::Patch
    };

    VersionStatus::UpdateAvailable {
        latest: latest.to_string(),
        severity,
        changelog,
        repository_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_status() {
        assert_eq!(
            check_version_status("1.0.0", "1.0.0", None, None),
            VersionStatus::UpToDate
        );
        
        match check_version_status("1.0.0", "1.0.1", None, None) {
            VersionStatus::UpdateAvailable { severity, .. } => {
                assert_eq!(severity, UpdateSeverity::Patch);
            }
            _ => panic!("Expected UpdateAvailable"),
        }
        
        match check_version_status("1.0.0", "1.1.0", None, None) {
            VersionStatus::UpdateAvailable { severity, .. } => {
                assert_eq!(severity, UpdateSeverity::Minor);
            }
            _ => panic!("Expected UpdateAvailable"),
        }
        
        match check_version_status("1.0.0", "2.0.0", None, None) {
            VersionStatus::UpdateAvailable { severity, .. } => {
                assert_eq!(severity, UpdateSeverity::Major);
            }
            _ => panic!("Expected UpdateAvailable"),
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
    fn test_version_comparison_zod() {
        // Test that zod's version range works correctly
        let current = Version::parse("3.22.0").unwrap();
        let latest = Version::parse("4.3.5").unwrap();
        let release_version = Version::parse("4.3.5").unwrap();
        
        // The filter is: current < version <= latest
        // 4.3.5 should be included because 3.22.0 < 4.3.5 <= 4.3.5
        assert!(release_version > current);
        assert!(release_version <= latest);
        
        // Test that version 3.22.0 would NOT be included
        let same_as_current = Version::parse("3.22.0").unwrap();
        assert!(!(same_as_current > current)); // 3.22.0 > 3.22.0 is false
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
