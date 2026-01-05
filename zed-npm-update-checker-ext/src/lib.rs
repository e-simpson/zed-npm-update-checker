use std::fs;
use zed_extension_api::{self as zed, LanguageServerId, Result};

struct NpmUpdatesExtension {
    cached_binary_path: Option<String>,
}

impl NpmUpdatesExtension {
    fn language_server_binary_path(&mut self, id: &LanguageServerId) -> Result<String> {
        // Check if we already have a cached path
        if let Some(ref path) = self.cached_binary_path {
            if fs::metadata(path).map(|m| m.is_file()).unwrap_or(false) {
                return Ok(path.clone());
            }
        }

        update_status(id, Status::CheckingForUpdate);

        let (platform, _arch) = zed::current_platform();
        let binary_name = match platform {
            zed::Os::Windows => "npm-update-checker-lsp.exe",
            _ => "npm-update-checker-lsp",
        };

        // Check various locations for the binary
        let search_paths = [
            binary_name.to_string(),
            format!("./{}", binary_name),
            format!("bin/{}", binary_name),
            format!("npm-update-checker-lsp/{}", binary_name),
        ];

        for path in &search_paths {
            if fs::metadata(path).map(|m| m.is_file()).unwrap_or(false) {
                self.cached_binary_path = Some(path.clone());
                update_status(id, Status::None);
                return Ok(path.clone());
            }
        }

        update_status(
            id,
            Status::Failed(
                "npm-update-checker-lsp binary not found. Build with: cargo build --release -p npm-update-checker-lsp"
                    .to_string(),
            ),
        );

        Err("npm-update-checker-lsp binary not found. Build with: cargo build --release -p npm-update-checker-lsp".into())
    }
}

impl zed::Extension for NpmUpdatesExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        _worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary_path = self
            .language_server_binary_path(language_server_id)
            .inspect_err(|err| {
                update_status(language_server_id, Status::Failed(err.to_string()));
            })?;

        Ok(zed::Command {
            command: binary_path,
            args: vec![],
            env: Default::default(),
        })
    }
}

enum Status {
    CheckingForUpdate,
    Failed(String),
    None,
}

fn update_status(id: &LanguageServerId, status: Status) {
    let status = match status {
        Status::CheckingForUpdate => zed::LanguageServerInstallationStatus::CheckingForUpdate,
        Status::Failed(msg) => zed::LanguageServerInstallationStatus::Failed(msg),
        Status::None => zed::LanguageServerInstallationStatus::None,
    };
    zed::set_language_server_installation_status(id, &status);
}

zed::register_extension!(NpmUpdatesExtension);
