//! Claude app-server binary entry point.

use claude_app_server::outgoing::OutgoingSender;
use claude_app_server::processor::MessageProcessor;
use claude_app_server::sidecar::SidecarClient;
use claude_app_server_transport::StdioTransport;
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const DEFAULT_MODEL: &str = "claude-opus-4-7";

#[derive(Debug, Parser)]
#[command(name = "claude-app-server", version, about = "Claude app server")]
struct Cli {
    /// Listener transport. Only `stdio://` is implemented today.
    #[arg(long, default_value = "stdio://")]
    listen: String,

    /// Override the default model id. Falls back to ANTHROPIC_MODEL env then the compiled default.
    #[arg(long)]
    model: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    install_tracing();
    let cli = Cli::parse();

    if !cli.listen.starts_with("stdio://") {
        anyhow::bail!(
            "Only stdio:// is implemented in this MVP; got {}",
            cli.listen
        );
    }

    let default_model = cli
        .model
        .or_else(|| std::env::var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    // Spawn the TypeScript sidecar before any client traffic arrives so that
    // the first `initialize` reply is already backed by a live agent loop.
    let sidecar = SidecarClient::spawn().await?;

    let transport = StdioTransport::spawn();
    let outgoing = OutgoingSender::new(transport.outgoing.clone());
    let processor = Arc::new(MessageProcessor::new(outgoing, sidecar.clone(), default_model));

    let mut incoming = transport.incoming;
    while let Some(msg) = incoming.recv().await {
        processor.clone().handle(msg).await;
    }

    // Shutdown: tell the sidecar to wind down, then drain stdout.
    if let Err(e) = sidecar.shutdown().await {
        tracing::warn!("sidecar shutdown failed: {e}");
    }
    drop(transport.outgoing);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Ok(())
}

fn install_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("LOG_FORMAT").ok().as_deref() == Some("json");
    if json {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .init();
    }
}
