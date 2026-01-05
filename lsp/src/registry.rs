use dashmap::DashMap;
use semver::Version;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org";
const GITHUB_API_URL: &str = "https://api.github.com";
const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes
const MAX_CONCURRENT_REQUESTS: usize = 10;

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
    /// Get the symbol for this severity
    pub fn symbol(&self) -> &'static str {
        match self {
            UpdateSeverity::Major => "⬆️",  // Red double arrow for major
            UpdateSeverity::Minor => "↗️",  // Orange arrow for minor  
            UpdateSeverity::Patch => "↑",   // Simple arrow for patch
        }
    }

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

#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub name: String,
    pub latest_version: String,
    pub repository_url: Option<String>,
    pub changelog: Option<String>,
    pub fetched_at: Instant,
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
    cache: Arc<DashMap<String, PackageInfo>>,
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

    /// Get full package info including changelog
    pub async fn get_package_info(&self, package_name: &str) -> Option<PackageInfo> {
        // Check cache first
        if let Some(cached) = self.cache.get(package_name) {
            if cached.fetched_at.elapsed() < CACHE_TTL {
                debug!("Cache hit for {}", package_name);
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
        
        // Try to fetch changelog from GitHub
        let changelog = if let Some(ref url) = repo_url {
            self.fetch_changelog(url, &latest).await
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
        self.cache.insert(package_name.to_string(), info.clone());

        Some(info)
    }

    /// Fetch changelog from GitHub releases or CHANGELOG.md
    async fn fetch_changelog(&self, repo_url: &str, version: &str) -> Option<String> {
        let (owner, repo) = parse_github_url(repo_url)?;
        
        // Try GitHub releases API first
        if let Some(changelog) = self.fetch_github_release(&owner, &repo, version).await {
            return Some(changelog);
        }

        // Fall back to CHANGELOG.md
        self.fetch_changelog_file(&owner, &repo).await
    }

    /// Fetch release notes from GitHub releases API
    async fn fetch_github_release(&self, owner: &str, repo: &str, version: &str) -> Option<String> {
        // Try different tag formats
        let tags = [
            format!("v{}", version),
            version.to_string(),
        ];

        for tag in &tags {
            let url = format!("{}/repos/{}/{}/releases/tags/{}", GITHUB_API_URL, owner, repo, tag);
            
            debug!("Fetching GitHub release: {}", url);
            
            let response = match self.client
                .get(&url)
                .header("Accept", "application/vnd.github.v3+json")
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                _ => continue,
            };

            if let Ok(release) = response.json::<GitHubRelease>().await {
                if let Some(body) = release.body {
                    // Truncate if too long
                    let truncated = if body.len() > 500 {
                        format!("{}...", &body[..500])
                    } else {
                        body
                    };
                    return Some(truncated);
                }
            }
        }

        None
    }

    /// Fetch and extract relevant section from CHANGELOG.md
    async fn fetch_changelog_file(&self, owner: &str, repo: &str) -> Option<String> {
        // Try common changelog file names
        let files = ["CHANGELOG.md", "changelog.md", "HISTORY.md", "CHANGES.md"];
        
        for file in &files {
            let url = format!(
                "https://raw.githubusercontent.com/{}/{}/main/{}",
                owner, repo, file
            );
            
            if let Ok(response) = self.client.get(&url).send().await {
                if response.status().is_success() {
                    if let Ok(content) = response.text().await {
                        // Extract first section (usually latest version)
                        return extract_first_changelog_section(&content);
                    }
                }
            }
            
            // Also try master branch
            let url = format!(
                "https://raw.githubusercontent.com/{}/{}/master/{}",
                owner, repo, file
            );
            
            if let Ok(response) = self.client.get(&url).send().await {
                if response.status().is_success() {
                    if let Ok(content) = response.text().await {
                        return extract_first_changelog_section(&content);
                    }
                }
            }
        }

        None
    }

    /// Get latest versions for multiple packages concurrently
    pub async fn get_package_infos(&self, packages: &[String]) -> Vec<(String, Option<PackageInfo>)> {
        let futures: Vec<_> = packages
            .iter()
            .map(|name| {
                let registry = self.clone_inner();
                let name = name.clone();
                async move {
                    let info = registry.get_package_info(&name).await;
                    (name, info)
                }
            })
            .collect();

        futures::future::join_all(futures).await
    }

    fn clone_inner(&self) -> Self {
        Self {
            client: self.client.clone(),
            cache: Arc::clone(&self.cache),
            semaphore: Arc::clone(&self.semaphore),
        }
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

/// Extract the first section from a changelog
fn extract_first_changelog_section(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut in_section = false;
    let mut section_lines = Vec::new();
    
    for line in lines {
        // Look for version headers (## [x.x.x] or ## x.x.x)
        if line.starts_with("## ") || line.starts_with("# ") {
            if in_section {
                // We've hit the next section, stop
                break;
            }
            in_section = true;
            section_lines.push(line);
        } else if in_section {
            section_lines.push(line);
        }
    }

    if section_lines.is_empty() {
        return None;
    }

    let section = section_lines.join("\n");
    
    // Truncate if too long
    if section.len() > 500 {
        Some(format!("{}...", &section[..500]))
    } else {
        Some(section)
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
    }
}
