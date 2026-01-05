# NPM Update Checker for Zed

A Zed extension that shows out-dated NPM packages in `package.json` files using a rust LSP server.

## Features

- Shows if a package is up to date
- Offers auto-complete to update a package to the latest version
- Distinguishes between major, minor, and patch updates
- Shows changelog if available

## Manual Installation

1. Clone this repository
2. Build the LSP:
   ```bash
   cargo build --release -p npm-update-checker-lsp
   cp target/release/npm-update-checker-lsp .
   ```
3. In Zed: Command Palette → "zed: install dev extension" → select this directory

## Project Structure

```
├── extension.toml       # Zed extension manifest
├── src/lib.rs           # WASM extension (registers LSP)
└── lsp/                 # LSP binary
    └── src/
        ├── main.rs      # Entry point
        ├── lsp.rs       # tower-lsp server
        ├── parser.rs    # package.json parsing
        └── registry.rs  # npm registry + GitHub client
```

## Defaults

- Cache TTL: 5 minutes
- Concurrent requests: 10
- Request timeout: 10 seconds

## License

MIT
