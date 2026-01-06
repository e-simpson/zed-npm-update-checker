mod lsp;
mod parser;
mod registry;

#[tokio::main]
async fn main() {
    // Initialize tracing FIRST for debugging (write to stderr, never stdout)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::DEBUG.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    // Handle version and help flags - write to STDERR for LSP compatibility
    let args: Vec<String> = std::env::args().collect();
    for arg in args.iter().map(|s| s.to_lowercase()) {
        if arg == "-v" || arg == "--version" {
            eprintln!("npm-package-json-checker-lsp v{}", env!("CARGO_PKG_VERSION"));
            return;
        } else if arg == "-h" || arg == "--help" {
            eprintln!("npm-package-json-checker-lsp - Language server for npm package updates");
            eprintln!();
            eprintln!("Usage: npm-package-json-checker-lsp [options]");
            eprintln!();
            eprintln!("Options:");
            eprintln!("  -v, --version    Print version information");
            eprintln!("  -h, --help       Print this help message");
            return;
        }
    }

    tracing::info!("Starting npm-package-json-checker-lsp v{}", env!("CARGO_PKG_VERSION"));
    
    lsp::start().await;
    
    tracing::info!("LSP server finished");
}

