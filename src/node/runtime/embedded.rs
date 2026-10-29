//! Linux-only embedded OCI runtime.
//!
//! Pulls images via [`crate::node::oci::image`], builds an OCI bundle via
//! [`crate::node::oci::bundle`], and hands it to youki's `libcontainer` to
//! create namespaces, set up cgroups v2, pivot_root, and exec the container's
//! init process. All in-process — no host containerd / runc / docker daemon.
//!
//! * **Networking.** Each container gets a netns + veth pair plugged into the
//!   host `superkube0` bridge (see [`crate::node::network`]). The pod IP is
//!   reachable from the host, which is how the NodePort proxy and
//!   `port-forward` get to it — nothing is published to host ports.
//! * **Logs.** The init process's stdout and stderr are pipes we own. One
//!   reader task per stream appends timestamped lines to
//!   `/var/lib/superkube/logs/<name>.log` and fans them out to live
//!   `kubectl logs -f` followers over a broadcast channel.
//! * **Exec.** A libcontainer *tenant* process joins the container's
//!   namespaces. Non-TTY execs get three pipes; TTY execs get a pty whose
//!   master libcontainer hands back over a console socket (`SCM_RIGHTS`).
//! * **Exit status.** libcontainer clones init with `CLONE_PARENT`, so it is
//!   a direct child of this process — a blocking `waitpid` per container
//!   records the real exit code and keeps zombies away.
//!
//! Not yet: resource limits / seccomp / AppArmor (the bundle generator just
//! doesn't emit them), and recovering containers across an agent restart
//! (the container table is in-memory, so a restarted agent starts fresh
//! while the old init processes keep running).

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use futures::StreamExt;
use libcontainer::workload::default::DefaultExecutor;
use libcontainer::workload::{Executor, ExecutorError, ExecutorSetEnvsError, ExecutorValidationError};
use nix::fcntl::OFlag;
use nix::libc;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;
// libcontainer pins its own oci-spec; its Executor trait speaks that one.
use libcontainer::oci_spec::runtime::Spec as LcSpec;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::unix::pipe;
use tokio::sync::{broadcast, mpsc, watch, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::ReaderStream;

use crate::models::Container as PodContainer;
use crate::node::network;
use crate::node::oci;
use crate::node::oci::bundle::RootlessConfig;

use super::{ContainerInfo, ExecSession, LogOptions, LogStream, PortMapping, Runtime};

const STATE_ROOT: &str = "/var/lib/superkube";
const ID_PREFIX: &str = "embedded://";
/// /24 the embedded runtime allocates pod IPs from. The CIDR config flag's
/// first three octets feed in here through `superkube node`'s plumbing in a
/// follow-up; for now we hard-code the historical default.
const POD_CIDR_PREFIX: &str = "10.244.0";
/// How long `remove_container` gives init after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// Live log fan-out buffer per container. Followers that fall further
/// behind than this skip ahead rather than stall the writer.
const LOG_FANOUT_CAPACITY: usize = 256;

pub struct EmbeddedRuntime {
    /// Where libcontainer stashes its per-container state directories.
    state_path: PathBuf,
    /// Where image rootfs trees live.
    image_root: PathBuf,
    /// Where we write bundle/<name>/{rootfs, config.json} for each container.
    bundle_root: PathBuf,
    /// Where captured stdout/stderr lands, one file per container.
    log_root: PathBuf,
    containers: Arc<Mutex<HashMap<String, ContainerEntry>>>,
    ipam: Arc<network::Ipam>,
    /// Set when libcontainer would otherwise reject the bundle with
    /// `NoUserNamespace` — non-root euid, or already inside a user namespace.
    rootless: Option<RootlessConfig>,
    /// Detached teardowns by container name, so a re-create of the same name
    /// waits for the previous instance to be fully gone.
    pending_teardowns: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

/// What [`teardown`] needs from the runtime, owned so it can run detached.
#[derive(Clone)]
struct TeardownCtx {
    state_path: PathBuf,
    bundle_root: PathBuf,
    ipam: Arc<network::Ipam>,
}

struct ContainerEntry {
    name: String,
    started_at: DateTime<Utc>,
    #[allow(dead_code)]
    image: String,
    /// Per-pod network handle, kept for teardown.
    pod_net: Option<network::PodNetwork>,
    /// Init process pid — our direct child, see module docs.
    pid: i32,
    /// Times this name has been (re)started by us.
    restart_count: i32,
    /// Filled in by the reaper once init exits.
    exit: Arc<std::sync::Mutex<Option<Exit>>>,
    log: Arc<ContainerLog>,
}

impl ContainerEntry {
    fn exit_state(&self) -> Option<Exit> {
        *self.exit.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[derive(Clone, Copy, Debug)]
struct Exit {
    code: i32,
    #[allow(dead_code)]
    at: DateTime<Utc>,
}

impl EmbeddedRuntime {
    pub async fn new() -> anyhow::Result<Self> {
        tracing::info!("embedded runtime: initializing (state root={})", STATE_ROOT);
        let state_path = PathBuf::from(STATE_ROOT).join("state");
        let image_root = PathBuf::from(STATE_ROOT).join("images");
        let bundle_root = PathBuf::from(STATE_ROOT).join("bundles");
        let log_root = PathBuf::from(STATE_ROOT).join("logs");
        for d in [&state_path, &image_root, &bundle_root, &log_root] {
            std::fs::create_dir_all(d).map_err(|e| {
                anyhow::anyhow!("creating {}: {}", d.display(), e)
            })?;
        }

        if !std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            tracing::warn!(
                "embedded runtime: cgroups v2 not detected (/sys/fs/cgroup/cgroup.controllers \
                 missing). Containers may fail to start."
            );
        } else if let Err(e) = ensure_cgroup_root_is_empty() {
            tracing::warn!("embedded runtime: cgroup root setup: {e}");
        }

        let ipam = Arc::new(network::Ipam::new(POD_CIDR_PREFIX));

        // Set up the host bridge + MASQUERADE once. Failing here doesn't kill
        // the runtime — pods will run with no networking, but we'll log loudly.
        let gateway = ipam.gateway();
        if let Err(e) = init_host_network(gateway).await {
            tracing::warn!("embedded: host network setup failed ({e:#}); pods will lack connectivity");
        }

        let rootless = detect_rootless();
        if let Some(rc) = rootless {
            tracing::info!(
                "embedded runtime: rootless mode (host uid={}, gid={}); user namespace + 1:1 mapping will be added to each bundle",
                rc.host_uid,
                rc.host_gid
            );
        }

        Ok(Self {
            state_path,
            image_root,
            bundle_root,
            log_root,
            containers: Arc::new(Mutex::new(HashMap::new())),
            ipam,
            rootless,
            pending_teardowns: Mutex::new(HashMap::new()),
        })
    }

    fn teardown_ctx(&self) -> TeardownCtx {
        TeardownCtx {
            state_path: self.state_path.clone(),
            bundle_root: self.bundle_root.clone(),
            ipam: Arc::clone(&self.ipam),
        }
    }

    /// Block until a detached teardown of `name`, if any, has finished.
    async fn await_pending_teardown(&self, name: &str) {
        let pending = self.pending_teardowns.lock().await.remove(name);
        if let Some(handle) = pending {
            let _ = handle.await;
        }
    }
}

/// Stop init, release the libcontainer state + cgroup, the pod network, the
/// bundle, and the log file. Everything is best-effort: by the time we get
/// here the kernel may already have torn some of it down.
async fn teardown(ctx: TeardownCtx, entry: ContainerEntry) {
    let name = entry.name.clone();

    // 1. Stop. Init is pid 1 of its pid namespace, so its death takes the
    //    whole namespace with it. The reaper records the exit.
    if entry.exit_state().is_none() {
        let pid = Pid::from_raw(entry.pid);
        let _ = kill(pid, Signal::SIGTERM);
        let deadline = Instant::now() + STOP_GRACE;
        while entry.exit_state().is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if entry.exit_state().is_none() {
            tracing::info!("embedded: {} ignored SIGTERM for {:?}; killing", name, STOP_GRACE);
            let _ = kill(pid, Signal::SIGKILL);
            let deadline = Instant::now() + Duration::from_secs(5);
            while entry.exit_state().is_none() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    // 2. libcontainer state dir + cgroup. `delete(force)` does both; if the
    //    state is too broken to load, just remove the directory.
    let state_dir = ctx.state_path.join(&name);
    let _ = tokio::task::spawn_blocking(move || {
        match libcontainer::container::Container::load(state_dir.clone()) {
            Ok(mut c) => {
                if let Err(e) = c.delete(true) {
                    tracing::debug!("embedded: libcontainer delete: {:?}", e);
                    let _ = std::fs::remove_dir_all(&state_dir);
                }
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&state_dir);
            }
        }
    })
    .await;

    // 3. Network + IP.
    if let Some(net) = entry.pod_net {
        network::teardown_pod_network(net).await;
    }
    ctx.ipam.release(&name);

    // 4. Bundle and log. The overlay comes off first so remove_dir_all only
    //    ever sees the bundle's own upper/work dirs (or, rootless, a symlink
    //    it unlinks without descending into the image tree).
    let bundle_dir = ctx.bundle_root.join(&name);
    unmount_rootfs(&bundle_dir);
    let _ = std::fs::remove_dir_all(&bundle_dir);
    let _ = std::fs::remove_file(&entry.log.path);
    tracing::info!("embedded: {} torn down", name);
}

/// Give the container its own writable view of the image: an overlayfs whose
/// read-only lower layer is the shared, extracted image tree. Without it,
/// every container running the same image would write into one directory —
/// two nginx replicas would fight over `/run/nginx.pid`, and `rm -rf /` in
/// one pod would eat the image cache for all of them.
///
/// Rootless can't mount overlayfs (no CAP_SYS_ADMIN in the init userns), so
/// it falls back to a symlink straight at the shared tree. Same if the image
/// path contains characters overlayfs' option parser can't take.
fn mount_rootfs(bundle_dir: &Path, image_rootfs: &Path, rootless: bool) -> anyhow::Result<()> {
    let rootfs = bundle_dir.join("rootfs");
    // Leftovers from a previous run of this name: an overlay still mounted,
    // or the symlink from a rootless run.
    unmount_rootfs(bundle_dir);
    if rootfs.is_symlink() {
        std::fs::remove_file(&rootfs)?;
    } else if rootfs.is_dir() {
        // An earlier copied rootfs, or a bare mountpoint dir. Any overlay
        // was detached just above, so this never reaches the image tree.
        std::fs::remove_dir_all(&rootfs)?;
    }

    let image_str = image_rootfs.to_string_lossy();
    if rootless || image_str.contains([':', ',']) {
        if rootless {
            tracing::warn!(
                "embedded: rootless — {} shares the image tree read-write with every other container of that image",
                bundle_dir.display()
            );
        }
        std::os::unix::fs::symlink(image_rootfs, &rootfs)?;
        return Ok(());
    }

    // Fresh upper/work on every start: a restarted container is a new
    // container, same as Kubernetes.
    let upper = bundle_dir.join("upper");
    let work = bundle_dir.join("work");
    for d in [&upper, &work] {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d)?;
    }
    std::fs::create_dir_all(&rootfs)?;
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        image_str,
        upper.display(),
        work.display()
    );
    if let Err(e) = nix::mount::mount(
        Some("overlay"),
        &rootfs,
        Some("overlay"),
        nix::mount::MsFlags::empty(),
        Some(opts.as_str()),
    ) {
        // Overlayfs refuses an upperdir that itself sits on an overlay — the
        // case when superkube runs inside a Docker container without a
        // volume at /var/lib/superkube. Fall back to a private copy of the
        // image: slower to start and bigger on disk, but still isolated.
        tracing::warn!(
            "embedded: overlay rootfs at {} failed ({e}); copying the image instead — \
             mount a volume at {} to avoid this",
            rootfs.display(),
            STATE_ROOT
        );
        let _ = std::fs::remove_dir_all(&upper);
        let _ = std::fs::remove_dir_all(&work);
        copy_tree(image_rootfs, &rootfs)?;
    }
    Ok(())
}

/// `cp -a`: preserves mode, ownership, timestamps, symlinks, and hard links,
/// which std's copy helpers don't. coreutils is a given on any host the
/// embedded runtime can run on.
fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", from.display()))
        .arg(to)
        .status()
        .map_err(|e| anyhow::anyhow!("running cp: {e}"))?;
    if !status.success() {
        anyhow::bail!("cp -a {} {}: {status}", from.display(), to.display());
    }
    Ok(())
}

/// Lazy unmount so a straggler holding a file open can't make teardown
/// fail. EINVAL ("not a mount point") is the normal case and ignored.
fn unmount_rootfs(bundle_dir: &Path) {
    let _ = nix::mount::umount2(&bundle_dir.join("rootfs"), nix::mount::MntFlags::MNT_DETACH);
}

/// Mirrors libcontainer's `rootless_required` (utils.rs:233) — rootless is
/// required when the effective uid isn't root, or when we're already inside a
/// new user namespace (i.e. /proc/self/uid_map doesn't span the full u32).
fn detect_rootless() -> Option<RootlessConfig> {
    let euid = nix::unistd::geteuid().as_raw();
    let in_userns = std::fs::read_to_string("/proc/self/uid_map")
        .map(|s| !s.contains("4294967295"))
        .unwrap_or(false);
    if euid != 0 || in_userns {
        Some(RootlessConfig {
            host_uid: euid,
            host_gid: nix::unistd::getegid().as_raw(),
        })
    } else {
        None
    }
}

async fn init_host_network(gateway: std::net::Ipv4Addr) -> anyhow::Result<()> {
    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);
    network::ensure_bridge(&handle, gateway).await?;
    // /24 derived from the gateway: x.y.z.0/24
    let octets = gateway.octets();
    let cidr = format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2]);
    network::iptables::ensure_masquerade(&cidr)?;
    // MASQUERADE is useless without forwarding. Usually already on where
    // Docker has been; not inside a fresh container's netns.
    if let Err(e) = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1") {
        tracing::warn!("embedded: enabling ip_forward: {e}; pods won't reach outside");
    }
    Ok(())
}

/// Inside a container (kind-style) we're usually at the root of our own
/// cgroup namespace, and cgroup v2 refuses to delegate controllers to child
/// cgroups while the root still holds processes ("no internal processes").
/// Move whatever lives in the root into an `init` leaf first, the way kind's
/// entrypoint does. On a normal host we sit in some systemd slice, not the
/// root, and this is a no-op.
fn ensure_cgroup_root_is_empty() -> anyhow::Result<()> {
    let own = std::fs::read_to_string("/proc/self/cgroup")?;
    if !own.lines().any(|l| l.trim() == "0::/") {
        return Ok(());
    }
    let root = Path::new("/sys/fs/cgroup");
    let procs = std::fs::read_to_string(root.join("cgroup.procs"))?;
    let pids: Vec<&str> = procs.split_whitespace().collect();
    if pids.is_empty() {
        return Ok(());
    }
    let init = root.join("init");
    std::fs::create_dir_all(&init)?;
    for pid in &pids {
        // Kernel threads and already-exited pids refuse the move; harmless.
        let _ = std::fs::write(init.join("cgroup.procs"), pid);
    }
    tracing::info!(
        "embedded: moved {} root-cgroup processes into /sys/fs/cgroup/init",
        pids.len()
    );
    let controllers = std::fs::read_to_string(root.join("cgroup.controllers")).unwrap_or_default();
    let enable = controllers
        .split_whitespace()
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>()
        .join(" ");
    if !enable.is_empty() {
        if let Err(e) = std::fs::write(root.join("cgroup.subtree_control"), enable) {
            tracing::debug!("embedded: cgroup.subtree_control: {e}");
        }
    }
    Ok(())
}

// ============================================================================
// Logs
// ============================================================================

/// On-disk log for one container plus a live fan-out for followers.
///
/// Lines are stored as `<rfc3339 nanos> <stdout|stderr> <text>\n` — the same
/// shape as CRI log files — so `--timestamps` and `--since` are answered by
/// parsing our own prefix rather than guessing at the container's output.
struct ContainerLog {
    path: PathBuf,
    /// Append handle. Held while writing *and* while publishing to `live`,
    /// so a follower that reads the file and then subscribes under the same
    /// lock sees every line exactly once.
    file: std::sync::Mutex<std::fs::File>,
    live: broadcast::Sender<Bytes>,
    /// Flips to `true` once both pumps hit EOF — the container and everything
    /// that inherited its stdio has exited.
    closed: watch::Sender<bool>,
    open_pumps: AtomicUsize,
}

impl ContainerLog {
    fn create(path: PathBuf) -> anyhow::Result<Arc<Self>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("creating log {}: {}", path.display(), e))?;
        let (live, _) = broadcast::channel(LOG_FANOUT_CAPACITY);
        let (closed, _) = watch::channel(false);
        Ok(Arc::new(Self {
            path,
            file: std::sync::Mutex::new(file),
            live,
            closed,
            open_pumps: AtomicUsize::new(0),
        }))
    }

    fn append(&self, stream: &'static str, text: &[u8]) {
        let mut line = Vec::with_capacity(text.len() + 48);
        line.extend_from_slice(
            Utc::now()
                .to_rfc3339_opts(SecondsFormat::Nanos, true)
                .as_bytes(),
        );
        line.push(b' ');
        line.extend_from_slice(stream.as_bytes());
        line.push(b' ');
        line.extend_from_slice(text);
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        let _ = file.write_all(&line);
        // No receivers is the common case (nobody following) — not an error.
        let _ = self.live.send(Bytes::from(line));
    }

    /// Drain `fd` (the read end of init's stdout or stderr pipe) into the
    /// log, line by line, until EOF.
    fn pump(self: &Arc<Self>, fd: OwnedFd, stream: &'static str) {
        self.open_pumps.fetch_add(1, Ordering::SeqCst);
        let log = Arc::clone(self);
        tokio::spawn(async move {
            match set_nonblocking(&fd).and_then(|_| pipe::Receiver::from_owned_fd(fd)) {
                Ok(mut rx) => {
                    let mut partial: Vec<u8> = Vec::new();
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match rx.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                partial.extend_from_slice(&buf[..n]);
                                while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
                                    let line: Vec<u8> = partial.drain(..=pos).collect();
                                    log.append(stream, &line[..line.len() - 1]);
                                }
                            }
                            Err(e) => {
                                tracing::debug!("log pump {} {}: {}", log.path.display(), stream, e);
                                break;
                            }
                        }
                    }
                    if !partial.is_empty() {
                        log.append(stream, &partial);
                    }
                }
                Err(e) => tracing::warn!("log pump {} {}: {}", log.path.display(), stream, e),
            }
            if log.open_pumps.fetch_sub(1, Ordering::SeqCst) == 1 {
                let _ = log.closed.send(true);
            }
        });
    }

    /// Read the whole file and render it for kubectl. Taking the file lock
    /// here is what makes `snapshot_and_follow` exact.
    fn render(&self, options: &LogOptions) -> String {
        let _guard = self.file.lock().unwrap_or_else(|p| p.into_inner());
        let raw = std::fs::read_to_string(&self.path).unwrap_or_default();
        render_log(&raw, options)
    }

    /// Backlog + a live subscription + the closed flag, all taken under the
    /// file lock so nothing lands between the snapshot and the subscription.
    fn snapshot_and_follow(
        &self,
        options: &LogOptions,
    ) -> (String, broadcast::Receiver<Bytes>, watch::Receiver<bool>) {
        let _guard = self.file.lock().unwrap_or_else(|p| p.into_inner());
        let raw = std::fs::read_to_string(&self.path).unwrap_or_default();
        (
            render_log(&raw, options),
            self.live.subscribe(),
            self.closed.subscribe(),
        )
    }
}

/// Turn stored lines into what kubectl shows: filtered by `--since`, cut to
/// `--tail`, with our timestamp kept only for `--timestamps`.
fn render_log(raw: &str, options: &LogOptions) -> String {
    let mut kept: Vec<(&str, &str)> = Vec::new();
    for line in raw.lines() {
        let Some((ts, text)) = split_stored_line(line) else {
            continue;
        };
        if let Some(since) = options.since_time {
            if let Ok(t) = DateTime::parse_from_rfc3339(ts) {
                if t < since {
                    continue;
                }
            }
        }
        kept.push((ts, text));
    }
    if let Some(n) = options.tail_lines {
        let n = n.max(0) as usize;
        if kept.len() > n {
            kept.drain(..kept.len() - n);
        }
    }
    let mut out = String::new();
    for (ts, text) in kept {
        push_rendered(&mut out, ts, text, options.timestamps);
    }
    if let Some(limit) = options.limit_bytes {
        let mut cut = (limit.max(0) as usize).min(out.len());
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// `<ts> <stream> <text>` → `(ts, text)`. The stream tag is dropped: kubectl
/// shows stdout and stderr as one stream, like Docker does.
fn split_stored_line(line: &str) -> Option<(&str, &str)> {
    let (ts, rest) = line.split_once(' ')?;
    let text = rest.split_once(' ').map(|(_, t)| t).unwrap_or("");
    Some((ts, text))
}

fn push_rendered(out: &mut String, ts: &str, text: &str, timestamps: bool) {
    if timestamps {
        out.push_str(ts);
        out.push(' ');
    }
    out.push_str(text);
    out.push('\n');
}

fn render_live_line(line: &[u8], timestamps: bool) -> Option<String> {
    let s = std::str::from_utf8(line).ok()?;
    let (ts, text) = split_stored_line(s.trim_end_matches('\n'))?;
    let mut out = String::with_capacity(s.len());
    push_rendered(&mut out, ts, text, timestamps);
    Some(out)
}

fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ============================================================================
// Process lifetime helpers
// ============================================================================

/// Wait for `pid` on a blocking thread and record how it ended. Init is our
/// direct child (libcontainer clones it with `CLONE_PARENT`), so `waitpid`
/// both gives us the status and reaps the zombie.
fn spawn_reaper(
    name: String,
    pid: i32,
    exit: Arc<std::sync::Mutex<Option<Exit>>>,
    reaped: Arc<AtomicBool>,
) {
    tokio::task::spawn_blocking(move || {
        let code = wait_for_exit(pid, &name);
        reaped.store(true, Ordering::SeqCst);
        *exit.lock().unwrap_or_else(|p| p.into_inner()) = Some(Exit {
            code,
            at: Utc::now(),
        });
        tracing::info!("embedded: {} (pid {}) exited with code {}", name, pid, code);
    });
}

/// Blocking `waitpid`. Shell convention for signals (128 + signo). If the pid
/// turns out not to be our child, fall back to watching `/proc` so we still
/// notice the exit — the code is then unknown, reported as -1.
fn wait_for_exit(pid: i32, name: &str) -> i32 {
    loop {
        match waitpid(Pid::from_raw(pid), None) {
            Ok(WaitStatus::Exited(_, c)) => return c,
            Ok(WaitStatus::Signaled(_, sig, _)) => return 128 + sig as i32,
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                tracing::warn!("embedded: waitpid({}) for {}: {}; polling /proc", pid, name, e);
                let proc_dir = format!("/proc/{pid}");
                while Path::new(&proc_dir).exists() {
                    std::thread::sleep(Duration::from_secs(1));
                }
                return -1;
            }
        }
    }
}

/// Reap an exec'd process in the background. Returns the flag [`KillOnDrop`]
/// checks and the channel the agent reads the exit code from.
fn reap_exec(pid: Pid, label: String) -> (Arc<AtomicBool>, tokio::sync::oneshot::Receiver<i32>) {
    let reaped = Arc::new(AtomicBool::new(false));
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let flag = Arc::clone(&reaped);
    tokio::task::spawn_blocking(move || {
        let code = wait_for_exit(pid.as_raw(), &label);
        flag.store(true, Ordering::SeqCst);
        let _ = exit_tx.send(code);
    });
    (reaped, exit_rx)
}

/// Kills an exec'd process when the last reference goes away — i.e. when
/// kubectl hangs up on the session. Without this, `kubectl exec -- sleep
/// 1000` would outlive its WebSocket. `reaped` guards against signalling a
/// recycled pid after the process already exited.
struct KillOnDrop {
    pid: Pid,
    reaped: Arc<AtomicBool>,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if !self.reaped.load(Ordering::SeqCst) {
            let _ = kill(self.pid, Signal::SIGKILL);
        }
    }
}

/// Attach `guard` to a stream so it's dropped exactly when the stream is.
fn guarded<S>(stream: S, guard: KillOnDrop) -> LogStream
where
    S: futures::Stream<Item = anyhow::Result<Bytes>> + Send + 'static,
{
    Box::pin(stream.map(move |item| {
        let _keep_alive = &guard;
        item
    }))
}

// ============================================================================
// Exec plumbing
// ============================================================================

/// What a tenant inherits from the container: its env (so `PATH`, and
/// things like `NGINX_VERSION`, look the same as from inside) and its user.
struct TenantDefaults {
    env: HashMap<String, String>,
    uid: Option<u32>,
    gid: Option<u32>,
}

fn tenant_defaults(bundle_dir: &Path, tty: bool) -> anyhow::Result<TenantDefaults> {
    let spec = oci_spec::runtime::Spec::load(bundle_dir.join("config.json"))
        .map_err(|e| anyhow::anyhow!("loading bundle spec: {}", e))?;
    let process = spec.process().as_ref();
    let mut env: HashMap<String, String> = process
        .and_then(|p| p.env().as_ref())
        .map(|vars| {
            vars.iter()
                .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    if tty {
        env.entry("TERM".to_string()).or_insert_with(|| "xterm".to_string());
    }
    let (uid, gid) = process
        .map(|p| (Some(p.user().uid()), Some(p.user().gid())))
        .unwrap_or((None, None));
    Ok(TenantDefaults { env, uid, gid })
}

/// Allocate a pty from the *container's* devpts instance, from the host.
/// `/proc/<init>/root/dev/ptmx` resolves inside the container's mount
/// namespace, so the slave shows up as `/dev/pts/N` in there and `tty`,
/// `ttyname()` and friends behave as they do under Docker. (libcontainer's
/// own console-socket path opens the pty before it enters the mount
/// namespace, which leaves the slave invisible from inside.)
fn open_container_pty(init_pid: i32) -> anyhow::Result<(OwnedFd, OwnedFd)> {
    use std::os::unix::fs::OpenOptionsExt;
    let ptmx = format!("/proc/{init_pid}/root/dev/ptmx");
    let master = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(&ptmx)
        .map_err(|e| anyhow::anyhow!("opening {ptmx}: {e}"))?;
    let master = OwnedFd::from(master);
    let unlock: libc::c_int = 0;
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSPTLCK, &unlock) } < 0 {
        return Err(anyhow::anyhow!("TIOCSPTLCK: {}", std::io::Error::last_os_error()));
    }
    // TIOCGPTPEER (Linux 4.13+) opens the peer straight off the master, no
    // path lookup — the slave's path only means something inside the
    // container anyway.
    let slave = unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        return Err(anyhow::anyhow!("TIOCGPTPEER: {}", std::io::Error::last_os_error()));
    }
    Ok((master, unsafe { OwnedFd::from_raw_fd(slave) }))
}

/// libcontainer's default executor plus one step: if fd 0 is a terminal (a
/// TTY exec), make it the controlling terminal before exec. The init process
/// is a fresh session leader by then, so `TIOCSCTTY` succeeds, and the shell
/// gets job control and Ctrl-C like it does under Docker.
#[derive(Clone)]
struct CttyExecutor {
    inner: DefaultExecutor,
}

impl Executor for CttyExecutor {
    fn exec(&self, spec: &LcSpec) -> Result<(), ExecutorError> {
        if unsafe { libc::isatty(0) } == 1 && unsafe { libc::ioctl(0, libc::TIOCSCTTY, 0) } < 0 {
            tracing::debug!("TIOCSCTTY: {}", std::io::Error::last_os_error());
        }
        self.inner.exec(spec)
    }

    fn validate(&self, spec: &LcSpec) -> Result<(), ExecutorValidationError> {
        self.inner.validate(spec)
    }

    fn setup_envs(&self, envs: HashMap<String, String>) -> Result<(), ExecutorSetEnvsError> {
        self.inner.setup_envs(envs)
    }
}

/// A pty master as a tokio stream. Linux reports `EIO` on the master once
/// the slave side is gone (the process exited) — that's EOF for us.
struct PtyMaster {
    fd: AsyncFd<OwnedFd>,
}

impl PtyMaster {
    fn new(fd: OwnedFd) -> std::io::Result<Self> {
        set_nonblocking(&fd)?;
        Ok(Self { fd: AsyncFd::new(fd)? })
    }
}

impl AsyncRead for PtyMaster {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            let mut guard = futures::ready!(this.fd.poll_read_ready(cx))?;
            let dst = buf.initialize_unfilled();
            let res = guard.try_io(|inner| {
                let n = unsafe {
                    libc::read(
                        inner.get_ref().as_raw_fd(),
                        dst.as_mut_ptr() as *mut libc::c_void,
                        dst.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Poll::Ready(Ok(())),
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for PtyMaster {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        loop {
            let mut guard = futures::ready!(this.fd.poll_write_ready(cx))?;
            let res = guard.try_io(|inner| {
                let n = unsafe {
                    libc::write(
                        inner.get_ref().as_raw_fd(),
                        data.as_ptr() as *const libc::c_void,
                        data.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(r) => return Poll::Ready(r),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> anyhow::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(anyhow::anyhow!("TIOCSWINSZ: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[async_trait::async_trait]
impl Runtime for EmbeddedRuntime {
    fn name(&self) -> &'static str {
        "embedded"
    }

    async fn create_and_start_container(
        &mut self,
        name: &str,
        container: &PodContainer,
    ) -> anyhow::Result<String> {
        tracing::info!(
            "embedded: create+start container name={} image={}",
            name,
            container.image
        );
        let id = format!("{ID_PREFIX}{}", name);

        // A previous instance under this name — one that exited and is being
        // started again, or one still being torn down after a delete — must
        // be fully gone before we reuse its state dir, netns, and bundle.
        self.await_pending_teardown(name).await;
        let restart_count = {
            let previous = self.containers.lock().await.remove(&id);
            match previous {
                Some(prev) => {
                    let n = prev.restart_count + 1;
                    teardown(self.teardown_ctx(), prev).await;
                    n
                }
                None => 0,
            }
        };

        // 1. Pull image (cached if already on disk).
        let pulled = oci::image::pull(&container.image, &self.image_root).await?;

        // 2. Set up the pod network: allocate IP, create netns, veth into bridge.
        // If anything fails we still try to run the container but with no
        // connectivity — the IP assignment is rolled back in IPAM either way.
        let pod_ip = self.ipam.allocate(name)?;
        let gateway = self.ipam.gateway();
        let pod_net = match network::setup_pod_network(name, pod_ip, gateway).await {
            Ok(n) => Some(n),
            Err(e) => {
                tracing::warn!(
                    "embedded: network setup for {} failed ({e:#}); container will run without networking",
                    name
                );
                self.ipam.release(name);
                None
            }
        };
        let netns_path: Option<String> =
            pod_net.as_ref().map(|n| n.netns_path.display().to_string());

        // From here on a failure must give the network back.
        let started = self
            .build_and_start(name, container, &pulled, netns_path.as_deref())
            .await;
        let (pid, log) = match started {
            Ok(v) => v,
            Err(e) => {
                unmount_rootfs(&self.bundle_root.join(name));
                if let Some(net) = pod_net {
                    network::teardown_pod_network(net).await;
                }
                self.ipam.release(name);
                return Err(e);
            }
        };

        let exit = Arc::new(std::sync::Mutex::new(None));
        spawn_reaper(name.to_string(), pid, Arc::clone(&exit), Arc::new(AtomicBool::new(false)));

        let mut map = self.containers.lock().await;
        map.insert(
            id.clone(),
            ContainerEntry {
                name: name.to_string(),
                started_at: Utc::now(),
                image: container.image.clone(),
                pod_net,
                pid,
                restart_count,
                exit,
                log,
            },
        );

        tracing::info!("embedded: started {} as {} (pid {})", name, id, pid);
        Ok(id)
    }

    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>> {
        let map = self.containers.lock().await;
        let id = format!("{ID_PREFIX}{}", name);
        let entry = match map.get(&id) {
            Some(e) => e,
            None => return Ok(None),
        };

        // The reaper is the source of truth for "exited"; /proc covers the
        // instant between init dying and waitpid returning.
        let exit = entry.exit_state();
        let running = exit.is_none() && Path::new(&format!("/proc/{}", entry.pid)).exists();

        let ip = entry
            .pod_net
            .as_ref()
            .map(|n| n.pod_ip.to_string());

        Ok(Some(ContainerInfo {
            id: id.clone(),
            running,
            restart_count: entry.restart_count,
            started_at: Some(entry.started_at),
            exit_code: exit.map(|e| e.code),
            ip,
            // Nothing is published to host ports: the proxy dials the pod IP.
            port_mappings: Vec::<PortMapping>::new(),
        }))
    }

    async fn remove_container(&mut self, name: &str) -> anyhow::Result<()> {
        let id = format!("{ID_PREFIX}{}", name);
        let entry = self.containers.lock().await.remove(&id);
        if let Some(entry) = entry {
            tracing::info!("embedded: removing container {}", name);
            // Detached: the agent calls this under its state lock (which
            // every logs/exec request also takes), and the SIGTERM grace
            // period must not stall those.
            let handle = tokio::spawn(teardown(self.teardown_ctx(), entry));
            let mut pending = self.pending_teardowns.lock().await;
            pending.retain(|_, h| !h.is_finished());
            pending.insert(name.to_string(), handle);
        }
        Ok(())
    }

    async fn get_logs(&self, container_id: &str, options: &LogOptions) -> anyhow::Result<String> {
        let log = {
            let map = self.containers.lock().await;
            let entry = map
                .get(container_id)
                .ok_or_else(|| anyhow::anyhow!("no such container: {}", container_id))?;
            Arc::clone(&entry.log)
        };
        Ok(log.render(options))
    }

    async fn stream_logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> anyhow::Result<LogStream> {
        let log = {
            let map = self.containers.lock().await;
            let entry = map
                .get(container_id)
                .ok_or_else(|| anyhow::anyhow!("no such container: {}", container_id))?;
            Arc::clone(&entry.log)
        };
        let timestamps = options.timestamps;
        let (backlog, mut live, mut closed) = log.snapshot_and_follow(options);

        // A small forwarder task turns "broadcast until closed" into a plain
        // stream. Dropping the stream (kubectl hung up) drops `out`, the next
        // send fails, and the task ends.
        let (tx, out) = mpsc::channel::<anyhow::Result<Bytes>>(64);
        tokio::spawn(async move {
            if !backlog.is_empty() && tx.send(Ok(Bytes::from(backlog))).await.is_err() {
                return;
            }
            if *closed.borrow() {
                return;
            }
            loop {
                tokio::select! {
                    biased;
                    r = live.recv() => match r {
                        Ok(line) => {
                            if let Some(s) = render_live_line(&line, timestamps) {
                                if tx.send(Ok(Bytes::from(s))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!("log follower lagged {} lines", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    _ = closed.changed() => {
                        // Container exited: nothing more will be published,
                        // so drain what's buffered and end the stream.
                        while let Ok(line) = live.try_recv() {
                            if let Some(s) = render_live_line(&line, timestamps) {
                                if tx.send(Ok(Bytes::from(s))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        return;
                    }
                }
            }
        });
        Ok(Box::pin(ReceiverStream::new(out)))
    }

    async fn exec(
        &self,
        container_id: &str,
        cmd: Vec<String>,
        tty: bool,
    ) -> anyhow::Result<ExecSession> {
        let (name, init_pid) = {
            let map = self.containers.lock().await;
            let entry = map
                .get(container_id)
                .ok_or_else(|| anyhow::anyhow!("no such container: {}", container_id))?;
            if entry.exit_state().is_some() {
                anyhow::bail!("container {} is not running", entry.name);
            }
            (entry.name.clone(), entry.pid)
        };
        let defaults = tenant_defaults(&self.bundle_root.join(&name), tty)?;
        if tty {
            self.exec_tty(name, init_pid, cmd, defaults).await
        } else {
            self.exec_pipes(name, cmd, defaults).await
        }
    }
}

impl EmbeddedRuntime {
    /// Bundle + libcontainer start. Returns the init pid and the log the
    /// container's stdio is being captured into.
    async fn build_and_start(
        &self,
        name: &str,
        container: &PodContainer,
        pulled: &oci::image::PulledImage,
        netns_path: Option<&str>,
    ) -> anyhow::Result<(i32, Arc<ContainerLog>)> {
        // Bundle: <bundle_root>/<name>/{rootfs, upper, work, config.json}
        let bundle_dir = self.bundle_root.join(name);
        std::fs::create_dir_all(&bundle_dir)?;
        {
            // Blocking: the fallback path copies a whole image tree.
            let bundle_dir = bundle_dir.clone();
            let image_rootfs = pulled.rootfs.clone();
            let rootless = self.rootless.is_some();
            tokio::task::spawn_blocking(move || mount_rootfs(&bundle_dir, &image_rootfs, rootless))
                .await
                .map_err(|e| anyhow::anyhow!("spawn_blocking: {}", e))??;
        }
        oci::bundle::write_config(
            &bundle_dir,
            &oci::bundle::BundleInputs {
                hostname: name,
                rootfs_path: "rootfs",
                container,
                image: &pulled.config,
                netns_path,
                rootless: self.rootless,
            },
        )?;

        // Clean up any state dir left over from a previous failed run for
        // the same name — libcontainer rejects the bundle with `Exist` if
        // `<state_path>/<name>` already exists.
        let stale_state = self.state_path.join(name);
        if stale_state.exists() {
            tracing::warn!(
                "embedded: removing stale libcontainer state at {} from a prior run",
                stale_state.display()
            );
            if let Err(e) = std::fs::remove_dir_all(&stale_state) {
                tracing::warn!(
                    "embedded: failed to remove {}: {}; container start will likely fail with Exist",
                    stale_state.display(),
                    e
                );
            }
        }

        // Stdio: /dev/null in, pipes out. libcontainer dup2s these onto
        // 0/1/2 in the init process; the write ends it was handed are closed
        // on our side once the builder is consumed, so the read ends see EOF
        // exactly when the last process in the container lets go.
        let log = ContainerLog::create(self.log_root.join(format!("{name}.log")))?;
        let devnull = OwnedFd::from(std::fs::File::open("/dev/null")?);
        let (out_r, out_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        let (err_r, err_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        log.pump(out_r, "stdout");
        log.pump(err_r, "stderr");

        // Hand off to libcontainer on a blocking thread: it does synchronous
        // clone / pivot_root work we don't want on the tokio runtime.
        let bundle_owned = bundle_dir.clone();
        let state_owned = self.state_path.clone();
        let name_owned = name.to_string();
        let pid = tokio::task::spawn_blocking(move || -> anyhow::Result<i32> {
            use libcontainer::container::builder::ContainerBuilder;
            use libcontainer::syscall::syscall::SyscallType;

            let mut container = ContainerBuilder::new(name_owned.clone(), SyscallType::Linux)
                .with_root_path(state_owned)
                .map_err(|e| anyhow::anyhow!("with_root_path: {:?}", e))?
                .with_stdin(devnull)
                .with_stdout(out_w)
                .with_stderr(err_w)
                .as_init(&bundle_owned)
                .with_systemd(false)
                .build()
                .map_err(|e| anyhow::anyhow!("ContainerBuilder::build: {:?}", e))?;

            container
                .start()
                .map_err(|e| anyhow::anyhow!("container.start: {:?}", e))?;
            container
                .pid()
                .map(|p| p.as_raw())
                .ok_or_else(|| anyhow::anyhow!("libcontainer reported no init pid"))
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {}", e))??;

        Ok((pid, log))
    }

    /// Non-TTY exec: three pipes. stdout and stderr are merged into one
    /// output stream, as the Docker runtime does.
    async fn exec_pipes(
        &self,
        name: String,
        cmd: Vec<String>,
        defaults: TenantDefaults,
    ) -> anyhow::Result<ExecSession> {
        let (in_r, in_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        let (out_r, out_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        let (err_r, err_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;

        let state_path = self.state_path.clone();
        let tenant_name = name.clone();
        let pid = tokio::task::spawn_blocking(move || -> anyhow::Result<Pid> {
            use libcontainer::container::builder::ContainerBuilder;
            use libcontainer::syscall::syscall::SyscallType;

            ContainerBuilder::new(tenant_name, SyscallType::Linux)
                .with_root_path(state_path)
                .map_err(|e| anyhow::anyhow!("with_root_path: {:?}", e))?
                .with_stdin(in_r)
                .with_stdout(out_w)
                .with_stderr(err_w)
                .as_tenant()
                .with_container_args(cmd)
                .with_env(defaults.env)
                .with_user(defaults.uid)
                .with_group(defaults.gid)
                .with_detach(true)
                .build()
                .map_err(|e| anyhow::anyhow!("exec: {:?}", e))
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {}", e))??;

        let (reaped, exit_rx) = reap_exec(pid, format!("exec in {name}"));

        // Merge stdout + stderr into one channel; the stream ends when both
        // readers have seen EOF and dropped their sender.
        let (tx, rx) = mpsc::channel::<anyhow::Result<Bytes>>(64);
        for fd in [out_r, err_r] {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = match set_nonblocking(&fd).and_then(|_| pipe::Receiver::from_owned_fd(fd)) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(anyhow::anyhow!("exec pipe: {}", e))).await;
                        return;
                    }
                };
                let mut buf = vec![0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => return,
                        Ok(n) => {
                            if tx.send(Ok(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(anyhow::anyhow!("exec read: {}", e))).await;
                            return;
                        }
                    }
                }
            });
        }
        drop(tx);

        set_nonblocking(&in_w)?;
        let input = pipe::Sender::from_owned_fd(in_w)?;

        Ok(ExecSession {
            output: guarded(ReceiverStream::new(rx), KillOnDrop { pid, reaped }),
            input: Box::pin(input),
            resize: Box::new(|_, _| Box::pin(async { Ok(()) })),
            exit: Some(exit_rx),
        })
    }

    /// TTY exec: we allocate the pty ourselves from the container's own
    /// devpts and hand the tenant the slave as plain stdio; [`CttyExecutor`]
    /// makes it the controlling terminal right before exec. Read, write, and
    /// resize all go through the master we keep.
    async fn exec_tty(
        &self,
        name: String,
        init_pid: i32,
        cmd: Vec<String>,
        defaults: TenantDefaults,
    ) -> anyhow::Result<ExecSession> {
        let (master, slave) = open_container_pty(init_pid)?;
        let dup_slave = || -> anyhow::Result<OwnedFd> {
            let fd = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if fd < 0 {
                return Err(anyhow::anyhow!("dup pty slave: {}", std::io::Error::last_os_error()));
            }
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        };
        let (stdin, stdout, stderr) = (dup_slave()?, dup_slave()?, dup_slave()?);
        drop(slave);

        let state_path = self.state_path.clone();
        let tenant_name = name.clone();
        let pid = tokio::task::spawn_blocking(move || -> anyhow::Result<Pid> {
            use libcontainer::container::builder::ContainerBuilder;
            use libcontainer::syscall::syscall::SyscallType;

            ContainerBuilder::new(tenant_name, SyscallType::Linux)
                .with_root_path(state_path)
                .map_err(|e| anyhow::anyhow!("with_root_path: {:?}", e))?
                .with_executor(CttyExecutor {
                    inner: DefaultExecutor {},
                })
                .with_stdin(stdin)
                .with_stdout(stdout)
                .with_stderr(stderr)
                .as_tenant()
                .with_container_args(cmd)
                .with_env(defaults.env)
                .with_user(defaults.uid)
                .with_group(defaults.gid)
                .with_detach(true)
                .build()
                .map_err(|e| anyhow::anyhow!("exec (tty): {:?}", e))
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {}", e))??;

        let (reaped, exit_rx) = reap_exec(pid, format!("tty exec in {name}"));

        // The resize closure needs its own handle: dup the master so it
        // outlives the split halves.
        let resize_fd = unsafe { OwnedFd::from_raw_fd(nix::unistd::dup(master.as_raw_fd())?) };
        let resize_fd = Arc::new(resize_fd);
        let pty = PtyMaster::new(master)?;
        let (rd, wr) = tokio::io::split(pty);
        let output = ReaderStream::new(rd).map(|r| r.map_err(|e| anyhow::anyhow!("pty read: {}", e)));

        Ok(ExecSession {
            output: guarded(output, KillOnDrop { pid, reaped }),
            input: Box::pin(wr),
            resize: Box::new(move |h: u16, w: u16| {
                let fd = Arc::clone(&resize_fd);
                Box::pin(async move { set_winsize(fd.as_raw_fd(), h, w) })
            }),
            exit: Some(exit_rx),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> LogOptions {
        LogOptions::default()
    }

    #[test]
    fn render_strips_prefix_and_keeps_order() {
        let raw = "2026-01-01T00:00:00.000000000Z stdout hello\n\
                   2026-01-01T00:00:01.000000000Z stderr oops\n\
                   2026-01-01T00:00:02.000000000Z stdout \n";
        assert_eq!(render_log(raw, &opts()), "hello\noops\n\n");
    }

    #[test]
    fn render_timestamps_tail_since_limit() {
        let raw = "2026-01-01T00:00:00Z stdout a\n\
                   2026-01-01T00:00:01Z stdout b\n\
                   2026-01-01T00:00:02Z stdout c\n";
        let mut o = opts();
        o.timestamps = true;
        o.tail_lines = Some(2);
        assert_eq!(
            render_log(raw, &o),
            "2026-01-01T00:00:01Z b\n2026-01-01T00:00:02Z c\n"
        );

        let mut o = opts();
        o.since_time = Some("2026-01-01T00:00:01Z".parse().unwrap());
        assert_eq!(render_log(raw, &o), "b\nc\n");

        let mut o = opts();
        o.limit_bytes = Some(3);
        assert_eq!(render_log(raw, &o), "a\nb");
    }

    #[test]
    fn live_line_renders_like_file() {
        let line = b"2026-01-01T00:00:00Z stderr warn: x\n";
        assert_eq!(render_live_line(line, false).as_deref(), Some("warn: x\n"));
        assert_eq!(
            render_live_line(line, true).as_deref(),
            Some("2026-01-01T00:00:00Z warn: x\n")
        );
        assert_eq!(render_live_line(b"garbage", false), None);
    }
}
