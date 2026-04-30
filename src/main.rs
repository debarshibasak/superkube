use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod db;
mod error;
mod models;
mod node;
mod server;
mod util;

pub use error::{Error, Result};

#[derive(Parser)]
#[command(name = "kais")]
#[command(about = "A minimal Kubernetes-like platform in Rust", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the control plane server (runs migrations automatically)
    Server {
        /// Database URL (postgres://... or sqlite://path/to/file.db).
        /// Defaults to a local SQLite file (`sqlite://./kais.db`) if not set.
        #[arg(long, env = "DATABASE_URL", default_value = "sqlite://./kais.db")]
        db_url: String,

        /// Port to listen on
        #[arg(long, default_value = "6443")]
        port: u16,

        /// Host to bind to
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
    },

    /// Start a node agent
    Node {
        /// Name of this node. Defaults to the host's hostname.
        #[arg(long)]
        name: Option<String>,

        /// URL of the kais server
        #[arg(long)]
        server: String,

        /// Path to containerd socket
        #[arg(long, default_value = "/run/containerd/containerd.sock")]
        containerd_socket: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kais=info,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Server { db_url, port, host } => {
            tracing::info!("Starting kais server on {}:{}", host, port);
            server::run(&db_url, &host, port).await?;
        }
        Commands::Node {
            name,
            server: server_url,
            containerd_socket,
        } => {
            let name = name.unwrap_or_else(util::detect_hostname);
            tracing::info!("Starting kais node agent: {}", name);
            node::run(&name, &server_url, &containerd_socket, std::collections::HashMap::new()).await?;
        }
    }

    Ok(())
}
