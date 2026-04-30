//! Built-in mini-CNI for the embedded runtime.
//!
//! On Linux, gives each pod a real netns + veth pair plugged into a host-side
//! `superkube0` bridge, with a default route through the bridge and a one-time
//! MASQUERADE rule for egress. No `/opt/cni/bin` plugins, no daemon — just
//! netlink calls in-process plus a single `iptables` shell-out at startup.
//!
//! Cross-node pod-to-pod is out of scope (no overlay yet); same-host pod-to-pod
//! works through the bridge.
//!
//! Only the public types compile cross-platform; the implementations are
//! Linux-only.

#![cfg(target_os = "linux")]

mod bridge;
mod ipam;
pub mod iptables;
mod veth;

pub use bridge::ensure_bridge;
pub use ipam::Ipam;
pub use veth::{setup_pod_network, teardown_pod_network, PodNetwork};

/// Default name we give the host-side bridge that backs the pod network.
pub const BRIDGE_NAME: &str = "superkube0";
