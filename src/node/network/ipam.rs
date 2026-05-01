//! Tiny IPAM. Allocates /32 addresses out of a /24 carved from the pod CIDR.
//! `<prefix>.1` is the bridge gateway; pods get `<prefix>.2 .. <prefix>.254`.

use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::sync::Mutex;

use anyhow::{anyhow, Result};

pub struct Ipam {
    /// First three octets of the /24 we're allocating from (e.g. "10.244.0").
    prefix: String,
    /// Last octet of the gateway (always 1 for now).
    gateway_octet: u8,
    state: Mutex<IpamState>,
}

#[derive(Default)]
struct IpamState {
    /// Map from pod_name → assigned last octet.
    by_pod: HashMap<String, u8>,
    /// Last octets currently in use.
    taken: BTreeSet<u8>,
}

impl Ipam {
    /// Build IPAM from a /24-prefix string like "10.244.0".
    pub fn new(prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        let mut state = IpamState::default();
        state.taken.insert(1); // gateway
        Self {
            prefix,
            gateway_octet: 1,
            state: Mutex::new(state),
        }
    }

    pub fn gateway(&self) -> Ipv4Addr {
        format!("{}.{}", self.prefix, self.gateway_octet).parse().unwrap()
    }

    /// Allocate (or return existing) IP for a pod. Stable across calls so
    /// recovering an existing pod re-uses its IP.
    pub fn allocate(&self, pod_name: &str) -> Result<Ipv4Addr> {
        let mut state = self.state.lock().unwrap();
        if let Some(o) = state.by_pod.get(pod_name) {
            let ip = format!("{}.{}", self.prefix, *o).parse().unwrap();
            return Ok(ip);
        }
        // Find first free octet in 2..255.
        let octet = (2..=254u8)
            .find(|o| !state.taken.contains(o))
            .ok_or_else(|| anyhow!("pod CIDR {} /24 exhausted", self.prefix))?;
        state.taken.insert(octet);
        state.by_pod.insert(pod_name.to_string(), octet);
        let ip: Ipv4Addr = format!("{}.{}", self.prefix, octet).parse().unwrap();
        tracing::info!("ipam: allocated {} to pod {}", ip, pod_name);
        Ok(ip)
    }

    pub fn release(&self, pod_name: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(o) = state.by_pod.remove(pod_name) {
            state.taken.remove(&o);
            tracing::info!("ipam: released {}.{} from pod {}", self.prefix, o, pod_name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_then_releases() {
        let ipam = Ipam::new("10.244.0");
        assert_eq!(ipam.gateway(), Ipv4Addr::new(10, 244, 0, 1));
        let a = ipam.allocate("pod-a").unwrap();
        let b = ipam.allocate("pod-b").unwrap();
        assert_ne!(a, b);
        // Re-allocating the same pod returns the same IP (idempotent).
        assert_eq!(ipam.allocate("pod-a").unwrap(), a);
        ipam.release("pod-a");
        // After release, the next allocation can reuse the freed slot.
        let c = ipam.allocate("pod-c").unwrap();
        assert_eq!(c, a);
    }
}
