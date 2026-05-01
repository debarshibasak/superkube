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
#[command(name = "superkube")]
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
        /// Defaults to a local SQLite file (`sqlite://./superkube.db`) if not set.
        #[arg(long, env = "DATABASE_URL", default_value = "sqlite://./superkube.db")]
        db_url: String,

        /// Port to listen on
        #[arg(long, default_value = "6443")]
        port: u16,

        /// Host to bind to
        #[arg(long, default_value = "0.0.0.0")]
        host: String,

        /// Pod IP range (CIDR). The first three octets are taken as the
        /// /24 prefix used by the embedded node agent to allocate pod IPs.
        #[arg(long, default_value = "10.244.0.0/16")]
        pod_cidr: String,

        /// Service ClusterIP range (CIDR). The /24 prefix is used to
        /// auto-assign ClusterIPs to ClusterIP-typed services.
        #[arg(long, default_value = "10.96.0.0/12")]
        service_cidr: String,
    },

    /// Start a node agent
    Node {
        /// Name of this node. Defaults to the host's hostname.
        #[arg(long)]
        name: Option<String>,

        /// URL of the superkube server
        #[arg(long)]
        server: String,

        /// Path to containerd socket (only used by the mock runtime).
        #[arg(long, default_value = "/run/containerd/containerd.sock")]
        containerd_socket: String,

        /// Container runtime backend. One of:
        ///   `auto` (default), `docker`, `embedded`, `mock`.
        #[arg(long, default_value = "auto")]
        runtime: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "superkube=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Server {
            db_url,
            port,
            host,
            pod_cidr,
            service_cidr,
        } => {
            tracing::info!("Starting superkube server on {}:{}", host, port);
            server::run(&db_url, &host, port, &pod_cidr, &service_cidr).await?;
        }
        Commands::Node {
            name,
            server: server_url,
            containerd_socket,
            runtime,
        } => {
            let name = name.unwrap_or_else(util::detect_hostname);
            tracing::info!("Starting superkube node agent: {} (runtime={})", name, runtime);
            node::run_full(
                &name,
                &server_url,
                &containerd_socket,
                std::collections::HashMap::new(),
                "10.244.0.0/16",
                &runtime,
            )
            .await?;
        }
    }

    Ok(())
}
