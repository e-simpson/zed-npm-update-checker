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

const LSP_NAME: &str = "npm-update-checker-lsp";

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

        // Spawn individual tasks for each package to update diagnostics incrementally
        let mut handles = Vec::new();
        
        for dep in &dependencies {
            let registry = self.registry.clone();
            let client = self.client.clone();
            let documents = self.documents.clone();
            let uri = uri.clone();
            let dep_name = dep.name.clone();
            let dep_clean_version = dep.clean_version.clone();
            
            let handle = tokio::spawn(async move {
                // Fetch package info
                let info = registry.get_package_info(&dep_name, &dep_clean_version).await;
                
                // Determine status
                let status = if let Some(pkg_info) = info {
                    if !dep_clean_version.is_empty() {
                        check_version_status(
                            &dep_clean_version,
                            &pkg_info.latest_version,
                            pkg_info.changelog,
                            pkg_info.repository_url,
                        )
                    } else {
                        VersionStatus::Unknown
                    }
                } else {
                    VersionStatus::Unknown
                };
                
                // Update state for this package
                {
                    let mut docs = documents.write().await;
                    if let Some(state) = docs.get_mut(&uri) {
                        state.check_states.insert(dep_name.clone(), CheckState::Done(status));
                    }
                }
                
                // Refresh inlay hints (removes the ⏳ for this package)
                let _ = client.send_request::<lsp_types::request::InlayHintRefreshRequest>(()).await;
                
                (uri, dep_name)
            });
            
            handles.push(handle);
        }
        
        // Wait for all tasks and publish diagnostics incrementally
        for handle in handles {
            if let Ok((uri, _dep_name)) = handle.await {
                // Publish updated diagnostics after each package completes
                self.publish_diagnostics(&uri).await;
            }
        }
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
                    // Severity levels: Patch → INFO, Minor → WARNING, Major → ERROR
                    // let severity_level = match severity {
                    //     UpdateSeverity::Major => DiagnosticSeverity::ERROR,
                    //     UpdateSeverity::Minor => DiagnosticSeverity::WARNING,
                    //     UpdateSeverity::Patch => DiagnosticSeverity::INFORMATION,
                    // };

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

        // Find if we're hovering over a dependency line
        for dep in &state.dependencies {
            if dep.line == position.line && 
               position.character >= dep.version_start_col && 
               position.character <= dep.version_end_col {
                if let Some(CheckState::Done(VersionStatus::UpdateAvailable { latest, severity, changelog, repository_url })) = state.check_states.get(&dep.name) {
                    // Build hover content based on what's available
                    let content = match (changelog, repository_url) {
                        (Some(cl), Some(repo)) => {
                            let clean_url = clean_repo_url(repo);
                            format!("{}\n\n---\n\n[View on GitHub]({})", cl, clean_url)
                        }
                        (Some(cl), None) => {
                            cl.clone()
                        }
                        (None, Some(repo)) => {
                            let clean_url = clean_repo_url(repo);
                            format!(
                                "*Changelog not available.*\n\n[View on GitHub]({})",
                                clean_url
                            )
                        }
                        (None, None) => {
                            format!(
                                "*Changelog not available.*"                                
                            )
                        }
                    };

                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: content,
                        }),
                        range: Some(Range {
                            start: Position { line: dep.line, character: dep.version_start_col },
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
