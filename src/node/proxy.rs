//! Service proxy — superkube's tiny userspace kube-proxy.
//!
//! Periodically fetches all NodePort services from the API. For each one,
//! ensures a TCP listener is running on `0.0.0.0:<nodePort>`. Connections that
//! land on that listener are forwarded to a backing pod's published host port
//! (which the agent records in `pod_ports` whenever it observes a container).
//!
//! This is single-host friendly: only pods running on *this* node can be
//! reached via the local proxy. In a multi-node cluster, every node would run
//! its own proxy and only handle its own pods — same as real kube-proxy in
//! userspace mode (modulo the cross-node forwarding it does).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration};

use crate::models::*;

use super::agent::AgentState;

const RECONCILE_INTERVAL_SECS: u64 = 5;

/// Internal: the API returns a generic List for our resources. We only need
/// the items, untyped.
#[derive(Deserialize)]
struct GenericList<T> {
    #[serde(default)]
    items: Vec<T>,
}

pub struct ServiceProxy {
    state: Arc<RwLock<AgentState>>,
    server_url: String,
    client: reqwest::Client,
    /// Active listeners keyed by nodePort.
    listeners: Mutex<HashMap<i32, JoinHandle<()>>>,
}

impl ServiceProxy {
    pub fn new(state: Arc<RwLock<AgentState>>, server_url: String) -> Arc<Self> {
        Arc::new(Self {
            state,
            server_url,
            client: reqwest::Client::new(),
            listeners: Mutex::new(HashMap::new()),
        })
    }

    pub async fn run(self: Arc<Self>) {
        let mut tick = interval(Duration::from_secs(RECONCILE_INTERVAL_SECS));
        loop {
            tick.tick().await;
            if let Err(e) = self.reconcile().await {
                tracing::debug!("proxy reconcile: {}", e);
            }
        }
    }

    async fn reconcile(&self) -> anyhow::Result<()> {
        let services = self.fetch_services().await?;
        let mut wanted: HashSet<i32> = HashSet::new();

        for svc in &services {
            if svc.spec.service_type != ServiceType::NodePort {
                continue;
            }
            let Some(node_port) = svc.spec.ports.first().and_then(|p| p.node_port) else {
                continue;
            };
            wanted.insert(node_port);

            let mut listeners = self.listeners.lock().await;
            if !listeners.contains_key(&node_port) {
                let svc = svc.clone();
                let state = self.state.clone();
                let server_url = self.server_url.clone();
                let client = self.client.clone();
                let handle = tokio::spawn(async move {
                    listen(node_port, svc, state, server_url, client).await;
                });
                listeners.insert(node_port, handle);
            }
        }

        // Stop listeners for ports that no longer correspond to a NodePort service.
        let mut listeners = self.listeners.lock().await;
        let stale: Vec<i32> = listeners
            .keys()
            .filter(|p| !wanted.contains(p))
            .copied()
            .collect();
        for p in stale {
            if let Some(h) = listeners.remove(&p) {
                h.abort();
                tracing::info!("proxy: stopped listening on :{}", p);
            }
        }
        Ok(())
    }

    async fn fetch_services(&self) -> anyhow::Result<Vec<Service>> {
        let url = format!("{}/api/v1/services", self.server_url);
        let resp = self.client.get(&url).send().await?;
        let list: GenericList<Service> = resp.json().await?;
        Ok(list.items)
    }
}

async fn listen(
    node_port: i32,
    service: Service,
    state: Arc<RwLock<AgentState>>,
    server_url: String,
    client: reqwest::Client,
) {
    let bind = format!("0.0.0.0:{}", node_port);
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("proxy: bind {}: {}", bind, e);
            return;
        }
    };
    tracing::info!(
        "proxy: listening on {} for service {}/{}",
        bind,
        service.metadata.namespace(),
        service.metadata.name()
    );

    loop {
        let (client_sock, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("proxy: accept on :{} failed: {}", node_port, e);
                continue;
            }
        };
        let svc = service.clone();
        let state = state.clone();
        let server_url = server_url.clone();
        let http = client.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(client_sock, &svc, &state, &server_url, &http).await {
                tracing::debug!("proxy {} conn from {}: {}", node_port, peer, e);
            }
        });
    }
}

async fn handle_conn(
    client: TcpStream,
    service: &Service,
    state: &Arc<RwLock<AgentState>>,
    server_url: &str,
    http: &reqwest::Client,
) -> anyhow::Result<()> {
    // Resolve targetPort. If targetPort is a name string we'd need the pod's
    // port spec to translate; for the first cut only handle int.
    let port_spec = service
        .spec
        .ports
        .first()
        .ok_or_else(|| anyhow::anyhow!("service has no ports"))?;
    let target_port = match &port_spec.target_port {
        Some(IntOrString::Int(n)) => *n,
        _ => port_spec.port,
    };

    // Find a backing pod via the service's endpoints.
    let url = format!(
        "{}/api/v1/namespaces/{}/endpoints/{}",
        server_url,
        service.metadata.namespace(),
        service.metadata.name()
    );
    let endpoints: Endpoints = http.get(&url).send().await?.json().await?;

    let candidates: Vec<(String, String)> = endpoints
        .subsets
        .iter()
        .flat_map(|s| s.addresses.iter().flatten())
        .filter_map(|addr| {
            let ns = addr.target_ref.as_ref()?.namespace.clone()?;
            let name = addr.target_ref.as_ref()?.name.clone()?;
            Some((ns, name))
        })
        .collect();

    if candidates.is_empty() {
        anyhow::bail!("no endpoints for service");
    }

    // Pick the first endpoint whose pod is local + has a published host port
    // for `target_port`.
    let agent_state = state.read().await;
    let host_port = candidates.iter().find_map(|(ns, pod)| {
        let mappings = agent_state.pod_ports.get(&(ns.clone(), pod.clone()))?;
        mappings
            .iter()
            .find(|m| m.container_port == target_port && m.protocol == "tcp")
            .map(|m| m.host_port)
    });
    drop(agent_state);

    let host_port = host_port.ok_or_else(|| {
        anyhow::anyhow!("no local backend with published port {}", target_port)
    })?;

    // Connect to localhost:<host_port> and bidi-pipe.
    let backend = TcpStream::connect(("127.0.0.1", host_port)).await?;
    let (mut cr, mut cw) = client.into_split();
    let (mut br, mut bw) = backend.into_split();

    let c2b = async move {
        let _ = tokio::io::copy(&mut cr, &mut bw).await;
        let _ = bw.shutdown().await;
    };
    let b2c = async move {
        let _ = tokio::io::copy(&mut br, &mut cw).await;
        let _ = cw.shutdown().await;
    };

    tokio::select! {
        _ = c2b => {},
        _ = b2c => {},
    }
    Ok(())
}
