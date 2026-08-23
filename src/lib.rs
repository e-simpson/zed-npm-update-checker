use std::fs;
use zed_extension_api::{self as zed, Result};

const GITHUB_REPO: &str = "e-simpson/zed-npm-update-checker";
const LANGUAGE_SERVER_NAME: &str = "npm-package-json-checker-lsp";

#[inline]
fn bin_name() -> &'static str {
    if zed::current_platform().0 == zed::Os::Windows {
        "npm-package-json-checker-lsp.exe"
    } else {
        "npm-package-json-checker-lsp"
    }
}

struct NpmUpdatesExtension {
    cached_binary_path: Option<String>,
}

enum Status {
    None,
    Downloading,
    Failed(String),
}

fn update_status(id: &zed::LanguageServerId, status: Status) {
    match status {
        Status::None => zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::None,
        ),
        Status::Downloading => zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::Downloading,
        ),
        Status::Failed(msg) => zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::Failed(msg),
        ),
    }
}

impl NpmUpdatesExtension {
    fn lsp_settings_for_worktree(worktree: &zed::Worktree) -> Option<zed::settings::LspSettings> {
        zed::settings::LspSettings::for_worktree(LANGUAGE_SERVER_NAME, worktree).ok()
    }

    fn language_server_binary_path(
        &mut self,
        id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        let bin_name = bin_name();

        // Dev extensions can place a locally built LSP beside extension.toml.
        // Prefer it over PATH and GitHub releases so local testing is deterministic.
        if fs::metadata(bin_name).is_ok_and(|stat| stat.is_file()) {
            return Ok(bin_name.to_string());
        }

        // Check if the binary is already installed in PATH
        if let Some(path) = worktree.which(bin_name) {
            return Ok(path);
        }

        // Check cached binary path
        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).is_ok_and(|stat| stat.is_file()) {
                update_status(id, Status::None);
                return Ok(path.clone());
            }
        }

        // Check if already downloaded to extension directory
        if let Some(binary_path) = Self::check_installed() {
            // Prefer the newest release, but keep the installed binary as an
            // offline fallback when GitHub cannot be reached.
            return Ok(Self::check_to_update(id).unwrap_or(binary_path));
        }

        // Download from GitHub releases
        let version_binary_path = Self::check_to_update(id)?;
        self.cached_binary_path = Some(version_binary_path.clone());
        Ok(version_binary_path)
    }

    fn check_installed() -> Option<String> {
        let entries = fs::read_dir(".").ok()?;
        for entry in entries.flatten().filter(|entry| entry.path().is_dir()) {
            let binary_path = entry.path().join(bin_name());
            if fs::metadata(&binary_path).is_ok_and(|stat| stat.is_file()) {
                return binary_path.to_str().map(|s| s.to_string());
            }
        }
        None
    }

    fn check_to_update(id: &zed::LanguageServerId) -> Result<String> {
        let (platform, arch) = zed::current_platform();
        let release = zed::latest_github_release(
            GITHUB_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let asset_name = format!(
            "npm-package-json-checker-lsp-{os}-{arch}.{ext}",
            arch = match arch {
                zed::Architecture::Aarch64 => "arm64",
                zed::Architecture::X86 => "amd64",
                zed::Architecture::X8664 => "amd64",
            },
            os = match platform {
                zed::Os::Mac => "darwin",
                zed::Os::Linux => "linux",
                zed::Os::Windows => "windows",
            },
            ext = match platform {
                zed::Os::Windows => "zip",
                _ => "tar.gz",
            }
        );

        let file_type = match platform {
            zed::Os::Windows => zed::DownloadedFileType::Zip,
            _ => zed::DownloadedFileType::GzipTar,
        };

        let version_dir = format!("npm-package-json-checker-lsp-{}", release.version);
        let bin_name = bin_name();
        let version_binary_path = format!("{version_dir}/{bin_name}");

        if !fs::metadata(&version_binary_path).is_ok_and(|stat| stat.is_file()) {
            update_status(id, Status::Downloading);

            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name == asset_name)
                .ok_or_else(|| format!("no asset found matching {:?}", asset_name))?;

            zed::download_file(&asset.download_url, &version_dir, file_type)
                .map_err(|e| format!("failed to download file: {e}"))?;

            // Clean up old versions
            let entries =
                fs::read_dir(".").map_err(|e| format!("failed to list working directory {e}"))?;
            for entry in entries {
                let entry = entry.map_err(|e| format!("failed to load directory entry {e}"))?;
                if entry.file_name().to_str() != Some(&version_dir) {
                    fs::remove_dir_all(entry.path()).ok();
                }
            }

            update_status(id, Status::None);
        }

        Ok(version_binary_path)
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
        id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = self
            .language_server_binary_path(id, worktree)
            .inspect_err(|err| {
                update_status(id, Status::Failed(err.to_string()));
            })?;

        Ok(zed::Command {
            command,
            args: vec![],
            env: Vec::new(),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        _: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        Ok(Self::lsp_settings_for_worktree(worktree)
            .and_then(|settings| settings.initialization_options))
    }

    fn language_server_workspace_configuration(
        &mut self,
        _: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        Ok(Self::lsp_settings_for_worktree(worktree).and_then(|settings| settings.settings))
    }
}

zed::register_extension!(NpmUpdatesExtension);
