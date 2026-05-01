//! Host-side bridge: created once, idempotent, brought up with the gateway IP
//! that pods will use as their default route.

use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use futures::TryStreamExt;
use rtnetlink::Handle;

use super::BRIDGE_NAME;

/// Create the bridge if it doesn't exist, assign `gateway/24`, set it up.
/// Returns the bridge's link index for plugging veths into.
pub async fn ensure_bridge(handle: &Handle, gateway: Ipv4Addr) -> Result<u32> {
    if let Some(idx) = link_index_by_name(handle, BRIDGE_NAME).await? {
        // Already exists — make sure it's up and has the gateway IP, then done.
        ensure_address(handle, idx, gateway, 24).await?;
        ensure_link_up(handle, idx).await?;
        return Ok(idx);
    }

    tracing::info!("network: creating bridge {} gateway={}", BRIDGE_NAME, gateway);
    handle
        .link()
        .add()
        .bridge(BRIDGE_NAME.to_string())
        .execute()
        .await
        .with_context(|| format!("creating bridge {BRIDGE_NAME}"))?;

    let idx = link_index_by_name(handle, BRIDGE_NAME)
        .await?
        .with_context(|| format!("bridge {BRIDGE_NAME} not found after add"))?;

    ensure_address(handle, idx, gateway, 24).await?;
    ensure_link_up(handle, idx).await?;
    tracing::info!("network: bridge {} ready (gateway={}, idx={})", BRIDGE_NAME, gateway, idx);
    Ok(idx)
}

pub(super) async fn link_index_by_name(handle: &Handle, name: &str) -> Result<Option<u32>> {
    let mut links = handle.link().get().match_name(name.to_string()).execute();
    if let Some(link) = links
        .try_next()
        .await
        .with_context(|| format!("link.get({name})"))?
    {
        return Ok(Some(link.header.index));
    }
    Ok(None)
}

async fn ensure_address(
    handle: &Handle,
    idx: u32,
    addr: Ipv4Addr,
    prefix_len: u8,
) -> Result<()> {
    // Best-effort: rtnetlink errors with EEXIST if the address is already there.
    let _ = handle
        .address()
        .add(idx, std::net::IpAddr::V4(addr), prefix_len)
        .execute()
        .await;
    Ok(())
}

async fn ensure_link_up(handle: &Handle, idx: u32) -> Result<()> {
    handle
        .link()
        .set(idx)
        .up()
        .execute()
        .await
        .context("setting link up")
}
