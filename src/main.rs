use clap::Parser;
use std::path::PathBuf;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(name = "aiproxy", about = "single-entry LLM proxy with MCP hosting")]
struct Cli {
    /// Path to config file (default: ./aiproxy.yaml)
    #[arg(short, long, default_value = "aiproxy.yaml")]
    config: PathBuf,
    /// Override the port from the config file
    #[arg(short, long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
    let cli = Cli::parse();
    let mut config =
        aiproxy::config::Config::load(&cli.config).map_err(|e| format!("config error: {e}"))?;
    if let Some(port) = cli.port {
        config.port = port;
    }
    tracing::info!(port = config.port, "starting aiproxy");
    aiproxy::server::run(config).await.map_err(Into::into)
}