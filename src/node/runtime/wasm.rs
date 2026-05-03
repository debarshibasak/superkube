//! WebAssembly container runtime backed by [`wasmtime`].
//!
//! Wasmtime is linked into the kais binary directly — there is no external
//! daemon, no separate `wasmtime` CLI, and no socket. Each "container" is
//! a WASIp1 module instantiated in its own [`Store`] and driven on a
//! background tokio task.
//!
//! ## Image references
//!
//! Three forms are accepted:
//!
//! * `file:///abs/path/to/foo.wasm` or `wasm:///abs/path/to/foo.wasm`
//! * an absolute path: `/abs/path/to/foo.wasm`
//! * an OCI reference: `ghcr.io/foo/bar:tag` — pulled via
//!   [`crate::node::oci::image::pull_wasm`], which understands both wasm
//!   artifact media types and ordinary OCI images that happen to contain
//!   a `.wasm` file.
//!
//! ## Stdout / stderr
//!
//! Captured into ring buffers ([`RingPipe`]). When full, the *oldest*
//! bytes are dropped to make room — no traps. `kubectl logs` returns the
//! current contents.
//!
//! ## Ports (inetd-style port mapping)
//!
//! If the container declares any [`ContainerPort`] with a `host_port`, we
//! bind a TCP listener on the host's port. Each accepted connection
//! spawns a *fresh* wasm instance whose stdin reads connection bytes and
//! whose stdout writes back to the connection. This matches the
//! "stdio-over-TCP" / inetd / WAGI-without-CGI-env model and works with
//! any WASIp1 module that talks line-oriented or framed I/O on stdin /
//! stdout. The module's own `_start` is *not* run separately when ports
//! are declared — every connection IS a `_start` invocation.
//!
//! ## Exec
//!
//! `Runtime::exec` re-instantiates the same compiled module with the
//! caller-supplied argv, plumbing stdin / stdout / stderr through the
//! kubectl WebSocket. Resize is a no-op (wasm has no terminal).

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::StreamExt;
use tokio::sync::Mutex;
use uuid::Uuid;
use wasmtime::{Config, Engine, Linker, Module, Store};
use wasmtime_wasi::p2::pipe::{AsyncReadStream, AsyncWriteStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StdoutStream, StreamError};
use wasmtime_wasi::p2::WasiCtxBuilder;
use wasmtime_wasi::preview1::{add_to_linker_async, WasiP1Ctx};
use wasmtime_wasi::I32Exit;

use crate::models::Container;
use crate::node::oci;

use super::{ContainerInfo, ExecSession, LogOptions, LogStream, PortMapping, Runtime};

const LOG_CAPACITY: usize = 1024 * 1024; // 1 MiB ring per stream
const STDIO_BUDGET: usize = 64 * 1024; // wasi async-write back-pressure window

pub struct WasmRuntime {
    engine: Engine,
    /// Where pulled `.wasm` modules are cached on disk.
    module_cache_root: PathBuf,
    containers: HashMap<String, ContainerEntry>,
    by_name: HashMap<String, String>,
}

struct ContainerEntry {
    id: String,
    started_at: DateTime<Utc>,
    /// Cached compiled module so exec / per-connection invocations avoid
    /// recompiling. Engine + Module are cheaply [`Clone`] (Arc-internally).
    module: Module,
    module_path: PathBuf,
    /// argv we'd build for a "default" run (from the container spec).
    /// Re-used by exec when the caller passes an empty cmd vec.
    base_argv: Vec<String>,
    /// env we'd set on the wasi ctx (k, v) pairs.
    base_env: Vec<(String, String)>,
    /// `None` while the run-once `_start` task is still in flight, or for
    /// listener-mode containers (which never run a single `_start`).
    /// `Some(code)` once the run-once task has finished. Listener-mode
    /// entries set this to `None` permanently and `running` is derived
    /// from listener task liveness instead — see `find_container`.
    exit: Arc<Mutex<Option<i32>>>,
    /// Aggregated stdio for `kubectl logs`. Per-connection / per-exec
    /// invocations *also* write into this buffer so users can see all
    /// activity in one place; the WAGI-style streams have their own
    /// dedicated pipes for the per-connection traffic.
    stdout: RingPipe,
    stderr: RingPipe,
    /// Out-of-band runtime errors (trap text, etc.).
    trap_text: Arc<Mutex<String>>,
    /// `true` if this container is in listener mode (declared host_port).
    listener_mode: bool,
    /// Listener / per-connection tasks. Aborting them on drop is what
    /// stops the container.
    #[allow(dead_code)]
    background: Vec<tokio::task::JoinHandle<()>>,
    port_mappings: Vec<PortMapping>,
}

impl WasmRuntime {
    pub async fn new() -> anyhow::Result<Self> {
        let mut config = Config::new();
        config.async_support(true);
        let engine = Engine::new(&config)?;
        let module_cache_root = default_cache_root();
        std::fs::create_dir_all(&module_cache_root)
            .with_context(&module_cache_root)?;
        Ok(Self {
            engine,
            module_cache_root,
            containers: HashMap::new(),
            by_name: HashMap::new(),
        })
    }
}

#[async_trait::async_trait]
impl Runtime for WasmRuntime {
    fn name(&self) -> &'static str {
        "wasm"
    }

    async fn create_and_start_container(
        &mut self,
        name: &str,
        container: &Container,
    ) -> anyhow::Result<String> {
        // 1. Resolve the wasm module path: local file or OCI pull.
        let module_path = oci::image::pull_wasm(&container.image, &self.module_cache_root)
            .await
            .map_err(|e| anyhow::anyhow!("wasm: resolving image {:?}: {}", container.image, e))?;
        tracing::info!("wasm: module {} for {}", module_path.display(), name);

        // 2. Compile it once. Module is internally Arc'd, cheap to clone.
        let module = Module::from_file(&self.engine, &module_path)?;

        // 3. Build the standard argv and env we'd use for either run-once
        //    or a from-scratch exec / connection invocation.
        let mut base_argv = vec![module_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.to_string())];
        if let Some(cmd) = &container.command {
            base_argv.extend(cmd.iter().cloned());
        }
        if let Some(args) = &container.args {
            base_argv.extend(args.iter().cloned());
        }

        let mut base_env: Vec<(String, String)> = Vec::new();
        if let Some(env) = &container.env {
            for v in env {
                base_env.push((v.name.clone(), v.value.clone().unwrap_or_default()));
            }
        }

        // 4. Aggregated log capture (always present).
        let stdout = RingPipe::new(LOG_CAPACITY);
        let stderr = RingPipe::new(LOG_CAPACITY);
        let trap_text = Arc::new(Mutex::new(String::new()));

        // 5. Decide between listener mode and run-once mode.
        let mut port_mappings: Vec<PortMapping> = Vec::new();
        let mut declared_host_ports: Vec<(u16, u16, String)> = Vec::new();
        if let Some(ports) = &container.ports {
            for p in ports {
                if let Some(host_port) = p.host_port {
                    let host_port = host_port.max(0) as u16;
                    let container_port = p.container_port.max(0) as u16;
                    let protocol = match &p.protocol {
                        Some(crate::models::Protocol::UDP) => "udp",
                        _ => "tcp",
                    };
                    declared_host_ports.push((
                        host_port,
                        container_port,
                        protocol.to_string(),
                    ));
                    port_mappings.push(PortMapping {
                        container_port: container_port as i32,
                        host_port,
                        protocol: protocol.to_string(),
                    });
                }
            }
        }

        let listener_mode = !declared_host_ports.is_empty();
        let exit = Arc::new(Mutex::new(None::<i32>));
        let mut background: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        if listener_mode {
            // Bind one TCP listener per declared host_port and spawn an
            // accept loop. UDP is acknowledged in port_mappings but not
            // wired up — TCP is the only protocol an inetd-style stdio
            // wrapper makes sense for.
            for (host_port, _container_port, protocol) in declared_host_ports {
                if protocol != "tcp" {
                    tracing::warn!(
                        "wasm: skipping non-tcp port mapping host_port={} (proto={})",
                        host_port,
                        protocol
                    );
                    continue;
                }
                let addr: std::net::SocketAddr =
                    ([0, 0, 0, 0], host_port).into();
                let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
                    anyhow::anyhow!("wasm: bind 0.0.0.0:{}: {}", host_port, e)
                })?;
                tracing::info!(
                    "wasm: listening on {} for container {}",
                    listener.local_addr().unwrap_or(addr),
                    name
                );
                let engine = self.engine.clone();
                let module_clone = module.clone();
                let argv = base_argv.clone();
                let env = base_env.clone();
                let agg_stdout = stdout.clone();
                let agg_stderr = stderr.clone();
                let agg_trap = trap_text.clone();
                let module_disp = module_path.clone();
                let handle = tokio::spawn(async move {
                    accept_loop(
                        listener,
                        engine,
                        module_clone,
                        argv,
                        env,
                        agg_stdout,
                        agg_stderr,
                        agg_trap,
                        module_disp,
                    )
                    .await;
                });
                background.push(handle);
            }
        } else {
            // Run-once: instantiate, call _start, capture exit.
            let stdout_t = stdout.clone();
            let stderr_t = stderr.clone();
            let trap_t = trap_text.clone();
            let argv = base_argv.clone();
            let env = base_env.clone();
            let module_clone = module.clone();
            let engine = self.engine.clone();
            let exit_t = exit.clone();
            let module_disp = module_path.clone();
            let handle = tokio::spawn(async move {
                let code = run_once(
                    engine,
                    module_clone,
                    argv,
                    env,
                    stdout_t,
                    stderr_t,
                    trap_t,
                    module_disp,
                )
                .await;
                *exit_t.lock().await = Some(code);
            });
            background.push(handle);
        }

        let id = format!("wasm://{}", Uuid::new_v4());
        let now = Utc::now();
        self.containers.insert(
            id.clone(),
            ContainerEntry {
                id: id.clone(),
                started_at: now,
                module,
                module_path,
                base_argv,
                base_env,
                exit,
                stdout,
                stderr,
                trap_text,
                listener_mode,
                background,
                port_mappings,
            },
        );
        self.by_name.insert(name.to_string(), id.clone());
        tracing::info!("wasm: container {} started as {}", name, id);
        Ok(id)
    }

    async fn is_container_running(&self, container_id: &str) -> anyhow::Result<bool> {
        match self.containers.get(container_id) {
            Some(e) => Ok(running(e).await),
            None => Ok(false),
        }
    }

    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>> {
        let id = match self.by_name.get(name) {
            Some(id) => id,
            None => return Ok(None),
        };
        let entry = match self.containers.get(id) {
            Some(e) => e,
            None => return Ok(None),
        };
        let exit = *entry.exit.lock().await;
        Ok(Some(ContainerInfo {
            id: entry.id.clone(),
            running: running(entry).await,
            restart_count: 0,
            started_at: Some(entry.started_at),
            exit_code: exit,
            ip: None,
            port_mappings: entry.port_mappings.clone(),
        }))
    }

    async fn get_logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> anyhow::Result<String> {
        let entry = self
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("container {} not found", container_id))?;
        let mut out = Vec::new();
        out.extend_from_slice(&entry.stdout.contents());
        out.extend_from_slice(&entry.stderr.contents());
        let trap = entry.trap_text.lock().await.clone();
        if !trap.is_empty() {
            out.extend_from_slice(trap.as_bytes());
        }
        let mut s = String::from_utf8_lossy(&out).into_owned();

        if let Some(tail) = options.tail_lines {
            let tail = tail.max(0) as usize;
            let kept: Vec<&str> = s.lines().rev().take(tail).collect();
            s = kept.into_iter().rev().collect::<Vec<_>>().join("\n");
            if !s.is_empty() {
                s.push('\n');
            }
        }
        if let Some(limit) = options.limit_bytes {
            let limit = limit.max(0) as usize;
            if s.len() > limit {
                s.truncate(limit);
            }
        }
        if options.timestamps {
            let ts = entry.started_at.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string();
            s = s
                .lines()
                .map(|l| format!("{ts} {l}"))
                .collect::<Vec<_>>()
                .join("\n");
            if !s.is_empty() {
                s.push('\n');
            }
        }
        Ok(s)
    }

    async fn exec(
        &self,
        container_id: &str,
        cmd: Vec<String>,
        _tty: bool,
    ) -> anyhow::Result<ExecSession> {
        let entry = self
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("container {} not found", container_id))?;

        // argv: if the caller didn't supply a command, replay the original
        // container argv; otherwise use what they sent (with the binary
        // name as argv[0] for parity with how container runtimes do it).
        let argv: Vec<String> = if cmd.is_empty() {
            entry.base_argv.clone()
        } else {
            let mut v = vec![entry
                .module_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "module.wasm".to_string())];
            v.extend(cmd);
            v
        };

        // stdin: client writes → wasi reads.
        let (stdin_client_write, stdin_wasi_read) = tokio::io::duplex(STDIO_BUDGET);
        // stdout/stderr: wasi writes → client reads (merged).
        let (stdout_wasi_write, stdout_client_read) = tokio::io::duplex(STDIO_BUDGET);
        let (stderr_wasi_write, stderr_client_read) = tokio::io::duplex(STDIO_BUDGET);

        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder
            .stdin(AsyncStdinAdapter::new(stdin_wasi_read))
            .stdout(AsyncStdoutAdapter::new(stdout_wasi_write))
            .stderr(AsyncStdoutAdapter::new(stderr_wasi_write))
            .args(&argv);
        for (k, v) in &entry.base_env {
            wasi_builder.env(k, v);
        }
        let wasi = wasi_builder.build_p1();

        let module = entry.module.clone();
        let engine = self.engine.clone();
        let module_path = entry.module_path.clone();
        tokio::spawn(async move {
            let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
            if let Err(e) = add_to_linker_async(&mut linker, |ctx| ctx) {
                tracing::warn!("wasm exec: linker setup failed: {}", e);
                return;
            }
            let mut store = Store::new(&engine, wasi);
            let instance = match linker.instantiate_async(&mut store, &module).await {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!("wasm exec: instantiate failed: {}", e);
                    return;
                }
            };
            let start = match instance.get_typed_func::<(), ()>(&mut store, "_start") {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("wasm exec: missing _start: {}", e);
                    return;
                }
            };
            if let Err(e) = start.call_async(&mut store, ()).await {
                if e.downcast_ref::<I32Exit>().is_none() {
                    tracing::warn!("wasm exec trap in {}: {}", module_path.display(), e);
                }
            }
        });

        // Merge stdout + stderr into one Bytes stream for the client.
        let stdout_stream = tokio_util::io::ReaderStream::new(stdout_client_read);
        let stderr_stream = tokio_util::io::ReaderStream::new(stderr_client_read);
        let merged = futures::stream::select(stdout_stream, stderr_stream);
        let output: LogStream = Box::pin(
            merged.map(|r| r.map(Bytes::from).map_err(anyhow::Error::from)),
        );

        let session_id = format!("wasm-exec://{}", Uuid::new_v4());
        Ok(ExecSession {
            id: session_id,
            output,
            input: Box::pin(stdin_client_write),
            // wasm has no terminal — resize is a no-op.
            resize: Box::new(|_, _| Box::pin(async { Ok(()) })),
        })
    }
}

/// Compute liveness, accounting for listener-mode containers (which run
/// indefinitely until kais shuts them down).
async fn running(entry: &ContainerEntry) -> bool {
    if entry.listener_mode {
        // Alive as long as at least one accept loop is still running.
        entry.background.iter().any(|h| !h.is_finished())
    } else {
        entry.exit.lock().await.is_none()
    }
}

/// Drive `_start` once and return the exit code (0 on clean return,
/// `I32Exit` value on `proc_exit`, 1 on trap). Trap text is appended to
/// `trap_text`.
async fn run_once(
    engine: Engine,
    module: Module,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    stdout_pipe: RingPipe,
    stderr_pipe: RingPipe,
    trap_text: Arc<Mutex<String>>,
    module_path_for_log: PathBuf,
) -> i32 {
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder
        .stdout(stdout_pipe)
        .stderr(stderr_pipe)
        .args(&argv);
    for (k, v) in &env {
        wasi_builder.env(k, v);
    }
    let wasi = wasi_builder.build_p1();

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    if let Err(e) = add_to_linker_async(&mut linker, |ctx| ctx) {
        trap_text
            .lock()
            .await
            .push_str(&format!("wasm linker setup: {e}\n"));
        return 1;
    }
    let mut store = Store::new(&engine, wasi);
    let instance = match linker.instantiate_async(&mut store, &module).await {
        Ok(i) => i,
        Err(e) => {
            trap_text
                .lock()
                .await
                .push_str(&format!("wasm instantiate {}: {}\n", module_path_for_log.display(), e));
            return 1;
        }
    };
    let start = match instance.get_typed_func::<(), ()>(&mut store, "_start") {
        Ok(s) => s,
        Err(e) => {
            trap_text
                .lock()
                .await
                .push_str(&format!("wasm: module has no _start: {e}\n"));
            return 1;
        }
    };
    match start.call_async(&mut store, ()).await {
        Ok(()) => 0,
        Err(e) => {
            if let Some(I32Exit(code)) = e.downcast_ref::<I32Exit>() {
                *code
            } else {
                trap_text
                    .lock()
                    .await
                    .push_str(&format!("wasm trap in {}: {:?}\n", module_path_for_log.display(), e));
                1
            }
        }
    }
}

/// Per-port accept loop: each TCP connection spawns a fresh wasm instance
/// whose stdin reads connection bytes and whose stdout writes back. stderr
/// is piped into the aggregated container log.
#[allow(clippy::too_many_arguments)]
async fn accept_loop(
    listener: tokio::net::TcpListener,
    engine: Engine,
    module: Module,
    base_argv: Vec<String>,
    base_env: Vec<(String, String)>,
    agg_stdout: RingPipe,
    agg_stderr: RingPipe,
    agg_trap: Arc<Mutex<String>>,
    module_path_for_log: PathBuf,
) {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("wasm: accept on {:?} failed: {}", listener.local_addr(), e);
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };
        // Mirror writes into the aggregated container log AS WELL AS to
        // the connection, so `kubectl logs` shows what the module said.
        let (sock_read, sock_write) = sock.into_split();

        let engine = engine.clone();
        let module = module.clone();
        let mut argv = base_argv.clone();
        // WAGI-ish env: surface the peer for the module if it cares.
        let mut env = base_env.clone();
        env.push(("REMOTE_ADDR".to_string(), peer.ip().to_string()));
        env.push(("REMOTE_PORT".to_string(), peer.port().to_string()));
        // argv[0] should already be the module file name; expose the URL-ish
        // path as argv[1] for CGI-style modules that read it.
        if argv.len() == 1 {
            argv.push("/".to_string());
        }

        let agg_stdout = agg_stdout.clone();
        let agg_stderr = agg_stderr.clone();
        let agg_trap = agg_trap.clone();
        let module_path_for_log = module_path_for_log.clone();
        tokio::spawn(async move {
            // stdin = bytes from the client.
            let stdin = AsyncStdinAdapter::new(sock_read);
            // stdout = TeeWriter(connection, aggregated stdout).
            let tee_stdout = TeeWriter::new(sock_write, agg_stdout);
            let stdout = AsyncStdoutAdapter::new(tee_stdout);
            // stderr = aggregated container log only — connections don't
            // care about diagnostic text.
            let stderr = agg_stderr;

            let mut wasi_builder = WasiCtxBuilder::new();
            wasi_builder
                .stdin(stdin)
                .stdout(stdout)
                .stderr(stderr)
                .args(&argv);
            for (k, v) in &env {
                wasi_builder.env(k, v);
            }
            let wasi = wasi_builder.build_p1();

            let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
            if let Err(e) = add_to_linker_async(&mut linker, |ctx| ctx) {
                agg_trap
                    .lock()
                    .await
                    .push_str(&format!("wasm conn linker: {e}\n"));
                return;
            }
            let mut store = Store::new(&engine, wasi);
            let instance = match linker.instantiate_async(&mut store, &module).await {
                Ok(i) => i,
                Err(e) => {
                    agg_trap
                        .lock()
                        .await
                        .push_str(&format!("wasm conn instantiate: {e}\n"));
                    return;
                }
            };
            let start = match instance.get_typed_func::<(), ()>(&mut store, "_start") {
                Ok(s) => s,
                Err(e) => {
                    agg_trap
                        .lock()
                        .await
                        .push_str(&format!("wasm conn: no _start: {e}\n"));
                    return;
                }
            };
            if let Err(e) = start.call_async(&mut store, ()).await {
                if e.downcast_ref::<I32Exit>().is_none() {
                    agg_trap.lock().await.push_str(&format!(
                        "wasm conn trap in {}: {:?}\n",
                        module_path_for_log.display(),
                        e
                    ));
                }
            }
        });
    }
}

// -- Stdio adapters -----------------------------------------------------------

/// Wrap a `tokio::io::AsyncRead` into something `WasiCtxBuilder::stdin`
/// accepts, which is a [`StdinStream`]. We reuse [`AsyncReadStream`] —
/// the wasi crate supplies this directly.
struct AsyncStdinAdapter<R: tokio::io::AsyncRead + Send + Unpin + 'static> {
    inner: StdMutex<Option<R>>,
}

impl<R: tokio::io::AsyncRead + Send + Unpin + 'static> AsyncStdinAdapter<R> {
    fn new(reader: R) -> Self {
        Self {
            inner: StdMutex::new(Some(reader)),
        }
    }
}

impl<R: tokio::io::AsyncRead + Send + Unpin + 'static> wasmtime_wasi::p2::StdinStream
    for AsyncStdinAdapter<R>
{
    fn stream(&self) -> Box<dyn wasmtime_wasi::p2::InputStream> {
        // `WasiCtxBuilder::stdin` calls `stream()` once when the context
        // is built; we hand the reader off here.
        let r = self
            .inner
            .lock()
            .unwrap()
            .take()
            .expect("AsyncStdinAdapter: stream() called twice");
        Box::new(AsyncReadStream::new(r))
    }
    fn isatty(&self) -> bool {
        false
    }
}

/// Wrap a `tokio::io::AsyncWrite` for stdout / stderr.
struct AsyncStdoutAdapter<W: tokio::io::AsyncWrite + Send + Unpin + 'static> {
    inner: StdMutex<Option<W>>,
}

impl<W: tokio::io::AsyncWrite + Send + Unpin + 'static> AsyncStdoutAdapter<W> {
    fn new(writer: W) -> Self {
        Self {
            inner: StdMutex::new(Some(writer)),
        }
    }
}

impl<W: tokio::io::AsyncWrite + Send + Unpin + 'static> StdoutStream for AsyncStdoutAdapter<W> {
    fn stream(&self) -> Box<dyn OutputStream> {
        let w = self
            .inner
            .lock()
            .unwrap()
            .take()
            .expect("AsyncStdoutAdapter: stream() called twice");
        Box::new(AsyncWriteStream::new(STDIO_BUDGET, w))
    }
    fn isatty(&self) -> bool {
        false
    }
}

/// Splits writes between a `tokio::io::AsyncWrite` (the live connection)
/// and a [`RingPipe`] (the aggregated kubectl-logs buffer). Used in
/// listener mode so `kubectl logs` still shows what each connection said.
struct TeeWriter<W> {
    primary: W,
    mirror: RingPipe,
}

impl<W: tokio::io::AsyncWrite + Unpin> TeeWriter<W> {
    fn new(primary: W, mirror: RingPipe) -> Self {
        Self { primary, mirror }
    }
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for TeeWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // Mirror best-effort: ring buffer never blocks and never errors.
        self.mirror.append(buf);
        Pin::new(&mut self.primary).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.primary).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.primary).poll_shutdown(cx)
    }
}

// -- Ring-buffered output pipe ------------------------------------------------

/// Bounded stdio capture pipe that drops *oldest* bytes on overflow
/// instead of trapping the guest like `MemoryOutputPipe` does. Plays the
/// role of stdout / stderr inside the wasm sandbox.
#[derive(Debug, Clone)]
pub(crate) struct RingPipe {
    capacity: usize,
    buffer: Arc<StdMutex<bytes::BytesMut>>,
}

impl RingPipe {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            buffer: Arc::new(StdMutex::new(bytes::BytesMut::new())),
        }
    }

    fn contents(&self) -> Bytes {
        self.buffer.lock().unwrap().clone().freeze()
    }

    /// Append-with-drop. Public so [`TeeWriter`] can mirror raw bytes
    /// into the same buffer the wasi pipe writes to.
    fn append(&self, bytes: &[u8]) {
        let mut buf = self.buffer.lock().unwrap();
        let total = buf.len() + bytes.len();
        if total > self.capacity {
            let buf_len = buf.len();
            let drop_n = (total - self.capacity).min(buf_len);
            // BytesMut has no cheap front-drop; copy what's left forward.
            let keep = buf_len - drop_n;
            buf.copy_within(drop_n..buf_len, 0);
            buf.truncate(keep);
        }
        // If a single write exceeds capacity, keep only its tail.
        let to_append = if bytes.len() > self.capacity {
            &bytes[bytes.len() - self.capacity..]
        } else {
            bytes
        };
        buf.extend_from_slice(to_append);
    }
}

#[async_trait::async_trait]
impl OutputStream for RingPipe {
    fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        self.append(&bytes);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), StreamError> {
        Ok(())
    }
    fn check_write(&mut self) -> Result<usize, StreamError> {
        // Always have room; we drop old bytes instead of refusing writes.
        Ok(self.capacity)
    }
}

#[async_trait::async_trait]
impl Pollable for RingPipe {
    async fn ready(&mut self) {}
}

impl StdoutStream for RingPipe {
    fn stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }
    fn isatty(&self) -> bool {
        false
    }
}

// -- Misc helpers -------------------------------------------------------------

fn default_cache_root() -> PathBuf {
    // Prefer the same /var/lib/superkube tree the embedded runtime uses
    // when it's writable; fall back to the user cache dir or /tmp so
    // dev / non-root invocations on macOS still work.
    let primary = PathBuf::from("/var/lib/superkube/wasm");
    if writable(&primary) {
        return primary;
    }
    if let Some(mut cache) = dirs_cache_dir() {
        cache.push("superkube-wasm");
        return cache;
    }
    std::env::temp_dir().join("superkube-wasm")
}

fn writable(p: &std::path::Path) -> bool {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).is_ok()
            && std::fs::metadata(parent)
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false)
    } else {
        false
    }
}

/// Hand-rolled equivalent of `dirs::cache_dir()` for `$HOME/.cache`-ish
/// behavior without pulling another dep. Returns `None` if `$HOME` is
/// unset.
fn dirs_cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg));
    }
    let home = std::env::var_os("HOME")?;
    if cfg!(target_os = "macos") {
        let mut p = PathBuf::from(home);
        p.push("Library/Caches");
        Some(p)
    } else {
        let mut p = PathBuf::from(home);
        p.push(".cache");
        Some(p)
    }
}

trait WithContext<T> {
    fn with_context(self, target: &std::path::Path) -> anyhow::Result<T>;
}

impl<T> WithContext<T> for std::io::Result<T> {
    fn with_context(self, target: &std::path::Path) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("wasm runtime: creating {}: {}", target.display(), e))
    }
}
