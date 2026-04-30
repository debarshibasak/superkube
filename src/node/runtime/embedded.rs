//! Linux-only embedded OCI runtime.
//!
//! Pulls images via [`crate::node::oci::image`], builds an OCI bundle via
//! [`crate::node::oci::bundle`], and hands it to youki's `libcontainer` to
//! create namespaces, set up cgroups v2, pivot_root, and exec the container's
//! init process. All in-process — no host containerd / runc / docker daemon.
//!
//! # What this skeleton does
//!
//! * Image pull → bundle build → libcontainer `start` for `create_and_start_container`.
//! * `find_container` queries libcontainer state on disk by name.
//!
//! # What's TODO
//!
//! * **Networking.** Pods get no veth / bridge yet — the container shares its
//!   own netns but has no connectivity. Need a small CNI-like layer:
//!   `rtnetlink` to create a `superkube0` bridge and per-pod veth pairs;
//!   `nftables` for NodePort forwarding (or piggyback the existing proxy).
//! * **Log capture.** libcontainer doesn't redirect stdout/stderr to a file
//!   for us — we need to wire a "console socket" or override the init pipe.
//!   Until that lands, `get_logs` returns a placeholder string.
//! * **Exec.** Same plumbing as logs; libcontainer has `exec` support but
//!   needs us to wire its IO end-to-end the way the Docker runtime does.
//! * **Resource limits, seccomp profiles, AppArmor.** All achievable through
//!   the OCI runtime spec; the bundle generator just needs to emit the
//!   right sections.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::models::Container as PodContainer;
use crate::node::oci;

use super::{ContainerInfo, ExecSession, LogOptions, LogStream, PortMapping, Runtime};

const STATE_ROOT: &str = "/var/lib/superkube";
const ID_PREFIX: &str = "embedded://";

pub struct EmbeddedRuntime {
    /// Where libcontainer stashes its per-container state directories.
    state_path: PathBuf,
    /// Where image rootfs trees live.
    image_root: PathBuf,
    /// Where we write bundle/<name>/{rootfs, config.json} for each container.
    bundle_root: PathBuf,
    containers: Arc<Mutex<HashMap<String, ContainerEntry>>>,
}

struct ContainerEntry {
    name: String,
    started_at: DateTime<Utc>,
    image: String,
}

impl EmbeddedRuntime {
    pub async fn new() -> anyhow::Result<Self> {
        let state_path = PathBuf::from(STATE_ROOT).join("state");
        let image_root = PathBuf::from(STATE_ROOT).join("images");
        let bundle_root = PathBuf::from(STATE_ROOT).join("bundles");
        for d in [&state_path, &image_root, &bundle_root] {
            std::fs::create_dir_all(d).map_err(|e| {
                anyhow::anyhow!("creating {}: {}", d.display(), e)
            })?;
        }

        // Sanity-check libcontainer's runtime expectations early so the agent
        // doesn't spawn a broken runtime mid-pod-creation.
        if !std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            tracing::warn!(
                "embedded runtime: cgroups v2 not detected (/sys/fs/cgroup/cgroup.controllers \
                 missing). Containers may fail to start."
            );
        }

        Ok(Self {
            state_path,
            image_root,
            bundle_root,
            containers: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn strip(id: &str) -> &str {
        id.strip_prefix(ID_PREFIX).unwrap_or(id)
    }
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
        // 1. Pull image (cached if already on disk).
        let pulled = oci::image::pull(&container.image, &self.image_root).await?;

        // 2. Build bundle: <bundle_root>/<name>/{rootfs -> pulled.rootfs, config.json}
        let bundle_dir = self.bundle_root.join(name);
        std::fs::create_dir_all(&bundle_dir)?;
        let rootfs_link = bundle_dir.join("rootfs");
        // Symlink rootfs in. Keeps bundles cheap; when the same image backs
        // multiple containers we don't duplicate the layers.
        if rootfs_link.exists() || rootfs_link.is_symlink() {
            std::fs::remove_file(&rootfs_link).ok();
        }
        std::os::unix::fs::symlink(&pulled.rootfs, &rootfs_link)?;
        oci::bundle::write_config(
            &bundle_dir,
            &oci::bundle::BundleInputs {
                hostname: name,
                rootfs_path: "rootfs",
                container,
                image: &pulled.config,
            },
        )?;

        // 3. Hand off to libcontainer. The exact builder API has shifted
        // across libcontainer releases; the call below targets the 0.5 series.
        // We `spawn_blocking` because libcontainer does synchronous
        // pivot_root / clone3 work that we don't want on the tokio runtime.
        let bundle_owned = bundle_dir.clone();
        let state_owned = self.state_path.clone();
        let name_owned = name.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            use libcontainer::container::builder::ContainerBuilder;
            use libcontainer::syscall::syscall::SyscallType;

            let mut container = ContainerBuilder::new(name_owned.clone(), SyscallType::Linux)
                .with_root_path(state_owned)
                .map_err(|e| anyhow::anyhow!("with_root_path: {:?}", e))?
                .as_init(&bundle_owned)
                .with_systemd(false)
                .build()
                .map_err(|e| anyhow::anyhow!("ContainerBuilder::build: {:?}", e))?;

            container
                .start()
                .map_err(|e| anyhow::anyhow!("container.start: {:?}", e))?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {}", e))??;

        let id = format!("{ID_PREFIX}{}", name);
        let mut map = self.containers.lock().await;
        map.insert(
            id.clone(),
            ContainerEntry {
                name: name.to_string(),
                started_at: Utc::now(),
                image: container.image.clone(),
            },
        );

        tracing::info!("embedded: started {} as {}", name, id);
        Ok(id)
    }

    async fn is_container_running(&self, id: &str) -> anyhow::Result<bool> {
        let info = self.find_container(Self::strip(id)).await?;
        Ok(info.map(|i| i.running).unwrap_or(false))
    }

    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>> {
        let map = self.containers.lock().await;
        let id = format!("{ID_PREFIX}{}", name);
        let entry = match map.get(&id) {
            Some(e) => e,
            None => return Ok(None),
        };

        // Ask libcontainer if the container is still alive. The state dir
        // layout is `<state_path>/<name>/`.
        let state_dir = self.state_path.join(&entry.name);
        let running = match libcontainer::container::Container::load(state_dir) {
            Ok(c) => matches!(c.status(), libcontainer::container::ContainerStatus::Running),
            Err(_) => false,
        };

        Ok(Some(ContainerInfo {
            id: id.clone(),
            running,
            restart_count: 0,
            started_at: Some(entry.started_at),
            exit_code: None,
            // No port publishing yet — networking is a TODO.
            port_mappings: Vec::<PortMapping>::new(),
        }))
    }

    async fn get_logs(&self, _container_id: &str, _options: &LogOptions) -> anyhow::Result<String> {
        // libcontainer doesn't capture stdout/stderr for us by default. Until
        // we wire a console socket / pipe, surface a placeholder so users
        // know logs aren't lost forever — they're just not collected.
        Ok("[embedded runtime: log capture not wired yet — TODO console socket]\n".to_string())
    }

    async fn stream_logs(
        &self,
        _container_id: &str,
        _options: &LogOptions,
    ) -> anyhow::Result<LogStream> {
        let msg = b"[embedded runtime: streaming logs not wired yet]\n";
        let bytes = Bytes::from_static(msg);
        Ok(Box::pin(futures::stream::iter([Ok(bytes)])))
    }

    async fn exec(
        &self,
        _container_id: &str,
        _cmd: Vec<String>,
        _tty: bool,
    ) -> anyhow::Result<ExecSession> {
        anyhow::bail!(
            "embedded runtime: exec not wired yet (libcontainer.exec needs IO plumbing)"
        )
    }
}
