use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{self, *};
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{debug, info};

use crate::parser::{parse_package_json, Dependency};
use crate::registry::{check_version_status, NpmRegistry, VersionStatus};

// Re-export for type inference in pattern matching
#[allow(unused_imports)]
use crate::registry::UpdateSeverity;

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

        // ═══════════════════════════════════════════════════════════════════════════
        // Fetch version info AND changelogs together in parallel
        // Since we use atom feeds (no rate limits), we can do everything at once
        // ═══════════════════════════════════════════════════════════════════════════
        
        let mut handles = Vec::new();
        
        for dep in &dependencies {
            let registry = self.registry.clone();
            let dep_name = dep.name.clone();
            let dep_clean_version = dep.clean_version.clone();
            
            let handle = tokio::spawn(async move {
                // Step 1: Get version info from npm registry
                let version_info = registry.get_package_version_info(&dep_name).await;
                
                // Step 2: Check version status and fetch changelog if repo URL exists
                let final_status = if let Some(ref pkg_info) = version_info {
                    if !dep_clean_version.is_empty() {
                        let temp_status = check_version_status(
                            &dep_clean_version,
                            &pkg_info.latest_version,
                            None, // Placeholder - we'll fill in changelog after
                            pkg_info.repository_url.clone(),
                        );
                        
                        // Fetch changelog if there's a repo URL (regardless of update status)
                        let changelog = if let Some(ref repo_url) = pkg_info.repository_url {
                            // For UpToDate, use current version as both current and latest
                            // For UpdateAvailable, use the latest version
                            let latest_for_changelog = match &temp_status {
                                VersionStatus::UpdateAvailable { latest, .. } => latest.clone(),
                                _ => dep_clean_version.clone(),
                            };
                            
                            registry.fetch_changelog_for_package(
                                &dep_name,
                                &dep_clean_version,
                                &latest_for_changelog,
                                repo_url,
                            ).await
                        } else {
                            None
                        };
                        
                        // Build final status with changelog
                        match temp_status {
                            VersionStatus::UpdateAvailable { latest, severity, repository_url, .. } => {
                                VersionStatus::UpdateAvailable {
                                    latest,
                                    severity,
                                    changelog,
                                    repository_url,
                                }
                            }
                            VersionStatus::UpToDate { .. } => {
                                VersionStatus::UpToDate {
                                    changelog,
                                    repository_url: pkg_info.repository_url.clone(),
                                }
                            }
                            VersionStatus::Unknown { .. } => {
                                VersionStatus::Unknown {
                                    changelog,
                                    repository_url: pkg_info.repository_url.clone(),
                                }
                            }
                            other => other,
                        }
                    } else {
                        VersionStatus::Unknown {
                            changelog: None,
                            repository_url: pkg_info.repository_url.clone(),
                        }
                    }
                } else {
                    VersionStatus::Unknown {
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
                if let VersionStatus::UpdateAvailable { latest, severity, .. } = status {
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
                        message: format!(
                            "{} update available: {} → {}",
                            severity.label(),
                            dep.version, latest
                        ),
                        related_information: None,
                        tags: None,
                        code_description: None,
                        data: Some(serde_json::json!({
                            "package": dep.name,
                            "current": dep.version,
                            "latest": latest,
                            "severity": severity.label(),
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
    async fn generate_code_actions(&self, uri: &Url, range: Range) -> Vec<CodeActionOrCommand> {
        let docs = self.documents.read().await;
        let Some(state) = docs.get(uri) else {
            return vec![];
        };

        let mut actions = Vec::new();
        let mut outdated_deps = Vec::new();

        for dep in &state.dependencies {
            // Check if this dependency is in the requested range
            if dep.line < range.start.line || dep.line > range.end.line {
                continue;
            }

            if let Some(CheckState::Done(VersionStatus::UpdateAvailable { latest, severity, .. })) = state.check_states.get(&dep.name) {
                outdated_deps.push((dep.clone(), latest.clone(), *severity));

                // Create individual update action
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
                    new_text: format_updated_version(&dep.version, latest),
                };

                let mut changes = HashMap::new();
                changes.insert(uri.clone(), vec![edit]);

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("{}: Update {} to {}", severity.label(), dep.name, latest),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    command: None,
                    is_preferred: Some(true),
                    disabled: None,
                    data: None,
                }));
            }
        }

        // Add "Update all" action if there are multiple outdated deps
        if outdated_deps.len() > 1 {
            let mut all_edits = Vec::new();
            
            for (dep, latest, _) in &outdated_deps {
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
                    new_text: format_updated_version(&dep.version, latest),
                });
            }

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

        actions
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

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        debug!("inlay_hint request for: {}", params.text_document.uri);
        let hints = self.generate_inlay_hints(&params.text_document.uri).await;
        debug!("inlay_hint returning {} hints", hints.len());
        for hint in &hints {
            debug!("  hint at {:?}: {:?}", hint.position, hint.label);
        }
        Ok(Some(hints))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        debug!("code_action: {}", params.text_document.uri);
        let actions = self
            .generate_code_actions(&params.text_document.uri, params.range)
            .await;
        Ok(Some(actions))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
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
                    let (changelog, repository_url) = match status {
                        VersionStatus::UpdateAvailable { changelog, repository_url, .. } => {
                            (changelog.clone(), repository_url.clone())
                        }
                        VersionStatus::UpToDate { changelog, repository_url } => {
                            (changelog.clone(), repository_url.clone())
                        }
                        VersionStatus::Unknown { changelog, repository_url } => {
                            (changelog.clone(), repository_url.clone())
                        }
                        _ => (None, None),
                    };
                    
                    // Build hover content based on what's available
                    let content = match (changelog, repository_url) {
                        (Some(cl), Some(repo)) => {
                            let clean_url = clean_repo_url(&repo);
                            let link_text = if let Some((owner, repo_name)) = extract_github_owner_repo(&repo) {
                                format!("View on GitHub: {}/{}", owner, repo_name)
                            } else {
                                "View on GitHub".to_string()
                            };
                            format!("[{}]({})\n\n---\n\n{}", link_text, clean_url, cl)
                        }
                        (Some(cl), None) => {
                            cl.clone()
                        }
                        (None, Some(repo)) => {
                            let clean_url = clean_repo_url(&repo);
                            let link_text = if let Some((owner, repo_name)) = extract_github_owner_repo(&repo) {
                                format!("View on GitHub: {}/{}", owner, repo_name)
                            } else {
                                "View on GitHub".to_string()
                            };
                            format!(
                                "*Changelog not available.*\n\n[{}]({})",
                                link_text, clean_url
                            )
                        }
                        (None, None) => {
                            "*Changelog not available.*".to_string()
                        }
                    };

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
