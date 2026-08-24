//! attic — MCP server entry point (skeleton).
//!
//! Stdout is reserved exclusively for the MCP stdio protocol.
//! All tracing output is directed to **stderr**.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

fn main() {
    // Route all tracing to stderr so stdout remains clean for MCP stdio.
    fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    info!("attic MCP server starting (Bootstrap skeleton — no behaviour yet)");

    // TODO(Phase 1D): initialise rmcp stdio transport and serve tools.
}
