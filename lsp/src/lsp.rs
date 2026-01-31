use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{self, *};
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{debug, info};

use crate::parser::{parse_package_json, Dependency};
use crate::registry::{
    check_version_status, format_date_with_ago, NpmRegistry, TrackUpdate,
    UpdateSeverity, VersionStatus
};

const LSP_NAME: &str = "npm-package-json-checker-lsp";

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
}

struct Backend {
    client: Client,
    registry: Arc<NpmRegistry>,
    documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            registry: Arc::new(NpmRegistry::new()),
            documents: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if the file is a package.json
    fn is_package_json(uri: &Url) -> bool {
        uri.path().ends_with("package.json")
    }

    /// Process a document - first show loading, then fetch versions incrementally
    async fn process_document(&self, uri: &Url, text: &str) {
        if !Self::is_package_json(uri) {
            return;
        }

        debug!("Processing {}", uri);

        // Parse dependencies
        let dependencies = parse_package_json(text);

        if dependencies.is_empty() {
            let mut docs = self.documents.write().await;
            docs.insert(uri.clone(), DocumentState {
                dependencies: vec![],
                check_states: HashMap::new(),
            });
            return;
        }

        // Set all packages to "Checking" state immediately
        let mut check_states = HashMap::new();
        for dep in &dependencies {
            check_states.insert(dep.name.clone(), CheckState::Checking);
        }

        // Store initial state with loading indicators
        {
            let mut docs = self.documents.write().await;
            docs.insert(uri.clone(), DocumentState {
                dependencies: dependencies.clone(),
                check_states,
            });
        }

        // Trigger refresh to show loading state
        let _ = self.client.send_request::<lsp_types::request::InlayHintRefreshRequest>(()).await;

        // Fetch version info AND changelogs together in parallel
        let mut handles = Vec::new();

        for dep in &dependencies {
            let registry = self.registry.clone();
            let dep_name = dep.name.clone();
            let dep_clean_version = dep.clean_version.clone();

            let handle = tokio::spawn(async move {
                // Step 1: Get version info from npm registry (includes all tracks)
                let version_info = registry.get_package_version_info(&dep_name, &dep_clean_version).await;

                // Step 2: Check version status and fetch changelog if repo URL exists
                let final_status = if let Some(ref pkg_info) = version_info {
                    if !dep_clean_version.is_empty() {
                        let temp_status = check_version_status(
                            &dep_clean_version,
                            &pkg_info.latest_on_track,
                            &pkg_info.current_track,
                            &pkg_info.all_tracks,
                            None, // Placeholder - we'll fill in changelog after
                            pkg_info.repository_url.clone(),
                        );

                        // Fetch changelog if there's a repo URL
                        let changelog = if let Some(ref repo_url) = pkg_info.repository_url {
                            let latest_for_changelog = match &temp_status {
                                VersionStatus::UpdateAvailable { latest_on_track, .. } => latest_on_track.clone(),
                                _ => dep_clean_version.clone(),
                            };

                            registry.fetch_changelog_for_package(
                                &dep_name,
                                &dep_clean_version,
                                &latest_for_changelog,
                                repo_url,
                                pkg_info.repository_directory.as_deref(),
                            ).await
                        } else {
                            None
                        };

                        // Build final status with changelog
                        match temp_status {
                            VersionStatus::UpdateAvailable {
                                current_track,
                                current_version,
                                latest_on_track,
                                other_tracks,
                                severity,
                                repository_url,
                                ..
                            } => {
                                VersionStatus::UpdateAvailable {
                                    current_track,
                                    current_version,
                                    latest_on_track,
                                    other_tracks,
                                    severity,
                                    changelog,
                                    repository_url,
                                }
                            }
                            VersionStatus::UpToDate { current_track, current_version, other_tracks, .. } => {
                                VersionStatus::UpToDate {
                                    current_track,
                                    current_version,
                                    other_tracks,
                                    changelog,
                                    repository_url: pkg_info.repository_url.clone(),
                                }
                            }
                            VersionStatus::Unknown { current_track, current_version, other_tracks, .. } => {
                                VersionStatus::Unknown {
                                    current_track,
                                    current_version,
                                    other_tracks,
                                    changelog,
                                    repository_url: pkg_info.repository_url.clone(),
                                }
                            }
                            other => other,
                        }
                    } else {
                        VersionStatus::Unknown {
                            current_track: "latest".to_string(),
                            current_version: dep_clean_version.clone(),
                            other_tracks: vec![],
                            changelog: None,
                            repository_url: pkg_info.repository_url.clone(),
                        }
                    }
                } else {
                    VersionStatus::Unknown {
                        current_track: "latest".to_string(),
                        current_version: dep_clean_version.clone(),
                        other_tracks: vec![],
                        changelog: None,
                        repository_url: None,
                    }
                };

                (dep_name, final_status)
            });

            handles.push(handle);
        }

        // Wait for ALL fetches to complete, then update state
        for handle in handles {
            if let Ok((dep_name, status)) = handle.await {
                let mut docs = self.documents.write().await;
                if let Some(state) = docs.get_mut(&uri) {
                    state.check_states.insert(dep_name, CheckState::Done(status));
                }
            }
        }

        // Publish diagnostics and refresh UI once everything is ready
        let _ = self.client.send_request::<lsp_types::request::InlayHintRefreshRequest>(()).await;
        self.publish_diagnostics(&uri).await;
    }

    /// Publish diagnostics for outdated packages
    async fn publish_diagnostics(&self, uri: &Url) {
        let docs = self.documents.read().await;
        let Some(state) = docs.get(uri) else {
            return;
        };

        let mut diagnostics = Vec::new();

        for dep in &state.dependencies {
            if let Some(CheckState::Done(status)) = state.check_states.get(&dep.name) {
                if let VersionStatus::UpdateAvailable {
                    latest_on_track,
                    severity,
                    current_track,
                    other_tracks,
                    ..
                } = status {
                    // Build message with track info in new format: "track PATCH/MINOR/MAJOR: New version code (X time ago)"
                    let time_ago = other_tracks.iter()
                        .find(|t| t.name == *current_track)
                        .and_then(|t| t.release_date)
                        .map(|date| format!(" ({})", format_date_with_ago(date)))
                        .unwrap_or_default();

                    let message = format!(
                        "{} {} available: {}{}",
                        current_track,
                        severity.label(),
                        latest_on_track,
                        time_ago
                    );

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
                        severity: Some(DiagnosticSeverity::INFORMATION),
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
                            "severity": severity.label(),
                            "track": current_track,
                        })),
                    });
                }
            }
        }

        drop(docs);

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }

    /// Generate inlay hints for a document
    async fn generate_inlay_hints(&self, uri: &Url) -> Vec<InlayHint> {
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
    async fn generate_code_actions(&self,
        uri: &Url,
        range: Range
    ) -> Vec<CodeActionOrCommand> {
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

            if let Some(CheckState::Done(ref status)) = state.check_states.get(&dep.name) {
                match status {
                    VersionStatus::UpdateAvailable {
                        latest_on_track,
                        severity,
                        current_track,
                        other_tracks,
                        ..
                    } => {
                        // 1. Update on current track (preferred)
                        let track_label = if current_track == "latest" {
                            "latest".to_string()
                        } else {
                            format!("{} track", current_track)
                        };

                        let update_action = create_update_action(
                            uri,
                            dep,
                            latest_on_track,
                            &format!("{}: Update {} to {} ({})",
                                severity.label(), dep.name, latest_on_track, track_label),
                            true, // is_preferred
                        );
                        actions.push(CodeActionOrCommand::CodeAction(update_action));

                        // 2. Alternative track switches
                        // Sort: current track first, then by release date/version
                        let mut sorted_tracks: Vec<&TrackUpdate> = other_tracks.iter().collect();

                        // Put stable track first if not on it
                        if current_track != "latest" {
                            if let Some(latest_idx) = sorted_tracks.iter().position(|t| t.name == "latest") {
                                let latest_track = sorted_tracks.remove(latest_idx);
                                sorted_tracks.insert(0, latest_track);
                            }
                        }

                        for track in sorted_tracks {
                            let date_str = if let Some(date) = track.release_date {
                                format!(" ({})", format_date_with_ago(date))
                            } else {
                                String::new()
                            };

                            let switch_action = create_update_action(
                                uri,
                                dep,
                                &track.version,
                                &format!("Switch to {}: {}{}",
                                    track.name, track.version, date_str),
                                false, // not preferred
                            );
                            actions.push(CodeActionOrCommand::CodeAction(switch_action));
                        }
                    }
                    VersionStatus::UpToDate { current_track, other_tracks, .. } => {
                        // Still show options to switch tracks even if up to date
                        let mut sorted_tracks: Vec<&TrackUpdate> = other_tracks.iter().collect();

                        // Put stable track first if not on it
                        if current_track != "latest" {
                            if let Some(latest_idx) = sorted_tracks.iter().position(|t| t.name == "latest") {
                                let latest_track = sorted_tracks.remove(latest_idx);
                                sorted_tracks.insert(0, latest_track);
                            }
                        }

                        for track in sorted_tracks {
                            let date_str = if let Some(date) = track.release_date {
                                format!(" ({})", format_date_with_ago(date))
                            } else {
                                String::new()
                            };

                            let switch_action = create_update_action(
                                uri,
                                dep,
                                &track.version,
                                &format!("Switch to {}: {}{}",
                                    track.name, track.version, date_str),
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
        let outdated_deps: Vec<_> = state.dependencies.iter().filter(|d| {
            if d.line < range.start.line || d.line > range.end.line {
                return false;
            }
            matches!(
                state.check_states.get(&d.name),
                Some(CheckState::Done(VersionStatus::UpdateAvailable { .. }))
            )
        }).collect();

        if outdated_deps.len() > 1 {
            let mut all_edits = Vec::new();

            for dep in &outdated_deps {
                if let Some(CheckState::Done(VersionStatus::UpdateAvailable { latest_on_track, .. })) =
                    state.check_states.get(&dep.name)
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
    uri: &Url,
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
    if let Some(rest) = url.strip_prefix("github.com/").or_else(|| url.strip_prefix("github.com:")) {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
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
        debug!("did_open: {}", params.text_document.uri);
        self.process_document(&params.text_document.uri, &params.text_document.text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        debug!("did_change: {}", params.text_document.uri);
        if let Some(change) = params.content_changes.into_iter().next() {
            self.process_document(&params.text_document.uri, &change.text).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        debug!("did_save: {}", params.text_document.uri);
        if let Some(text) = params.text {
            self.process_document(&params.text_document.uri, &text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        debug!("did_close: {}", params.text_document.uri);
        let mut docs = self.documents.write().await;
        docs.remove(&params.text_document.uri);
    }

    async fn inlay_hint(&self,
        params: InlayHintParams
    ) -> Result<Option<Vec<InlayHint>>> {
        debug!("inlay_hint request for: {}", params.text_document.uri);
        let hints = self.generate_inlay_hints(&params.text_document.uri).await;
        debug!("inlay_hint returning {} hints", hints.len());
        for hint in &hints {
            debug!("  hint at {:?}: {:?}", hint.position, hint.label);
        }
        Ok(Some(hints))
    }

    async fn code_action(&self,
        params: CodeActionParams
    ) -> Result<Option<CodeActionResponse>> {
        debug!("code_action: {}", params.text_document.uri);
        let actions = self
            .generate_code_actions(&params.text_document.uri, params.range)
            .await;
        Ok(Some(actions))
    }

    async fn hover(&self,
        params: HoverParams
    ) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let docs = self.documents.read().await;
        let Some(state) = docs.get(uri) else {
            return Ok(None);
        };

        // Find if we're hovering over a dependency line (package name or version)
        for dep in &state.dependencies {
            // Check if cursor is on the package name OR version string
            let on_name = position.character >= dep.name_start_col &&
                          position.character <= dep.name_end_col;
            let on_version = position.character >= dep.version_start_col &&
                             position.character <= dep.version_end_col;

            if dep.line == position.line && (on_name || on_version) {
                if let Some(CheckState::Done(status)) = state.check_states.get(&dep.name) {
                    // Extract changelog and repository_url from any status variant
                    let (current_track, current_version, other_tracks, changelog, repository_url, latest_on_track) = match status {
                        VersionStatus::UpdateAvailable {
                            current_track,
                            current_version,
                            other_tracks,
                            changelog,
                            repository_url,
                            latest_on_track,
                            ..
                        } => {
                            (current_track.clone(), current_version.clone(), other_tracks.clone(),
                             changelog.clone(), repository_url.clone(), Some(latest_on_track.clone()))
                        }
                        VersionStatus::UpToDate {
                            current_track,
                            current_version,
                            other_tracks,
                            changelog,
                            repository_url
                        } => {
                            (current_track.clone(), current_version.clone(), other_tracks.clone(),
                             changelog.clone(), repository_url.clone(), None)
                        }
                        VersionStatus::Unknown {
                            current_track,
                            current_version,
                            other_tracks,
                            changelog,
                            repository_url
                        } => {
                            (current_track.clone(), current_version.clone(), other_tracks.clone(),
                             changelog.clone(), repository_url.clone(), None)
                        }
                        _ => return Ok(None),
                    };

                    // Build hover content based on what's available
                    let mut content_parts = vec![];

                    // Add repository link if available
                    if let Some(ref repo) = repository_url {
                        let clean_url = clean_repo_url(repo);
                        let link_text = if let Some((owner, repo_name)) = extract_github_owner_repo(repo) {
                            format!("View on GitHub: {}/{}", owner, repo_name)
                        } else {
                            "View on GitHub".to_string()
                        };
                        content_parts.push(format!("[{}]({})  ", link_text, clean_url));
                    }

                    // Add NPM link with spacing
                    content_parts.push(format!("[View on NPM: {}](https://www.npmjs.com/package/{})\n", dep.name, dep.name));

                    // Divider between links and track info
                    content_parts.push("---".to_string());

                    // Available release tracks header
                    content_parts.push("**Available release tracks:**".to_string());

                    // Build track list - sort by release date (most recent first)
                    let mut all_tracks: Vec<_> = other_tracks.iter().collect();

                    // Add current track info with the LATEST version for that track (not current version)
                    let current_track_version = latest_on_track.clone().unwrap_or_else(|| current_version.clone());
                    let current_track_info = TrackUpdate {
                        name: current_track.clone(),
                        version: current_track_version,
                        release_date: None, // Will be looked up from other_tracks if available
                        is_newer: false,
                    };
                    all_tracks.push(&current_track_info);

                    // Sort by release date (most recent first)
                    all_tracks.sort_by(|a, b| {
                        match (b.release_date, a.release_date) {
                            (Some(b_date), Some(a_date)) => b_date.cmp(&a_date),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => std::cmp::Ordering::Equal,
                        }
                    });

                    // Format track lines
                    let mut track_lines = vec![];
                    for track in all_tracks {
                        let date_str = if let Some(date) = track.release_date {
                            format!(" ({})", format_date_with_ago(date))
                        } else {
                            String::new()
                        };

                        // Check if this is the current track
                        let current_marker = if track.name == current_track {
                            " `current track`"
                        } else {
                            ""
                        };

                        track_lines.push(format!("- **{}**: {}{}{}", track.name, track.version, date_str, current_marker));
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
                            start: Position { line: dep.line, character: dep.name_start_col },
                            end: Position { line: dep.line, character: dep.version_end_col },
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
