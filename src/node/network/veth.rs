//! Per-pod network plumbing.
//!
//! For each pod we:
//!   1. Create a persistent netns at `/var/run/netns/<pod_name>` (so libcontainer
//!      can join it via the OCI spec's `namespaces.path`).
//!   2. Create a veth pair: `vp<8 hex>` on the host, `eth0` inside the netns.
//!   3. Plug the host end into the `superkube0` bridge.
//!   4. Inside the netns: set `eth0` up with the pod IP/24, add a default route
//!      via the bridge gateway, bring `lo` up.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use futures::TryStreamExt;
use nix::sched::{setns, CloneFlags};
use rtnetlink::NetworkNamespace;

use super::bridge::link_index_by_name;
use super::BRIDGE_NAME;

pub struct PodNetwork {
    pub pod_name: String,
    /// Path to the netns we created — passed to libcontainer in the OCI spec.
    pub netns_path: PathBuf,
    pub pod_ip: Ipv4Addr,
    pub host_veth: String,
    pub container_veth: String,
}

/// Set up everything for one pod. Returns a handle the caller stores so it
/// can later tear down.
pub async fn setup_pod_network(
    pod_name: &str,
    pod_ip: Ipv4Addr,
    gateway: Ipv4Addr,
) -> Result<PodNetwork> {
    // Persistent netns at /var/run/netns/<pod_name>. NetworkNamespace::add
    // creates and bind-mounts it for us.
    NetworkNamespace::add(pod_name.to_string())
        .await
        .with_context(|| format!("creating netns {pod_name}"))?;
    let netns_path = PathBuf::from(format!("/var/run/netns/{pod_name}"));

    // Short, deterministic-ish host veth name (max 15 chars per kernel limit).
    let suffix: String = pod_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let host_veth = format!("vp{suffix}");
    let container_veth = "eth0".to_string();

    let (conn, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(conn);

    // Look up the bridge once — must already exist (caller called ensure_bridge first).
    let bridge_idx = link_index_by_name(&handle, BRIDGE_NAME)
        .await?
        .with_context(|| format!("bridge {BRIDGE_NAME} not found; ensure_bridge first"))?;

    // 1. Create the veth pair (peer side will be moved into the netns next).
    handle
        .link()
        .add()
        .veth(host_veth.clone(), container_veth.clone())
        .execute()
        .await
        .with_context(|| format!("creating veth pair {host_veth}<->{container_veth}"))?;

    // 2. Move the peer into the pod's netns. Open the netns fd and pass it.
    let peer_idx = link_index_by_name(&handle, &container_veth)
        .await?
        .with_context(|| format!("looking up freshly-created peer {container_veth}"))?;

    let netns_fd = std::fs::File::open(&netns_path)
        .with_context(|| format!("opening netns {}", netns_path.display()))?;
    use std::os::fd::AsRawFd;
    handle
        .link()
        .set(peer_idx)
        .setns_by_fd(netns_fd.as_raw_fd())
        .execute()
        .await
        .with_context(|| format!("moving {container_veth} into netns {pod_name}"))?;

    // 3. Plug the host end into the bridge + bring it up.
    let host_idx = link_index_by_name(&handle, &host_veth)
        .await?
        .with_context(|| format!("looking up host veth {host_veth}"))?;
    handle
        .link()
        .set(host_idx)
        .controller(bridge_idx)
        .execute()
        .await
        .context("attaching host veth to bridge")?;
    handle
        .link()
        .set(host_idx)
        .up()
        .execute()
        .await
        .context("bringing host veth up")?;

    // 4. Inside the netns: configure eth0 + lo + default route. We do this on
    // a blocking thread because `setns` is per-thread; bouncing back is messy
    // and we don't need to share the rtnetlink handle.
    let pod_name_owned = pod_name.to_string();
    let netns_path_owned = netns_path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        configure_inside_netns(&pod_name_owned, &netns_path_owned, pod_ip, gateway)
    })
    .await
    .context("spawn_blocking for netns configuration")??;

    Ok(PodNetwork {
        pod_name: pod_name.to_string(),
        netns_path,
        pod_ip,
        host_veth,
        container_veth,
    })
}

/// Reverse of `setup_pod_network`: best-effort cleanup. Errors are logged but
/// not propagated — by the time we tear down, the netns or veth may already be
/// gone (e.g. the kernel reaped them when the container init exited).
pub async fn teardown_pod_network(net: PodNetwork) {
    let (conn, handle, _) = match rtnetlink::new_connection() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("teardown: rtnetlink: {}", e);
            return;
        }
    };
    tokio::spawn(conn);

    if let Ok(Some(idx)) = link_index_by_name(&handle, &net.host_veth).await {
        let _ = handle.link().del(idx).execute().await;
    }
    let _ = NetworkNamespace::del(net.pod_name.clone()).await;
}

/// Run on a blocking thread: enter the netns, drive a fresh rtnetlink runtime
/// inside it, configure interfaces, exit.
fn configure_inside_netns(
    pod_name: &str,
    netns_path: &std::path::Path,
    pod_ip: Ipv4Addr,
    gateway: Ipv4Addr,
) -> Result<()> {
    let netns_fd = std::fs::File::open(netns_path)
        .with_context(|| format!("opening netns {}", netns_path.display()))?;

    // Switch this thread into the pod netns. From here on, all socket / netlink
    // operations apply to that namespace. nix ≥0.29 takes `AsFd` here, so the
    // File is borrowed for the duration of the call and dropped at end of scope.
    setns(&netns_fd, CloneFlags::CLONE_NEWNET)
        .with_context(|| format!("setns into {pod_name}"))?;

    // Spin up a per-task rtnetlink runtime tied to *this* (now switched) thread.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("rt for netns setup")?;
    rt.block_on(async move {
        let (conn, handle, _) = rtnetlink::new_connection()?;
        tokio::spawn(conn);

        let lo_idx = link_index_by_name(&handle, "lo")
            .await?
            .ok_or_else(|| anyhow::anyhow!("lo not present in netns"))?;
        handle.link().set(lo_idx).up().execute().await?;

        let eth_idx = link_index_by_name(&handle, "eth0")
            .await?
            .ok_or_else(|| anyhow::anyhow!("eth0 not present in netns"))?;
        handle
            .address()
            .add(eth_idx, std::net::IpAddr::V4(pod_ip), 24)
            .execute()
            .await?;
        handle.link().set(eth_idx).up().execute().await?;

        // Default route via the bridge gateway.
        handle
            .route()
            .add()
            .v4()
            .gateway(gateway)
            .output_interface(eth_idx)
            .execute()
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}
