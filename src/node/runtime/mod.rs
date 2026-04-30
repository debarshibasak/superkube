//! Container runtime abstraction.
//!
//! Multiple backends sit behind one trait:
//!   - `mock`   : in-memory stub, works everywhere — useful for dev and tests
//!   - `docker` : talks to Docker Desktop / dockerd via /var/run/docker.sock
//!                (compiled on macOS; this is how we actually run containers
//!                on a Mac, since macOS has no Linux kernel features)
//!   - (future) `embedded`: libcontainer + image pull, Linux-only single binary
//!
//! `default()` picks the right backend for the host: Docker on macOS,
//! mock everywhere else (until the libcontainer wrapper lands).

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::Stream;
use tokio::io::AsyncWrite;

use crate::models::Container;

mod mock;
#[cfg(target_os = "macos")]
mod docker;
#[cfg(target_os = "linux")]
mod embedded;

/// Stream of raw log bytes from a container. Each item is one chunk; we don't
/// guarantee chunks align to lines.
pub type LogStream = Pin<Box<dyn Stream<Item = anyhow::Result<Bytes>> + Send>>;

/// Options for fetching container logs.
#[derive(Default, Debug, Clone)]
pub struct LogOptions {
    pub tail_lines: Option<i64>,
    pub timestamps: bool,
    pub since_time: Option<DateTime<Utc>>,
    pub limit_bytes: Option<i64>,
}

/// One published port: container's internal port → host port.
#[derive(Debug, Clone)]
pub struct PortMapping {
    pub container_port: i32,
    pub host_port: u16,
    /// "tcp" or "udp" (lowercase).
    pub protocol: String,
}

/// Snapshot of a container's state, as observed by the runtime. Returned by
/// `find_container` so the agent can build `containerStatuses` and decide
/// whether the container needs to be (re)started.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub running: bool,
    pub restart_count: i32,
    pub started_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    /// Ports the runtime has published to the host. Empty if the runtime
    /// doesn't do port publishing (e.g. the in-memory mock).
    pub port_mappings: Vec<PortMapping>,
}

/// What every container backend must do.
///
/// Methods are async; mutating ops take `&mut self` so the in-memory mock can
/// keep using a `HashMap`. `Send + Sync` so the agent can hold one inside
/// `Arc<RwLock<_>>`.
#[async_trait::async_trait]
pub trait Runtime: Send + Sync {
    /// Create + start a container. `name` is the (already namespaced) container
    /// name; `container` carries the image, command/args, env, working_dir, etc.
    /// Returns an opaque runtime-specific ID (`docker://...`, `containerd://...`).
    async fn create_and_start_container(
        &mut self,
        name: &str,
        container: &Container,
    ) -> anyhow::Result<String>;

    async fn is_container_running(&self, container_id: &str) -> anyhow::Result<bool>;

    /// Look up a container by the *name* the agent gave it
    /// (typically `<pod-name>-<container-name>`). Returns `None` if no such
    /// container exists. This is the heart of reconciliation: every tick the
    /// agent re-observes runtime state by name rather than trusting an
    /// in-memory map that resets across restarts.
    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>>;

    async fn get_logs(&self, container_id: &str, options: &LogOptions) -> anyhow::Result<String>;

    /// Stream container logs as they're produced. Used by `kubectl logs -f`.
    /// Default implementation just dumps the current log contents and ends —
    /// runtimes that support real streaming (Docker) override this.
    async fn stream_logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> anyhow::Result<LogStream> {
        let logs = self.get_logs(container_id, options).await?;
        let bytes = Bytes::from(logs.into_bytes());
        Ok(Box::pin(futures::stream::iter([Ok(bytes)])))
    }

    /// Spawn an exec session inside the container. Returns the streams the
    /// caller will pump bytes through. Default implementation errors out —
    /// runtimes that don't support exec just say so.
    async fn exec(
        &self,
        _container_id: &str,
        _cmd: Vec<String>,
        _tty: bool,
    ) -> anyhow::Result<ExecSession> {
        anyhow::bail!("exec not supported by {} runtime", self.name())
    }

    /// Best-effort name describing this backend (logged at startup).
    fn name(&self) -> &'static str;
}

/// Owned bundle returned by `Runtime::exec`. The agent pumps bytes between
/// `output`/`input` and the kubectl WebSocket; `resize` is called when the
/// client sends a terminal-size update.
pub struct ExecSession {
    pub id: String,
    pub output: LogStream,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    pub resize:
        Box<dyn Fn(u16, u16) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>,
}

/// Pick the right runtime for the host. macOS → Docker; Linux → embedded;
/// otherwise mock. Errors fall back to the mock so the agent at least
/// registers and is observable while you fix the real runtime.
pub async fn default(socket_path: &str) -> anyhow::Result<Box<dyn Runtime>> {
    select(socket_path, "auto").await
}

/// Like `default`, but with an explicit choice from `--runtime=`.
pub async fn select(
    socket_path: &str,
    name: &str,
) -> anyhow::Result<Box<dyn Runtime>> {
    let want = name.trim().to_ascii_lowercase();
    match want.as_str() {
        "auto" => auto_select(socket_path).await,
        "docker" => {
            #[cfg(target_os = "macos")]
            {
                let r = docker::DockerRuntime::new().await?;
                tracing::info!("using Docker runtime");
                return Ok(Box::new(r));
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = socket_path;
                anyhow::bail!("docker runtime is only compiled on macOS in this build")
            }
        }
        "embedded" => {
            #[cfg(target_os = "linux")]
            {
                let r = embedded::EmbeddedRuntime::new().await?;
                tracing::info!("using embedded (libcontainer) runtime");
                return Ok(Box::new(r));
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = socket_path;
                anyhow::bail!("embedded runtime is Linux-only")
            }
        }
        "mock" => {
            let r = mock::ContainerdRuntime::new(socket_path).await?;
            tracing::info!("using mock runtime");
            Ok(Box::new(r))
        }
        other => anyhow::bail!("unknown --runtime: {} (expected auto/docker/embedded/mock)", other),
    }
}

async fn auto_select(socket_path: &str) -> anyhow::Result<Box<dyn Runtime>> {
    #[cfg(target_os = "macos")]
    {
        match docker::DockerRuntime::new().await {
            Ok(r) => {
                tracing::info!("using Docker runtime (auto)");
                return Ok(Box::new(r));
            }
            Err(e) => {
                tracing::warn!("Docker unavailable ({}); falling back to mock", e);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        match embedded::EmbeddedRuntime::new().await {
            Ok(r) => {
                tracing::info!("using embedded (libcontainer) runtime (auto)");
                return Ok(Box::new(r));
            }
            Err(e) => {
                tracing::warn!("embedded runtime unavailable ({}); falling back to mock", e);
            }
        }
    }
    let mock = mock::ContainerdRuntime::new(socket_path).await?;
    tracing::info!("using mock runtime (auto)");
    Ok(Box::new(mock))
}
