//! Claude app-server binary entry point.

use claude_app_server::outgoing::OutgoingSender;
use claude_app_server::processor::MessageProcessor;
use claude_app_server::rollouts::{default_dir, RolloutStore};
use claude_app_server::sidecar::SidecarClient;
use claude_app_server::thread_store::ThreadStore;
use claude_app_server::ws_transport;
use claude_app_server_transport::StdioTransport;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const DEFAULT_MODEL: &str = "claude-opus-4-7";

#[derive(Debug, Parser)]
#[command(name = "claude-app-server", version, about = "Claude app server")]
struct Cli {
    /// Listener transport. `stdio://` (default), `ws://HOST:PORT`, or
    /// `off` (don't listen — useful with generate-* subcommands).
    #[arg(long, default_value = "stdio://")]
    listen: String,

    /// Override the default model id. Falls back to ANTHROPIC_MODEL env then the compiled default.
    #[arg(long)]
    model: Option<String>,

    #[command(subcommand)]
    command: Option<Subcmd>,
}

#[derive(Debug, Subcommand)]
enum Subcmd {
    /// Emit TypeScript type definitions for the JSON-RPC protocol.
    GenerateTs {
        /// Output directory. Will be created if missing.
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Emit JSON Schema definitions for the JSON-RPC protocol.
    GenerateJsonSchema {
        /// Output directory. Will be created if missing.
        #[arg(long)]
        out: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    install_tracing();
    let cli = Cli::parse();

    if let Some(sub) = cli.command {
        return handle_subcommand(sub).await;
    }

    let default_model = cli
        .model
        .or_else(|| std::env::var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    // Spawn the TypeScript sidecar before any client traffic arrives so that
    // the first `initialize` reply is already backed by a live agent loop.
    let sidecar = SidecarClient::spawn().await?;

    // Open the rollout store and replay any persisted threads from prior
    // runs so `thread/list` / `thread/resume` work after a restart.
    let rollouts = RolloutStore::open(default_dir())?;
    let store = ThreadStore::with_rollouts(rollouts);
    store.replay_rollouts().await;

    let listen = cli.listen.as_str();
    if listen == "off" {
        // Useful when the binary is being driven purely as a subcommand
        // launcher (generate-*). We still spawned the sidecar to validate
        // it, but we exit immediately.
        return Ok(());
    } else if listen.starts_with("ws://") {
        let addr = listen.trim_start_matches("ws://").parse()?;
        ws_transport::serve(addr, sidecar, store, default_model).await?;
        return Ok(());
    } else if !listen.starts_with("stdio://") {
        anyhow::bail!("unsupported --listen value: {listen}");
    }

    let transport = StdioTransport::spawn();
    let outgoing = OutgoingSender::new(transport.outgoing.clone());
    let processor = Arc::new(MessageProcessor::with_store(
        outgoing,
        sidecar.clone(),
        default_model,
        store,
    ));

    let mut incoming = transport.incoming;
    while let Some(msg) = incoming.recv().await {
        processor.clone().handle(msg).await;
    }

    if let Err(e) = sidecar.shutdown().await {
        tracing::warn!("sidecar shutdown failed: {e}");
    }
    drop(transport.outgoing);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Ok(())
}

async fn handle_subcommand(sub: Subcmd) -> anyhow::Result<()> {
    match sub {
        Subcmd::GenerateTs { out } => {
            claude_app_server::schema_export::write_typescript(&out)?;
            println!("wrote TypeScript schema to {}", out.display());
        }
        Subcmd::GenerateJsonSchema { out } => {
            claude_app_server::schema_export::write_json_schema(&out)?;
            println!("wrote JSON schema to {}", out.display());
        }
    }
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
