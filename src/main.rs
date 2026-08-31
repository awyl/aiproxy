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
    let config =
        aiproxy::config::Config::load(&cli.config).map_err(|e| format!("config error: {e}"))?;
    let (host, port) = config
        .bind_host_port()
        .map_err(|e| format!("config error: {e}"))?;
    let port = cli.port.unwrap_or(port);
    tracing::info!(host = %host, port, "starting aiproxy");
    aiproxy::server::run_with_port(config, cli.port)
        .await
        .map_err(Into::into)
}
