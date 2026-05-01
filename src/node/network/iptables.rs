//! One-time iptables setup: MASQUERADE traffic leaving the bridge so pods can
//! reach the outside world via the host's default route.
//!
//! Idempotent — uses `-C` to check before `-I` to avoid duplicate rules. We
//! shell out rather than pull in a netfilter crate; the rule set is tiny and
//! `iptables` is universally available on Linux hosts.

use std::process::Command;

use anyhow::{Context, Result};

use super::BRIDGE_NAME;

/// Install MASQUERADE for `<pod_cidr>` egress + permissive forwarding from the
/// bridge. Safe to call repeatedly.
pub fn ensure_masquerade(pod_cidr: &str) -> Result<()> {
    tracing::info!("network: ensuring iptables MASQUERADE for pod CIDR {}", pod_cidr);
    install("nat", "POSTROUTING", &[
        "-s", pod_cidr,
        "!", "-o", BRIDGE_NAME,
        "-j", "MASQUERADE",
    ])?;
    install("filter", "FORWARD", &[
        "-i", BRIDGE_NAME,
        "-j", "ACCEPT",
    ])?;
    install("filter", "FORWARD", &[
        "-o", BRIDGE_NAME,
        "-j", "ACCEPT",
    ])?;
    Ok(())
}

fn install(table: &str, chain: &str, rule: &[&str]) -> Result<()> {
    // `-C` returns 0 if the rule already exists, non-zero otherwise.
    let mut check = Command::new("iptables");
    check.args(["-t", table, "-C", chain]).args(rule);
    if let Ok(status) = check.status() {
        if status.success() {
            return Ok(());
        }
    }
    tracing::info!("network: installing iptables -t {} -I {} {:?}", table, chain, rule);
    let mut add = Command::new("iptables");
    add.args(["-t", table, "-I", chain]).args(rule);
    let status = add
        .status()
        .with_context(|| format!("running iptables -t {table} -I {chain}"))?;
    if !status.success() {
        anyhow::bail!("iptables -t {table} -I {chain} failed (status {status})");
    }
    Ok(())
}
