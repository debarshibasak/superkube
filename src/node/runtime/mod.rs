//! Container runtime abstraction.
//!
//! Several engines sit behind one trait, and a node runs every engine it can
//! construct at once. A pod picks its engine through `spec.runtimeClassName`
//! and falls back to the node's default (`--runtime=`):
//!   - `embedded` : libcontainer + in-process image pull. Linux-only, no
//!                  daemon. The default on Linux.
//!   - `docker`   : talks to Docker Desktop / dockerd via /var/run/docker.sock.
//!                  Compiled on macOS, where a Linux VM is the only way to
//!                  run Linux containers. The default there.
//!   - `wasm`     : wasmtime linked in; runs WASI modules on any OS.
//!   - `mock`     : in-memory stub, works everywhere — useful for dev and tests.
//!
//! [`Engines`] is the per-node multiplexer. Container IDs carry their engine
//! as a URL scheme (`embedded://…`, `docker://…`, `wasm://…`, `containerd://…`
//! for the mock), so log / exec requests route back to the engine that
//! minted them without the agent having to remember.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::Stream;
use tokio::io::AsyncWrite;

use crate::models::Container;

mod mock;
mod wasm;
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
    /// Real network address the container reachable on. For Docker this is
    /// the address inside Docker's bridge VM (e.g. `172.17.0.5`); for the
    /// embedded runtime it's the pod's veth address on the host bridge. The
    /// agent stamps this on `pod.status.podIP` so kubectl/exec see the
    /// address that other containers actually route to.
    pub ip: Option<String>,
    /// Ports the runtime has published to the host. Empty if the runtime
    /// doesn't do port publishing (the mock, and the embedded runtime —
    /// whose pod IPs the host can dial directly).
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

    /// Look up a container by the *name* the agent gave it
    /// (typically `<pod-name>-<container-name>`). Returns `None` if no such
    /// container exists. This is the heart of reconciliation: every tick the
    /// agent re-observes runtime state by name rather than trusting an
    /// in-memory map that resets across restarts.
    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>>;

    /// Stop and remove a container the agent no longer wants — its pod was
    /// deleted or moved. Must be idempotent: unknown names are not an error.
    /// Default no-op for engines with nothing worth cleaning up.
    async fn remove_container(&mut self, _name: &str) -> anyhow::Result<()> {
        Ok(())
    }

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
    pub output: LogStream,
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
    pub resize:
        Box<dyn Fn(u16, u16) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>,
    /// Resolves with the process's exit code once it has ended, for runtimes
    /// that can tell. `None` means unknown, and the agent reports success.
    pub exit: Option<tokio::sync::oneshot::Receiver<i32>>,
}

/// Every engine this node can run, keyed by the name a pod selects with
/// `spec.runtimeClassName`. The agent holds exactly one of these.
///
/// Implements [`Runtime`] itself so the agent's log / exec / lookup paths
/// don't care how many engines sit underneath: calls that carry a container
/// ID route by its scheme, lookups by name ask each engine in turn (default
/// first), and creates without an explicit class go to the default.
pub struct Engines {
    default: String,
    /// Default engine first — `find_container` asks in this order.
    engines: Vec<(String, Box<dyn Runtime>)>,
}

impl Engines {
    /// Build the default engine for `default_kind` (`auto`/`docker`/
    /// `embedded`/`mock`/`wasm`) plus every peer that is free to have around.
    ///
    /// Peers: `wasm` runs anywhere with no host dependencies; `docker` only
    /// costs a ping. The embedded engine is *not* added as a peer because
    /// constructing it touches the host (bridge, iptables, `/var/lib`) — on
    /// Linux it's the default anyway, elsewhere it doesn't compile.
    pub async fn build(socket_path: &str, default_kind: &str) -> anyhow::Result<Self> {
        let default_engine = select(socket_path, default_kind).await?;
        let default = default_engine.name().to_string();
        let mut engines: Vec<(String, Box<dyn Runtime>)> = vec![(default.clone(), default_engine)];

        if !engines.iter().any(|(n, _)| n == "wasm") {
            match wasm::WasmRuntime::new().await {
                Ok(r) => engines.push(("wasm".to_string(), Box::new(r))),
                Err(e) => tracing::warn!("wasm engine unavailable: {}", e),
            }
        }
        #[cfg(target_os = "macos")]
        if !engines.iter().any(|(n, _)| n == "docker") {
            match docker::DockerRuntime::new().await {
                Ok(r) => engines.push(("docker".to_string(), Box::new(r))),
                Err(e) => tracing::debug!("docker engine unavailable as a peer: {}", e),
            }
        }

        Ok(Self { default, engines })
    }

    pub fn default_name(&self) -> &str {
        &self.default
    }

    pub fn names(&self) -> Vec<&str> {
        self.engines.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// The engine a pod asked for, or the default when it didn't. An unknown
    /// class is an error the agent surfaces on the pod (`kubectl describe`
    /// shows it) rather than silently running on the wrong engine.
    pub fn for_class(&mut self, class: Option<&str>) -> anyhow::Result<&mut dyn Runtime> {
        let want = class.unwrap_or(&self.default).to_string();
        let names = self.names().join(", ");
        for (n, e) in self.engines.iter_mut() {
            if *n == want {
                let engine: &mut dyn Runtime = e.as_mut();
                return Ok(engine);
            }
        }
        Err(anyhow::anyhow!(
            "no engine named {:?} on this node (have: {})",
            want,
            names
        ))
    }

    /// Route a container ID back to the engine that minted it.
    fn by_id(&self, id: &str) -> anyhow::Result<&dyn Runtime> {
        let scheme = id.split_once("://").map(|(s, _)| s).unwrap_or("");
        let name = match scheme {
            "containerd" => "mock",
            s => s,
        };
        self.engines
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| e.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!("container {}: no engine for scheme {:?} on this node", id, scheme)
            })
    }
}

#[async_trait::async_trait]
impl Runtime for Engines {
    fn name(&self) -> &'static str {
        "engines"
    }

    async fn create_and_start_container(
        &mut self,
        name: &str,
        container: &Container,
    ) -> anyhow::Result<String> {
        self.for_class(None)?
            .create_and_start_container(name, container)
            .await
    }

    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>> {
        for (i, (engine_name, engine)) in self.engines.iter().enumerate() {
            match engine.find_container(name).await {
                Ok(Some(info)) => return Ok(Some(info)),
                Ok(None) => {}
                // The default engine's failures are the node's failures; a
                // peer that's having a bad day (Docker daemon went away)
                // shouldn't stop pods on the other engines from reconciling.
                Err(e) if i == 0 => return Err(e),
                Err(e) => tracing::debug!("{} engine: find {}: {}", engine_name, name, e),
            }
        }
        Ok(None)
    }

    async fn remove_container(&mut self, name: &str) -> anyhow::Result<()> {
        let mut first_err = None;
        for (_, engine) in self.engines.iter_mut() {
            if let Err(e) = engine.remove_container(name).await {
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn get_logs(&self, container_id: &str, options: &LogOptions) -> anyhow::Result<String> {
        self.by_id(container_id)?.get_logs(container_id, options).await
    }

    async fn stream_logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> anyhow::Result<LogStream> {
        self.by_id(container_id)?.stream_logs(container_id, options).await
    }

    async fn exec(
        &self,
        container_id: &str,
        cmd: Vec<String>,
        tty: bool,
    ) -> anyhow::Result<ExecSession> {
        self.by_id(container_id)?.exec(container_id, cmd, tty).await
    }
}

/// Construct a single engine by name. `auto` picks the host's native one —
/// Docker on macOS, embedded on Linux — and falls back to the mock if that
/// fails, so the node at least registers and is observable while you fix
/// the real runtime.
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
        "wasm" => {
            let r = wasm::WasmRuntime::new().await?;
            tracing::info!("using wasm (wasmtime) runtime");
            Ok(Box::new(r))
        }
        other => anyhow::bail!(
            "unknown --runtime: {} (expected auto/docker/embedded/mock/wasm)",
            other
        ),
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
