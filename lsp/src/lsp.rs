use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{self, *};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tracing::{debug, info};

use crate::parser::{Dependency, parse_package_json};
use crate::registry::{
    NpmRegistry, PackageVersionInfo, RegistryConfig, TrackUpdate, VersionRelease, VersionStatus,
    check_version_status, prerelease_track_label,
};
use crate::settings::{ExtensionSettings, ExtensionSettingsPatch};

const LSP_NAME: &str = "npm-package-json-checker-lsp";
const RESULT_BATCH_SIZE: usize = 4;
const RESULT_BATCH_MAX_WAIT: Duration = Duration::from_millis(100);

/// State of a dependency check
#[derive(Debug, Clone)]
enum CheckState {
    /// Currently checking this package
    Checking,
    /// Check completed with result
    Done(VersionStatus),
}

/// Cached state for a document
#[derive(Debug, Clone)]
struct DocumentState {
    /// Parsed dependencies
    dependencies: Vec<Dependency>,
    /// Check states for each package (package_name -> state)
    check_states: HashMap<String, CheckState>,
    document_version: i32,
    generation: u64,
}

#[derive(Clone)]
struct Backend {
    client: Client,
    registry: Arc<NpmRegistry>,
    documents: Arc<RwLock<HashMap<Uri, DocumentState>>>,
    settings: Arc<RwLock<ExtensionSettings>>,
    document_tasks: Arc<Mutex<HashMap<Uri, JoinHandle<()>>>>,
    next_generation: Arc<AtomicU64>,
}

struct ChangelogRequest {
    package_name: String,
    current_version: String,
    latest_version: String,
    repository_url: String,
    repository_directory: Option<String>,
    version_publish_dates: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        let settings = ExtensionSettings::default();
        Self {
            client,
            registry: Arc::new(NpmRegistry::new(Self::registry_config(&settings))),
            documents: Arc::new(RwLock::new(HashMap::new())),
            settings: Arc::new(RwLock::new(settings)),
            document_tasks: Arc::new(Mutex::new(HashMap::new())),
            next_generation: Arc::new(AtomicU64::new(1)),
        }
    }

    fn registry_config(settings: &ExtensionSettings) -> RegistryConfig {
        RegistryConfig {
            registry_url: settings.registry_url.clone(),
            cache_ttl: std::time::Duration::from_secs(settings.cache_ttl_seconds),
            max_concurrent_requests: settings.max_concurrent_requests,
            max_concurrent_changelog_requests: settings.max_concurrent_changelog_requests,
            request_timeout: std::time::Duration::from_secs(settings.request_timeout_seconds),
            date_display: settings.date_display.clone(),
        }
    }

    async fn update_settings(&self, patch: ExtensionSettingsPatch) {
        let updated = {
            let mut settings = self.settings.write().await;
            settings.apply_patch(patch);
            settings.clone()
        };

        self.registry.apply_config(Self::registry_config(&updated));
    }

    async fn replace_settings(&self, new_settings: ExtensionSettings) {
        {
            let mut settings = self.settings.write().await;
            *settings = new_settings.clone();
        }

        self.registry
            .apply_config(Self::registry_config(&new_settings));
    }

    async fn current_settings(&self) -> ExtensionSettings {
        self.settings.read().await.clone()
    }

    async fn refresh_all_diagnostics(&self) {
        let uris: Vec<Uri> = {
            let docs = self.documents.read().await;
            docs.keys().cloned().collect()
        };

        for uri in uris {
            self.publish_diagnostics(&uri).await;
        }
    }

    /// Check if the file is a package.json
    fn is_package_json(uri: &Uri) -> bool {
        uri.path().as_str().rsplit('/').next() == Some("package.json")
    }

    async fn schedule_document(
        &self,
        uri: Uri,
        text: String,
        document_version: i32,
        debounce: bool,
    ) {
        if !Self::is_package_json(&uri) {
            return;
        }

        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let mut tasks = self.document_tasks.lock().await;
        if let Some(previous) = tasks.remove(&uri) {
            previous.abort();
        }

        let backend = self.clone();
        let task_uri = uri.clone();
        let task = tokio::spawn(async move {
            if debounce {
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
            backend
                .process_document(task_uri, text, document_version, generation)
                .await;
        });
        tasks.insert(uri, task);
    }

    /// Process one document generation. Dropping this future cancels all in-flight HTTP futures.
    async fn process_document(
        &self,
        uri: Uri,
        text: String,
        document_version: i32,
        generation: u64,
    ) {
        debug!("Processing {:?} generation {}", uri, generation);

        let new_dependencies = parse_package_json(&text);

        if new_dependencies.is_empty() {
            {
                let mut docs = self.documents.write().await;
                docs.insert(
                    uri.clone(),
                    DocumentState {
                        dependencies: vec![],
                        check_states: HashMap::new(),
                        document_version,
                        generation,
                    },
                );
            }
            self.client
                .publish_diagnostics(uri.clone(), Vec::new(), Some(document_version))
                .await;
            self.refresh_inlay_hints().await;
            return;
        }

        // Get existing state to determine what needs re-checking
        let existing_deps: HashMap<String, (String, CheckState)> = {
            let docs = self.documents.read().await;
            docs.get(&uri)
                .map(|state| {
                    state
                        .dependencies
                        .iter()
                        .filter_map(|d| {
                            state
                                .check_states
                                .get(&d.name)
                                .map(|cs| (d.name.clone(), (d.version.clone(), cs.clone())))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        // Determine which packages need checking vs. can be preserved
        let mut check_states = HashMap::new();
        let mut needs_fetch = Vec::new();

        for dep in &new_dependencies {
            if let Some((old_version, CheckState::Done(old_status))) = existing_deps.get(&dep.name)
            {
                if old_version == &dep.version {
                    // Version unchanged - preserve existing check state
                    debug!("Preserving cached state for {} @ {}", dep.name, dep.version);
                    check_states.insert(dep.name.clone(), CheckState::Done(old_status.clone()));
                } else {
                    // Version changed - needs re-checking
                    debug!(
                        "Version changed for {}: {} -> {}",
                        dep.name, old_version, dep.version
                    );
                    check_states.insert(dep.name.clone(), CheckState::Checking);
                    needs_fetch.push(dep.clone());
                }
            } else {
                // New dependency - needs checking
                debug!("New dependency: {} @ {}", dep.name, dep.version);
                check_states.insert(dep.name.clone(), CheckState::Checking);
                needs_fetch.push(dep.clone());
            }
        }

        // Store state (preserved + loading indicators for new/changed)
        {
            let mut docs = self.documents.write().await;
            docs.insert(
                uri.clone(),
                DocumentState {
                    dependencies: new_dependencies.clone(),
                    check_states,
                    document_version,
                    generation,
                },
            );
        }

        if needs_fetch.is_empty() {
            self.publish_diagnostics(&uri).await;
            return;
        }
        self.refresh_inlay_hints().await;

        let mut version_futures: FuturesUnordered<
            BoxFuture<'static, (Dependency, Option<crate::registry::PackageVersionInfo>)>,
        > = FuturesUnordered::new();
        for dep in needs_fetch {
            let registry = self.registry.clone();
            version_futures.push(Box::pin(async move {
                let info = registry
                    .get_package_version_info(&dep.name, &dep.clean_version)
                    .await;
                (dep, info)
            }));
        }

        let mut changelog_futures: FuturesUnordered<
            BoxFuture<'static, (String, String, Option<String>)>,
        > = FuturesUnordered::new();
        let mut unflushed = 0usize;
        let mut published_first_result = false;
        let mut last_flush = Instant::now();
        let mut changelog_unflushed = 0usize;
        let mut published_first_changelog = false;
        let mut last_changelog_flush = Instant::now();

        while !version_futures.is_empty() || !changelog_futures.is_empty() {
            tokio::select! {
                Some((dep, package_info)) = version_futures.next(), if !version_futures.is_empty() => {
                    let (status, changelog_request) = build_initial_status(&dep, package_info);
                    if !self.store_status_if_current(&uri, generation, &dep, status).await {
                        return;
                    }
                    unflushed += 1;

                    if let Some(request) = changelog_request {
                        let registry = self.registry.clone();
                        let package_name = request.package_name.clone();
                        let current_version = request.current_version.clone();
                        changelog_futures.push(Box::pin(async move {
                            let changelog = registry
                                .fetch_changelog_for_package(
                                    &request.package_name,
                                    &request.current_version,
                                    &request.latest_version,
                                    &request.repository_url,
                                    request.repository_directory.as_deref(),
                                    Some(&request.version_publish_dates),
                                )
                                .await;
                            (package_name, current_version, changelog)
                        }));
                    }

                    if should_publish_batch(
                        published_first_result,
                        unflushed,
                        last_flush.elapsed(),
                        version_futures.is_empty(),
                    ) {
                        self.publish_progress(&uri, generation).await;
                        published_first_result = true;
                        unflushed = 0;
                        last_flush = Instant::now();
                    }
                }
                Some((package_name, current_version, changelog)) = changelog_futures.next(), if !changelog_futures.is_empty() => {
                    if !self.store_changelog_if_current(
                        &uri,
                        generation,
                        &package_name,
                        &current_version,
                        changelog,
                    ).await {
                        return;
                    }
                    changelog_unflushed += 1;
                    if should_publish_batch(
                        published_first_changelog,
                        changelog_unflushed,
                        last_changelog_flush.elapsed(),
                        changelog_futures.is_empty(),
                    ) {
                        self.publish_diagnostics(&uri).await;
                        published_first_changelog = true;
                        changelog_unflushed = 0;
                        last_changelog_flush = Instant::now();
                    }
                }
            }
        }

        if unflushed > 0 {
            self.publish_progress(&uri, generation).await;
        }
    }

    async fn store_status_if_current(
        &self,
        uri: &Uri,
        generation: u64,
        dependency: &Dependency,
        status: VersionStatus,
    ) -> bool {
        let mut docs = self.documents.write().await;
        let Some(state) = docs.get_mut(uri) else {
            return false;
        };
        if state.generation != generation
            || !state.dependencies.iter().any(|candidate| {
                candidate.name == dependency.name && candidate.version == dependency.version
            })
        {
            return false;
        }
        state
            .check_states
            .insert(dependency.name.clone(), CheckState::Done(status));
        true
    }

    async fn store_changelog_if_current(
        &self,
        uri: &Uri,
        generation: u64,
        package_name: &str,
        current_version: &str,
        changelog: Option<String>,
    ) -> bool {
        let mut docs = self.documents.write().await;
        let Some(state) = docs.get_mut(uri) else {
            return false;
        };
        if state.generation != generation
            || !state.dependencies.iter().any(|dependency| {
                dependency.name == package_name && dependency.clean_version == current_version
            })
        {
            return false;
        }
        if let Some(CheckState::Done(status)) = state.check_states.get_mut(package_name) {
            set_changelog(status, changelog);
        }
        true
    }

    async fn publish_progress(&self, uri: &Uri, generation: u64) {
        let is_current = self
            .documents
            .read()
            .await
            .get(uri)
            .is_some_and(|state| state.generation == generation);
        if is_current {
            self.refresh_inlay_hints().await;
            self.publish_diagnostics(uri).await;
        }
    }

    async fn refresh_inlay_hints(&self) {
        if self.current_settings().await.show_loading_hints {
            let _ = self
                .client
                .send_request::<ls_types::request::InlayHintRefreshRequest>(())
                .await;
        }
    }

    /// Publish diagnostics for outdated packages
    async fn publish_diagnostics(&self, uri: &Uri) {
        let settings = self.current_settings().await;
        let docs = self.documents.read().await;
        let Some(state) = docs.get(uri) else {
            return;
        };

        let mut diagnostics = Vec::new();

        for dep in &state.dependencies {
            if let Some(CheckState::Done(VersionStatus::UpdateAvailable {
                latest_on_track,
                severity,
                current_track,
                current_track_release_date: _,
                current_version,
                other_tracks,
                ..
            })) = state.check_states.get(&dep.name)
            {
                let current_pre_label = prerelease_track_label(current_version);
                let latest_pre_label = prerelease_track_label(latest_on_track);

                // For prerelease channels, only surface updates that stay on the same channel.
                if !(current_pre_label.is_some() && current_pre_label != latest_pre_label) {
                    let update_label = current_pre_label
                        .filter(|label| Some(*label) == latest_pre_label)
                        .unwrap_or_else(|| severity.label());

                    let message = format!("{} → {}", update_label, latest_on_track);

                    let diagnostic_severity = if current_pre_label.is_some() {
                        DiagnosticSeverity::HINT
                    } else {
                        DiagnosticSeverity::INFORMATION
                    };

                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position {
                                line: dep.line,
                                character: dep.version_start_col,
                            },
                            end: Position {
                                line: dep.line,
                                character: dep.version_end_col,
                            },
                        },
                        severity: Some(diagnostic_severity),
                        code: Some(NumberOrString::String("outdated-dependency".to_string())),
                        source: Some(LSP_NAME.to_string()),
                        message,
                        related_information: None,
                        tags: None,
                        code_description: None,
                        data: Some(serde_json::json!({
                            "package": dep.name,
                            "current": dep.version,
                            "latest": latest_on_track,
                            "severity": update_label,
                            "track": current_track,
                        })),
                    });
                }

                if settings.show_experimental_tracks {
                    let newer_other_tracks = collect_newer_track_summaries(other_tracks, &settings);

                    if !newer_other_tracks.is_empty() {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line: dep.line,
                                    character: dep.version_start_col,
                                },
                                end: Position {
                                    line: dep.line,
                                    character: dep.version_end_col,
                                },
                            },
                            severity: Some(DiagnosticSeverity::HINT),
                            code: Some(NumberOrString::String(
                                "outdated-dependency-alt-track".to_string(),
                            )),
                            source: Some(LSP_NAME.to_string()),
                            message: newer_other_tracks.join(", "),
                            related_information: None,
                            tags: None,
                            code_description: None,
                            data: Some(serde_json::json!({
                                "package": dep.name,
                                "track_updates": newer_other_tracks,
                            })),
                        });
                    }
                }
            }
        }

        let document_version = state.document_version;
        drop(docs);

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, Some(document_version))
            .await;
    }

    /// Generate inlay hints for a document
    async fn generate_inlay_hints(&self, uri: &Uri) -> Vec<InlayHint> {
        if !self.current_settings().await.show_loading_hints {
            return vec![];
        }

        let docs = self.documents.read().await;
        let Some(state) = docs.get(uri) else {
            return vec![];
        };

        let mut hints = Vec::new();

        for dep in &state.dependencies {
            let check_state = state.check_states.get(&dep.name);

            // Only show ⏳ while checking - no indicators after check completes
            if matches!(check_state, Some(CheckState::Checking)) {
                hints.push(InlayHint {
                    position: Position {
                        line: dep.line,
                        // Position before the version string (after the opening quote)
                        character: dep.version_start_col,
                    },
                    label: InlayHintLabel::String("⏳ ".to_string()),
                    kind: None,
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(false),
                    padding_right: Some(true),
                    data: None,
                });
            }
        }

        hints
    }

    /// Generate code actions for a range
    async fn generate_code_actions(&self, uri: &Uri, range: Range) -> Vec<CodeActionOrCommand> {
        let settings = self.current_settings().await;
        let docs = self.documents.read().await;
        let Some(state) = docs.get(uri) else {
            return vec![];
        };

        let mut actions = Vec::new();

        for dep in &state.dependencies {
            // Check if this dependency is in the requested range
            if dep.line < range.start.line || dep.line > range.end.line {
                continue;
            }

            if let Some(CheckState::Done(status)) = state.check_states.get(&dep.name) {
                match status {
                    VersionStatus::UpdateAvailable {
                        latest_on_track,
                        severity,
                        current_track,
                        current_track_release_date,
                        recent_current_track_releases,
                        other_tracks,
                        ..
                    } => {
                        // 1. Update on current track (preferred)
                        let type_label = prerelease_track_label(latest_on_track)
                            .unwrap_or_else(|| severity.label());
                        let current_track_time = current_track_release_date
                            .as_ref()
                            .copied()
                            .map(|date| {
                                format!(" ({})", settings.date_display.format_date_tag(date))
                            })
                            .unwrap_or_default();
                        let preferred_title = if current_track == "latest" {
                            format!(
                                "{}: Update to latest {}{}",
                                type_label, latest_on_track, current_track_time
                            )
                        } else if let Some(date) = current_track_release_date {
                            format!(
                                "{}: Update to {} ({} track, {})",
                                type_label,
                                latest_on_track,
                                current_track,
                                settings.date_display.format_date_tag(*date)
                            )
                        } else {
                            format!(
                                "{}: Update to {} ({} track)",
                                type_label, latest_on_track, current_track
                            )
                        };

                        let update_action = create_update_action(
                            uri,
                            dep,
                            latest_on_track,
                            &preferred_title,
                            true, // is_preferred
                        );
                        actions.push(CodeActionOrCommand::CodeAction(update_action));

                        // 2. Fallback options on current track (older than latest, newer than current)
                        for release in recent_release_actions(
                            recent_current_track_releases,
                            settings.recent_releases_in_code_actions,
                        ) {
                            let title = format!(
                                "- Update to {} {} ({})",
                                current_track,
                                release.version,
                                settings.date_display.format_date_tag(release.release_date)
                            );

                            let fallback_action =
                                create_update_action(uri, dep, &release.version, &title, false);
                            actions.push(CodeActionOrCommand::CodeAction(fallback_action));
                        }

                        // 3. Alternative track switches
                        // Sort: current track first, then by release date/version
                        let mut sorted_tracks: Vec<&TrackUpdate> = other_tracks.iter().collect();

                        // Put stable track first if not on it
                        if current_track != "latest" {
                            if let Some(latest_idx) =
                                sorted_tracks.iter().position(|t| t.name == "latest")
                            {
                                let latest_track = sorted_tracks.remove(latest_idx);
                                sorted_tracks.insert(0, latest_track);
                            }
                        }

                        for track in sorted_tracks {
                            let date_str = if let Some(date) = track.release_date {
                                format!(" ({})", settings.date_display.format_date_tag(date))
                            } else {
                                String::new()
                            };

                            let switch_action = create_update_action(
                                uri,
                                dep,
                                &track.version,
                                &format!("Switch to {}: {}{}", track.name, track.version, date_str),
                                false, // not preferred
                            );
                            actions.push(CodeActionOrCommand::CodeAction(switch_action));
                        }
                    }
                    VersionStatus::UpToDate {
                        current_track,
                        other_tracks,
                        ..
                    } => {
                        // Still show options to switch tracks even if up to date
                        let mut sorted_tracks: Vec<&TrackUpdate> = other_tracks.iter().collect();

                        // Put stable track first if not on it
                        if current_track != "latest" {
                            if let Some(latest_idx) =
                                sorted_tracks.iter().position(|t| t.name == "latest")
                            {
                                let latest_track = sorted_tracks.remove(latest_idx);
                                sorted_tracks.insert(0, latest_track);
                            }
                        }

                        for track in sorted_tracks {
                            let date_str = if let Some(date) = track.release_date {
                                format!(" ({})", settings.date_display.format_date_tag(date))
                            } else {
                                String::new()
                            };

                            let switch_action = create_update_action(
                                uri,
                                dep,
                                &track.version,
                                &format!("Switch to {}: {}{}", track.name, track.version, date_str),
                                false,
                            );
                            actions.push(CodeActionOrCommand::CodeAction(switch_action));
                        }
                    }
                    _ => {}
                }
            }
        }

        // Add "Update all" action if there are multiple outdated deps on the current page
        let outdated_deps: Vec<_> = state
            .dependencies
            .iter()
            .filter(|d| {
                if d.line < range.start.line || d.line > range.end.line {
                    return false;
                }
                matches!(
                    state.check_states.get(&d.name),
                    Some(CheckState::Done(VersionStatus::UpdateAvailable { .. }))
                )
            })
            .collect();

        if outdated_deps.len() > 1 {
            let mut all_edits = Vec::new();

            for dep in &outdated_deps {
                if let Some(CheckState::Done(VersionStatus::UpdateAvailable {
                    latest_on_track,
                    ..
                })) = state.check_states.get(&dep.name)
                {
                    all_edits.push(TextEdit {
                        range: Range {
                            start: Position {
                                line: dep.line,
                                character: dep.version_start_col,
                            },
                            end: Position {
                                line: dep.line,
                                character: dep.version_end_col,
                            },
                        },
                        new_text: format_updated_version(&dep.version, latest_on_track),
                    });
                }
            }

            if !all_edits.is_empty() {
                let mut changes = HashMap::new();
                changes.insert(uri.clone(), all_edits);

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Update all {} outdated packages", outdated_deps.len()),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    command: None,
                    is_preferred: Some(false),
                    disabled: None,
                    data: None,
                }));
            }
        }

        actions
    }
}

/// Create an update code action
fn create_update_action(
    uri: &Uri,
    dep: &Dependency,
    new_version: &str,
    title: &str,
    is_preferred: bool,
) -> CodeAction {
    let edit = TextEdit {
        range: Range {
            start: Position {
                line: dep.line,
                character: dep.version_start_col,
            },
            end: Position {
                line: dep.line,
                character: dep.version_end_col,
            },
        },
        new_text: format_updated_version(&dep.version, new_version),
    };

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    CodeAction {
        title: title.to_string(),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        command: None,
        is_preferred: Some(is_preferred),
        disabled: None,
        data: None,
    }
}

fn collect_newer_track_summaries(
    other_tracks: &[TrackUpdate],
    settings: &ExtensionSettings,
) -> Vec<String> {
    let _ = settings;

    other_tracks
        .iter()
        .filter(|track| track.is_newer)
        .map(|track| {
            let label = prerelease_track_label(&track.version)
                .map(str::to_string)
                .unwrap_or_else(|| track.name.to_ascii_uppercase());

            format!("{} → {}", label, track.version)
        })
        .collect()
}

fn recent_release_actions(releases: &[VersionRelease], limit: usize) -> Vec<&VersionRelease> {
    if limit == 0 {
        return Vec::new();
    }

    releases.iter().take(limit).collect()
}

/// Format the updated version, preserving the prefix (^, ~, etc.)
fn format_updated_version(current: &str, latest: &str) -> String {
    if current.starts_with('^') {
        format!("^{}", latest)
    } else if current.starts_with('~') {
        format!("~{}", latest)
    } else if current.starts_with(">=") {
        format!(">={}", latest)
    } else {
        latest.to_string()
    }
}

/// Clean up repository URL for display
fn clean_repo_url(url: &str) -> String {
    url.trim()
        .trim_start_matches("git+")
        .trim_start_matches("git://")
        .trim_end_matches(".git")
        .replace("git@github.com:", "https://github.com/")
        .to_string()
}

/// Extract owner/repo from GitHub URL for display
fn extract_github_owner_repo(url: &str) -> Option<(String, String)> {
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

fn build_initial_status(
    dependency: &Dependency,
    package_info: Option<PackageVersionInfo>,
) -> (VersionStatus, Option<ChangelogRequest>) {
    let Some(package_info) = package_info else {
        return (
            VersionStatus::Unknown {
                current_track: "latest".to_string(),
                current_version: dependency.clean_version.clone(),
                current_track_release_date: None,
                recent_current_track_releases: vec![],
                other_tracks: vec![],
                changelog: None,
                repository_url: None,
            },
            None,
        );
    };

    if dependency.clean_version.is_empty() {
        return (
            VersionStatus::Unknown {
                current_track: "latest".to_string(),
                current_version: dependency.clean_version.clone(),
                current_track_release_date: None,
                recent_current_track_releases: vec![],
                other_tracks: vec![],
                changelog: None,
                repository_url: package_info.repository_url,
            },
            None,
        );
    }

    let status = check_version_status(
        &dependency.clean_version,
        &package_info.latest_on_track,
        &package_info.current_track,
        &package_info.all_tracks,
        &package_info.recent_current_track_releases,
        None,
        package_info.repository_url.clone(),
    );
    let latest_version = match &status {
        VersionStatus::UpdateAvailable {
            latest_on_track, ..
        } => latest_on_track.clone(),
        _ => dependency.clean_version.clone(),
    };
    let changelog_request = package_info
        .repository_url
        .map(|repository_url| ChangelogRequest {
            package_name: dependency.name.clone(),
            current_version: dependency.clean_version.clone(),
            latest_version,
            repository_url,
            repository_directory: package_info.repository_directory,
            version_publish_dates: package_info.version_publish_dates,
        });

    (status, changelog_request)
}

fn set_changelog(status: &mut VersionStatus, changelog: Option<String>) {
    match status {
        VersionStatus::UpToDate {
            changelog: current, ..
        }
        | VersionStatus::UpdateAvailable {
            changelog: current, ..
        }
        | VersionStatus::Unknown {
            changelog: current, ..
        } => *current = changelog,
    }
}

fn should_publish_batch(
    published_first_result: bool,
    pending_results: usize,
    elapsed: Duration,
    version_stage_complete: bool,
) -> bool {
    pending_results > 0
        && (!published_first_result
            || pending_results >= RESULT_BATCH_SIZE
            || elapsed >= RESULT_BATCH_MAX_WAIT
            || version_stage_complete)
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let initial_settings =
            ExtensionSettings::from_value_or_default(params.initialization_options.as_ref());
        self.replace_settings(initial_settings).await;

        info!("{} initializing", LSP_NAME);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: LSP_NAME.to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
                    InlayHintOptions {
                        resolve_provider: Some(false),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: None,
                        },
                    },
                ))),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: None,
                        },
                        resolve_provider: None,
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("{} initialized", LSP_NAME);
    }

    async fn shutdown(&self) -> Result<()> {
        info!("{} shutting down", LSP_NAME);
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        debug!("did_open: {:?}", params.text_document.uri);
        let document = params.text_document;
        self.schedule_document(document.uri, document.text, document.version, false)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        debug!("did_change: {:?}", params.text_document.uri);
        if let Some(change) = params.content_changes.into_iter().next() {
            self.schedule_document(
                params.text_document.uri,
                change.text,
                params.text_document.version,
                true,
            )
            .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        debug!("did_save: {:?}", params.text_document.uri);
        // FULL-sync didChange is authoritative and carries a document version.
        // Reprocessing optional save text would risk publishing it with an older
        // version while a debounced change is still pending.
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        debug!("did_close: {:?}", params.text_document.uri);
        if let Some(task) = self
            .document_tasks
            .lock()
            .await
            .remove(&params.text_document.uri)
        {
            task.abort();
        }
        self.documents
            .write()
            .await
            .remove(&params.text_document.uri);
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
        self.refresh_inlay_hints().await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        if let Some(patch) = ExtensionSettingsPatch::from_value(&params.settings) {
            self.update_settings(patch).await;
            self.refresh_all_diagnostics().await;

            let _ = self
                .client
                .send_request::<ls_types::request::InlayHintRefreshRequest>(())
                .await;
        }
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        debug!("inlay_hint request for: {:?}", params.text_document.uri);
        let hints = self.generate_inlay_hints(&params.text_document.uri).await;
        debug!("inlay_hint returning {} hints", hints.len());
        for hint in &hints {
            debug!("  hint at {:?}: {:?}", hint.position, hint.label);
        }
        Ok(Some(hints))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        debug!("code_action: {:?}", params.text_document.uri);
        let actions = self
            .generate_code_actions(&params.text_document.uri, params.range)
            .await;
        Ok(Some(actions))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let settings = self.current_settings().await;
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let docs = self.documents.read().await;
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };

        // Find if we're hovering over a dependency line (package name or version)
        for dep in &state.dependencies {
            // Check if cursor is on the package name OR version string
            let on_name =
                position.character >= dep.name_start_col && position.character <= dep.name_end_col;
            let on_version = position.character >= dep.version_start_col
                && position.character <= dep.version_end_col;

            if dep.line == position.line && (on_name || on_version) {
                if let Some(CheckState::Done(status)) = state.check_states.get(&dep.name) {
                    // Extract changelog and repository_url from any status variant
                    let (
                        current_track,
                        current_version,
                        current_track_release_date,
                        other_tracks,
                        changelog,
                        repository_url,
                        latest_on_track,
                    ) = match status {
                        VersionStatus::UpdateAvailable {
                            current_track,
                            current_version,
                            current_track_release_date,
                            other_tracks,
                            changelog,
                            repository_url,
                            latest_on_track,
                            ..
                        } => (
                            current_track.clone(),
                            current_version.clone(),
                            *current_track_release_date,
                            other_tracks.clone(),
                            changelog.clone(),
                            repository_url.clone(),
                            Some(latest_on_track.clone()),
                        ),
                        VersionStatus::UpToDate {
                            current_track,
                            current_version,
                            current_track_release_date,
                            other_tracks,
                            changelog,
                            repository_url,
                            ..
                        } => (
                            current_track.clone(),
                            current_version.clone(),
                            *current_track_release_date,
                            other_tracks.clone(),
                            changelog.clone(),
                            repository_url.clone(),
                            None,
                        ),
                        VersionStatus::Unknown {
                            current_track,
                            current_version,
                            current_track_release_date,
                            other_tracks,
                            changelog,
                            repository_url,
                            ..
                        } => (
                            current_track.clone(),
                            current_version.clone(),
                            *current_track_release_date,
                            other_tracks.clone(),
                            changelog.clone(),
                            repository_url.clone(),
                            None,
                        ),
                    };

                    // Build hover content based on what's available
                    let mut content_parts = vec![];

                    // Add repository link if available
                    if let Some(ref repo) = repository_url {
                        let clean_url = clean_repo_url(repo);
                        let link_text =
                            if let Some((owner, repo_name)) = extract_github_owner_repo(repo) {
                                format!("View on GitHub: {}/{}", owner, repo_name)
                            } else {
                                "View on GitHub".to_string()
                            };
                        content_parts.push(format!("[{}]({})  ", link_text, clean_url));
                    }

                    // Add NPM link with spacing
                    content_parts.push(format!(
                        "[View on NPM: {}](https://www.npmjs.com/package/{})\n",
                        dep.name, dep.name
                    ));

                    // Divider between links and track info
                    content_parts.push("---".to_string());

                    // Available release tracks header
                    content_parts.push("**Available release tracks:**".to_string());

                    // Build track list - sort by release date (most recent first)
                    let mut all_tracks: Vec<_> = other_tracks.iter().collect();

                    // Add current track info with the LATEST version for that track (not current version)
                    let current_track_version = latest_on_track
                        .clone()
                        .unwrap_or_else(|| current_version.clone());
                    let current_track_info = TrackUpdate {
                        name: current_track.clone(),
                        version: current_track_version,
                        release_date: current_track_release_date,
                        is_newer: false,
                    };
                    all_tracks.push(&current_track_info);

                    // Sort by release date (most recent first)
                    all_tracks.sort_by(|a, b| match (b.release_date, a.release_date) {
                        (Some(b_date), Some(a_date)) => b_date.cmp(&a_date),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    });

                    // Format track lines
                    let mut track_lines = vec![];
                    for track in all_tracks {
                        let date_str = if let Some(date) = track.release_date {
                            format!(" ({})", settings.date_display.format_date_tag(date))
                        } else {
                            String::new()
                        };

                        // Check if this is the current track
                        let current_marker = if track.name == current_track {
                            " `current track`"
                        } else {
                            ""
                        };

                        track_lines.push(format!(
                            "- **{}**: {}{}{}",
                            track.name, track.version, date_str, current_marker
                        ));
                    }

                    if !track_lines.is_empty() {
                        content_parts.push(track_lines.join("\n"));
                    }

                    // Add changelog with header
                    if let Some(ref cl) = changelog {
                        if !cl.is_empty() {
                            content_parts.push(format!("\n---\n\n**Changelog:**\n\n{}", cl));
                        }
                    } else {
                        content_parts.push("\n*Changelog not available.*".to_string());
                    }

                    let content = content_parts.join("\n");

                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: content,
                        }),
                        range: Some(Range {
                            start: Position {
                                line: dep.line,
                                character: dep.name_start_col,
                            },
                            end: Position {
                                line: dep.line,
                                character: dep.version_end_col,
                            },
                        }),
                    }));
                }
            }
        }

        Ok(None)
    }
}

pub async fn start() {
    tracing::debug!("Creating stdin/stdout handles");
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    tracing::debug!("Creating LspService");
    let (service, socket) = LspService::new(Backend::new);

    tracing::debug!("Starting server");
    Server::new(stdin, stdout, socket).serve(service).await;
    tracing::debug!("Server stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{DateDisplaySettings, DateTagMode};
    use chrono::{DateTime, Utc};
    use std::str::FromStr;

    fn utc_date(input: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(input)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn collect_newer_track_summaries_filters_and_formats() {
        let settings = ExtensionSettings {
            date_display: DateDisplaySettings {
                mode: DateTagMode::Date,
                format: "%Y-%m-%d".to_string(),
            },
            ..ExtensionSettings::default()
        };

        let summaries = collect_newer_track_summaries(
            &[
                TrackUpdate {
                    name: "next".to_string(),
                    version: "2.1.0-beta.1".to_string(),
                    release_date: Some(utc_date("2026-02-10T00:00:00Z")),
                    is_newer: true,
                },
                TrackUpdate {
                    name: "latest".to_string(),
                    version: "2.0.0".to_string(),
                    release_date: Some(utc_date("2026-02-11T00:00:00Z")),
                    is_newer: false,
                },
            ],
            &settings,
        );

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0], "BETA → 2.1.0-beta.1");
    }

    #[test]
    fn recent_release_actions_respects_limit() {
        let releases = vec![
            VersionRelease {
                version: "1.0.3".to_string(),
                release_date: utc_date("2026-01-03T00:00:00Z"),
            },
            VersionRelease {
                version: "1.0.2".to_string(),
                release_date: utc_date("2026-01-02T00:00:00Z"),
            },
            VersionRelease {
                version: "1.0.1".to_string(),
                release_date: utc_date("2026-01-01T00:00:00Z"),
            },
        ];

        assert!(recent_release_actions(&releases, 0).is_empty());
        assert_eq!(recent_release_actions(&releases, 2).len(), 2);
    }

    #[test]
    fn progressive_publication_emits_first_then_batches() {
        assert!(should_publish_batch(false, 1, Duration::ZERO, false));
        assert!(!should_publish_batch(true, 1, Duration::ZERO, false));
        assert!(should_publish_batch(
            true,
            RESULT_BATCH_SIZE,
            Duration::ZERO,
            false
        ));
        assert!(should_publish_batch(true, 1, RESULT_BATCH_MAX_WAIT, false));
        assert!(should_publish_batch(true, 1, Duration::ZERO, true));
    }

    #[test]
    fn package_json_detection_requires_exact_file_name() {
        let package_json = Uri::from_str("file:///workspace/package.json").unwrap();
        let lookalike = Uri::from_str("file:///workspace/my-package.json").unwrap();

        assert!(Backend::is_package_json(&package_json));
        assert!(!Backend::is_package_json(&lookalike));
    }
}
