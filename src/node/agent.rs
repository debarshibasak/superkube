use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use uuid::Uuid;

use crate::models::*;

use super::runtime::{self, LogOptions, PortMapping, Runtime};

/// Node agent HTTP server port
const NODE_AGENT_PORT: u16 = 10250;

/// Run the node agent. `labels` are attached to the registered Node object —
/// pass `node-role.kubernetes.io/control-plane=""` for the embedded server-side
/// agent so kubectl shows it under the `control-plane` role.
pub async fn run(
    name: &str,
    server_url: &str,
    containerd_socket: &str,
    labels: HashMap<String, String>,
) -> anyhow::Result<()> {
    run_with_pod_cidr(name, server_url, containerd_socket, labels, "10.244.0.0/16").await
}

/// Like [`run`], but lets the caller pin the pod CIDR. The agent uses the /24
/// prefix to allocate pod IPs.
pub async fn run_with_pod_cidr(
    name: &str,
    server_url: &str,
    containerd_socket: &str,
    labels: HashMap<String, String>,
    pod_cidr: &str,
) -> anyhow::Result<()> {
    run_full(name, server_url, containerd_socket, labels, pod_cidr, "auto").await
}

/// Most-flexible entry point: also accepts a runtime selector string
/// (`auto`/`docker`/`embedded`/`mock`) to back `--runtime=` on the CLI.
pub async fn run_full(
    name: &str,
    server_url: &str,
    containerd_socket: &str,
    labels: HashMap<String, String>,
    pod_cidr: &str,
    runtime_kind: &str,
) -> anyhow::Result<()> {
    let agent = NodeAgent::new_with_runtime(
        name,
        server_url,
        containerd_socket,
        labels,
        pod_cidr.to_string(),
        runtime_kind,
    )
    .await?;
    agent.run().await
}

/// Shared state for the node agent
pub(super) struct AgentState {
    pub name: String,
    pub labels: HashMap<String, String>,
    pub server_url: String,
    pub client: Client,
    pub runtime: Box<dyn Runtime>,
    /// Pod CIDR — first three octets become the /24 prefix used to assemble
    /// pod IPs from the pod UID.
    pub pod_cidr_prefix: String,
    /// Map of pod UID to container ID
    pub containers: HashMap<Uuid, String>,
    /// Map of (namespace, pod_name, container_name) to container ID for log lookup
    pub pod_containers: HashMap<(String, String, String), String>,
    /// Map of (namespace, pod_name) → published port mappings from the runtime.
    /// Read by the NodePort proxy to find a local backend host port for an
    /// endpoint pod hosted on this node.
    pub pod_ports: HashMap<(String, String), Vec<PortMapping>>,
}

/// Node agent that manages containers on this node
struct NodeAgent {
    state: Arc<RwLock<AgentState>>,
}

impl NodeAgent {
    async fn new(
        name: &str,
        server_url: &str,
        containerd_socket: &str,
        labels: HashMap<String, String>,
        pod_cidr: String,
    ) -> anyhow::Result<Self> {
        Self::new_with_runtime(
            name,
            server_url,
            containerd_socket,
            labels,
            pod_cidr,
            "auto",
        )
        .await
    }

    async fn new_with_runtime(
        name: &str,
        server_url: &str,
        containerd_socket: &str,
        labels: HashMap<String, String>,
        pod_cidr: String,
        runtime_kind: &str,
    ) -> anyhow::Result<Self> {
        let runtime = runtime::select(containerd_socket, runtime_kind).await?;
        tracing::info!("node agent runtime: {}", runtime.name());

        let pod_cidr_prefix = pod_cidr_to_prefix(&pod_cidr);
        tracing::info!("pod CIDR: {} (assigning IPs as {}.X)", pod_cidr, pod_cidr_prefix);

        let state = AgentState {
            name: name.to_string(),
            labels,
            server_url: server_url.trim_end_matches('/').to_string(),
            client: Client::new(),
            runtime,
            pod_cidr_prefix,
            containers: HashMap::new(),
            pod_containers: HashMap::new(),
            pod_ports: HashMap::new(),
        };

        Ok(Self {
            state: Arc::new(RwLock::new(state)),
        })
    }

    /// Run the node agent main loop
    async fn run(self) -> anyhow::Result<()> {
        // Register node with server
        self.register_node().await?;

        // Start the HTTP server for log requests
        let http_state = self.state.clone();
        let http_handle = tokio::spawn(async move {
            if let Err(e) = run_http_server(http_state).await {
                tracing::error!("HTTP server error: {}", e);
            }
        });

        // Start the NodePort service proxy.
        let server_url = self.state.read().await.server_url.clone();
        let proxy = super::proxy::ServiceProxy::new(self.state.clone(), server_url);
        let proxy_handle = tokio::spawn(async move {
            proxy.run().await;
        });
        let _ = proxy_handle;

        // Start heartbeat and pod sync loops
        let mut heartbeat_interval = interval(Duration::from_secs(10));
        let mut sync_interval = interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                _ = heartbeat_interval.tick() => {
                    if let Err(e) = self.send_heartbeat().await {
                        tracing::error!("Heartbeat failed: {}", e);
                    }
                }
                _ = sync_interval.tick() => {
                    if let Err(e) = self.sync_pods().await {
                        tracing::error!("Pod sync failed: {}", e);
                    }
                }
            }
        }
    }

    /// Register this node with the API server
    async fn register_node(&self) -> anyhow::Result<()> {
        let state = self.state.read().await;
        tracing::info!("Registering node {} with server", state.name);

        let node = build_node_object(&state.name, &state.labels);

        let response = state
            .client
            .post(format!("{}/api/v1/nodes", state.server_url))
            .json(&node)
            .send()
            .await?;

        if response.status().is_success() {
            tracing::info!("Node {} registered successfully", state.name);
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::warn!("Node registration returned {}: {}", status, body);
        }

        Ok(())
    }

    /// Send heartbeat to update node status
    async fn send_heartbeat(&self) -> anyhow::Result<()> {
        let state = self.state.read().await;
        let node = build_node_object(&state.name, &state.labels);

        let response = state
            .client
            .put(format!("{}/api/v1/nodes/{}", state.server_url, state.name))
            .json(&node)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Heartbeat failed with {}: {}", status, body);
        }

        tracing::debug!("Heartbeat sent for node {}", state.name);
        Ok(())
    }

    /// Sync pods assigned to this node
    async fn sync_pods(&self) -> anyhow::Result<()> {
        let (server_url, name) = {
            let state = self.state.read().await;
            (state.server_url.clone(), state.name.clone())
        };

        // Get pods assigned to this node
        let client = Client::new();
        let response = client
            .get(format!(
                "{}/api/v1/pods?fieldSelector=spec.nodeName={}",
                server_url, name
            ))
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch pods: {}", response.status());
        }

        let pod_list: List<Pod> = response.json().await?;

        for pod in pod_list.items {
            // Check if pod is assigned to this node
            if pod.spec.node_name.as_deref() != Some(&name) {
                continue;
            }

            if let Err(e) = self.reconcile_pod(&pod).await {
                tracing::error!(
                    "Failed to reconcile pod {}/{}: {}",
                    pod.metadata.namespace(),
                    pod.metadata.name(),
                    e
                );
            }
        }

        Ok(())
    }

    /// Reconcile a single pod — observe runtime state, create missing
    /// containers, then publish a fresh `PodStatus` (including
    /// `containerStatuses`, which is what kubectl uses for the READY column).
    async fn reconcile_pod(&self, pod: &Pod) -> anyhow::Result<()> {
        let pod_uid = pod
            .metadata
            .uid
            .ok_or_else(|| anyhow::anyhow!("Pod has no UID"))?;
        let namespace = pod.metadata.namespace().to_string();
        let pod_name = pod.metadata.name().to_string();

        if pod.spec.containers.is_empty() {
            return Ok(());
        }

        let mut container_statuses: Vec<ContainerStatus> = Vec::new();
        let mut all_running = true;
        let mut any_failed = false;
        let mut earliest_start: Option<chrono::DateTime<Utc>> = None;

        // Observe + act per container.
        for container in &pod.spec.containers {
            let docker_name = format!("{}-{}", pod_name, container.name);

            // Observe.
            let mut state = self.state.write().await;
            let observed = state.runtime.find_container(&docker_name).await?;

            // Act if missing.
            let info = match observed {
                Some(info) => info,
                None => {
                    tracing::info!(
                        "pod {}/{}: starting container {}",
                        namespace,
                        pod_name,
                        container.name
                    );
                    drop(state);
                    self.emit_event(
                        pod,
                        EventType::Normal,
                        "Pulling",
                        &format!("Pulling image \"{}\"", container.image),
                    )
                    .await;
                    let mut state = self.state.write().await;
                    match state
                        .runtime
                        .create_and_start_container(&docker_name, container)
                        .await
                    {
                        Ok(_id) => {
                            let info = state
                                .runtime
                                .find_container(&docker_name)
                                .await?
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "created container {docker_name} but cannot find it"
                                    )
                                })?;
                            drop(state);
                            self.emit_event(
                                pod,
                                EventType::Normal,
                                "Pulled",
                                &format!("Successfully pulled image \"{}\"", container.image),
                            )
                            .await;
                            self.emit_event(
                                pod,
                                EventType::Normal,
                                "Created",
                                &format!("Created container {}", container.name),
                            )
                            .await;
                            self.emit_event(
                                pod,
                                EventType::Normal,
                                "Started",
                                &format!("Started container {}", container.name),
                            )
                            .await;
                            // Re-acquire the write lock so the surrounding code
                            // (state.containers / pod_containers maps) keeps
                            // working as before.
                            let mut state = self.state.write().await;
                            state.containers.insert(pod_uid, info.id.clone());
                            state.pod_containers.insert(
                                (
                                    namespace.clone(),
                                    pod_name.clone(),
                                    container.name.clone(),
                                ),
                                info.id.clone(),
                            );
                            if !info.port_mappings.is_empty() {
                                state
                                    .pod_ports
                                    .entry((namespace.clone(), pod_name.clone()))
                                    .or_default()
                                    .extend(info.port_mappings.iter().cloned());
                            }
                            drop(state);
                            // info is needed below for status reporting; clone
                            // out of the matched arm.
                            // Use a sentinel: build the ContainerStatus inline
                            // here rather than fall through.
                            if info.running {
                                if let Some(t) = info.started_at {
                                    earliest_start =
                                        Some(earliest_start.map_or(t, |cur| cur.min(t)));
                                }
                            } else {
                                all_running = false;
                                if info.exit_code.unwrap_or(0) != 0 {
                                    any_failed = true;
                                }
                            }
                            container_statuses.push(ContainerStatus {
                                name: container.name.clone(),
                                ready: info.running,
                                restart_count: info.restart_count,
                                image: container.image.clone(),
                                image_id: container.image.clone(),
                                container_id: Some(info.id.clone()),
                                state: Some(container_state(&info)),
                                last_state: None,
                                started: Some(info.running),
                            });
                            continue;
                        }
                        Err(e) => {
                            tracing::error!(
                                "pod {}/{}: failed to start {}: {}",
                                namespace,
                                pod_name,
                                container.name,
                                e
                            );
                            drop(state);
                            self.emit_event(
                                pod,
                                EventType::Warning,
                                "Failed",
                                &format!(
                                    "Failed to start container {}: {}",
                                    container.name, e
                                ),
                            )
                            .await;
                            any_failed = true;
                            container_statuses.push(failed_container_status(container, &e));
                            continue;
                        }
                    }
                }
            };

            // Maintain in-memory indexes for log/exec endpoints + the
            // NodePort proxy.
            state.containers.insert(pod_uid, info.id.clone());
            state.pod_containers.insert(
                (namespace.clone(), pod_name.clone(), container.name.clone()),
                info.id.clone(),
            );
            if !info.port_mappings.is_empty() {
                state
                    .pod_ports
                    .entry((namespace.clone(), pod_name.clone()))
                    .or_default()
                    .extend(info.port_mappings.iter().cloned());
            }
            drop(state);

            if !info.running {
                all_running = false;
                if info.exit_code.unwrap_or(0) != 0 {
                    any_failed = true;
                }
            }

            if let Some(t) = info.started_at {
                earliest_start = Some(earliest_start.map_or(t, |cur| cur.min(t)));
            }

            container_statuses.push(ContainerStatus {
                name: container.name.clone(),
                ready: info.running,
                restart_count: info.restart_count,
                image: container.image.clone(),
                image_id: container.image.clone(),
                container_id: Some(info.id.clone()),
                state: Some(container_state(&info)),
                last_state: None,
                started: Some(info.running),
            });
        }

        let phase = if any_failed {
            PodPhase::Failed
        } else if all_running && !container_statuses.is_empty() {
            PodPhase::Running
        } else {
            PodPhase::Pending
        };

        let pod_ip = {
            let s = self.state.read().await;
            format!("{}.{}", s.pod_cidr_prefix, (pod_uid.as_u128() % 254 + 1) as u8)
        };

        self.publish_pod_status(
            &namespace,
            &pod_name,
            phase,
            Some(pod_ip),
            container_statuses,
            earliest_start,
        )
        .await?;

        Ok(())
    }

    /// Fire-and-forget event POST to the server. Failures are logged at debug —
    /// events are observability, never something that should fail a reconcile.
    async fn emit_event(
        &self,
        pod: &Pod,
        typ: EventType,
        reason: &str,
        message: &str,
    ) {
        let state = self.state.read().await;
        let namespace = pod.metadata.namespace().to_string();
        let pod_name = pod.metadata.name().to_string();
        let now = Utc::now();

        let event_name = format!(
            "{}.{:x}",
            pod_name,
            Uuid::new_v4().as_u128() & 0xFFFFFFFF
        );

        let event = Event {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Event".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(event_name),
                namespace: Some(namespace.clone()),
                uid: Some(Uuid::new_v4()),
                ..Default::default()
            },
            involved_object: ObjectReference {
                api_version: Some("v1".to_string()),
                kind: Some("Pod".to_string()),
                name: Some(pod_name),
                namespace: Some(namespace.clone()),
                uid: pod.metadata.uid,
                resource_version: None,
                field_path: None,
            },
            reason: Some(reason.to_string()),
            message: Some(message.to_string()),
            source: Some(EventSource {
                component: Some("kubelet".to_string()),
                host: Some(state.name.clone()),
            }),
            first_timestamp: Some(now),
            last_timestamp: Some(now),
            event_time: Some(now),
            count: Some(1),
            event_type: Some(typ),
            action: None,
            reporting_controller: Some("superkube/kubelet".to_string()),
            reporting_instance: Some(state.name.clone()),
        };

        let url = format!(
            "{}/api/v1/namespaces/{}/events",
            state.server_url, namespace
        );
        let result = state.client.post(&url).json(&event).send().await;
        if let Err(e) = result {
            tracing::debug!("emit_event POST failed: {}", e);
        }
    }

    /// Push a fresh PodStatus to the server. The status carries the full
    /// containerStatuses array so kubectl can render READY / RESTARTS columns.
    async fn publish_pod_status(
        &self,
        namespace: &str,
        name: &str,
        phase: PodPhase,
        pod_ip: Option<String>,
        container_statuses: Vec<ContainerStatus>,
        start_time: Option<chrono::DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        let state = self.state.read().await;

        let status = PodStatus {
            phase,
            pod_ip,
            host_ip: Some("127.0.0.1".to_string()),
            start_time: start_time.or_else(|| Some(Utc::now())),
            container_statuses: Some(container_statuses),
            ..Default::default()
        };

        let pod = Pod {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Pod".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            spec: PodSpec::default(),
            status: Some(status),
        };

        let response = state
            .client
            .put(format!(
                "{}/api/v1/namespaces/{}/pods/{}/status",
                state.server_url, namespace, name
            ))
            .json(&pod)
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to update pod status: {}", body);
        }

        Ok(())
    }
}

fn container_state(info: &super::runtime::ContainerInfo) -> ContainerState {
    if info.running {
        ContainerState {
            running: Some(ContainerStateRunning {
                started_at: info.started_at,
            }),
            ..Default::default()
        }
    } else if let Some(exit) = info.exit_code {
        ContainerState {
            terminated: Some(ContainerStateTerminated {
                exit_code: exit,
                signal: None,
                reason: Some(if exit == 0 { "Completed" } else { "Error" }.to_string()),
                message: None,
                started_at: info.started_at,
                finished_at: None,
                container_id: Some(info.id.clone()),
            }),
            ..Default::default()
        }
    } else {
        ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some("ContainerCreating".to_string()),
                message: None,
            }),
            ..Default::default()
        }
    }
}

fn failed_container_status(container: &Container, err: &anyhow::Error) -> ContainerStatus {
    ContainerStatus {
        name: container.name.clone(),
        ready: false,
        restart_count: 0,
        image: container.image.clone(),
        image_id: container.image.clone(),
        container_id: None,
        state: Some(ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some("CreateContainerError".to_string()),
                message: Some(err.to_string()),
            }),
            ..Default::default()
        }),
        last_state: None,
        started: Some(false),
    }
}

/// Build the Node object for registration/heartbeat.
fn build_node_object(name: &str, labels: &HashMap<String, String>) -> Node {
    let mut capacity = HashMap::from([
        ("cpu".to_string(), detect_cpu_count().to_string()),
        ("memory".to_string(), detect_memory_capacity()),
        ("pods".to_string(), "110".to_string()),
    ]);
    if let Some(disk) = detect_ephemeral_storage() {
        capacity.insert("ephemeral-storage".to_string(), disk);
    }

    let internal_ip = detect_internal_ip().unwrap_or_else(|| "127.0.0.1".to_string());

    let addresses = vec![
        NodeAddress {
            address_type: NodeAddressType::Hostname,
            address: name.to_string(),
        },
        NodeAddress {
            address_type: NodeAddressType::InternalIP,
            address: internal_ip,
        },
    ];

    let conditions = vec![NodeCondition {
        condition_type: NodeConditionType::Ready,
        status: ConditionStatus::True,
        last_heartbeat_time: Some(Utc::now()),
        last_transition_time: Some(Utc::now()),
        reason: Some("KubeletReady".to_string()),
        message: Some("kubelet is posting ready status".to_string()),
    }];

    let node_info = NodeSystemInfo {
        kernel_version: Some(uname_field("-r").unwrap_or_else(|| "unknown".into())),
        os_image: Some(detect_os_image()),
        container_runtime_version: Some(format!("superkube://{}", env!("CARGO_PKG_VERSION"))),
        kubelet_version: Some(format!("superkube/{}", env!("CARGO_PKG_VERSION"))),
        operating_system: Some(std::env::consts::OS.to_string()),
        architecture: Some(std::env::consts::ARCH.to_string()),
        ..Default::default()
    };

    let label_map = if labels.is_empty() {
        None
    } else {
        Some(labels.clone())
    };

    Node {
        type_meta: TypeMeta {
            api_version: Some("v1".to_string()),
            kind: Some("Node".to_string()),
        },
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            uid: Some(Uuid::new_v4()),
            labels: label_map,
            ..Default::default()
        },
        spec: Some(NodeSpec::default()),
        status: Some(NodeStatus {
            capacity: Some(capacity.clone()),
            allocatable: Some(capacity),
            addresses: Some(addresses),
            conditions: Some(conditions),
            node_info: Some(node_info),
            phase: NodePhase::Running,
            ..Default::default()
        }),
    }
}

/// Pull the first three octets out of a pod CIDR (e.g. "10.244.0.0/16" →
/// "10.244.0"). Pod IPs are then assembled as `<prefix>.<last_octet>`. If the
/// input doesn't parse, fall back to the safe default.
fn pod_cidr_to_prefix(cidr: &str) -> String {
    let addr_part = cidr.split('/').next().unwrap_or(cidr);
    let octets: Vec<&str> = addr_part.split('.').collect();
    if octets.len() == 4 && octets.iter().all(|o| o.parse::<u8>().is_ok()) {
        format!("{}.{}.{}", octets[0], octets[1], octets[2])
    } else {
        "10.244.0".to_string()
    }
}

/// Best-effort total disk capacity for the partition holding `/`. Reported as
/// a Kubernetes quantity. None if statvfs / df fails.
fn detect_ephemeral_storage() -> Option<String> {
    let out = std::process::Command::new("df")
        .args(["-Pk", "/"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    // df -Pk:
    //   Filesystem  1024-blocks  Used  Available  Capacity  Mounted on
    //   /dev/disk1s2  ...
    let line = s.lines().nth(1)?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(format_quantity(kb * 1024))
}

/// Run `uname <flag>` and return the stdout, trimmed. None if uname is missing
/// or fails (e.g. on Windows).
fn uname_field(flag: &str) -> Option<String> {
    let out = std::process::Command::new("uname").arg(flag).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Best-effort human-readable OS image string. On Linux this reads
/// /etc/os-release; on macOS it uses sw_vers; otherwise falls back to OS const.
fn detect_os_image() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                    return rest.trim_matches('"').to_string();
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
        {
            if out.status.success() {
                if let Ok(v) = String::from_utf8(out.stdout) {
                    return format!("macOS {}", v.trim());
                }
            }
        }
    }
    std::env::consts::OS.to_string()
}

/// Pick a non-loopback IPv4 address by opening a UDP socket and asking the OS
/// what it would route through. No packet is actually sent.
fn detect_internal_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let addr = sock.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// Number of logical CPUs visible to this process.
fn detect_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Total physical memory, formatted as a Kubernetes-style quantity ("16Gi",
/// "8192Mi", etc.). Reads the OS-specific source on each platform; falls back
/// to "8Gi" if anything goes wrong (so the node still registers cleanly).
fn detect_memory_capacity() -> String {
    if let Some(bytes) = read_total_memory_bytes() {
        return format_quantity(bytes);
    }
    "8Gi".to_string()
}

#[cfg(target_os = "linux")]
fn read_total_memory_bytes() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // Format: "MemTotal:       16322388 kB"
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn read_total_memory_bytes() -> Option<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    s.trim().parse().ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_total_memory_bytes() -> Option<u64> {
    None
}

/// Render a byte count as a Kubernetes quantity. Picks the largest unit that
/// gives a clean integer; falls back to bytes if nothing divides evenly.
fn format_quantity(bytes: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = 1024 * KI;
    const GI: u64 = 1024 * MI;
    const TI: u64 = 1024 * GI;

    if bytes % TI == 0 {
        format!("{}Ti", bytes / TI)
    } else if bytes % GI == 0 {
        format!("{}Gi", bytes / GI)
    } else if bytes % MI == 0 {
        format!("{}Mi", bytes / MI)
    } else if bytes % KI == 0 {
        format!("{}Ki", bytes / KI)
    } else {
        format!("{}", bytes)
    }
}

// ============================================================================
// HTTP Server for Log Requests
// ============================================================================

/// Query parameters for log requests
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LogQueryParams {
    pub tail_lines: Option<i64>,
    #[serde(default)]
    pub timestamps: bool,
    #[serde(default)]
    pub follow: bool,
    #[serde(default)]
    pub previous: bool,
    pub since_seconds: Option<i64>,
    pub limit_bytes: Option<i64>,
}

/// Run the HTTP server for serving log, exec, and port-forward requests
async fn run_http_server(state: Arc<RwLock<AgentState>>) -> anyhow::Result<()> {
    let app = Router::new()
        .route(
            "/logs/:namespace/:pod/:container",
            get(handle_logs),
        )
        .route(
            "/exec/:namespace/:pod/:container",
            get(handle_exec),
        )
        .route(
            "/portforward/:namespace/:pod/:pod_ip",
            get(handle_portforward),
        )
        .with_state(state);

    let addr = format!("0.0.0.0:{}", NODE_AGENT_PORT);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("Node agent HTTP server listening on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Handle log requests
async fn handle_logs(
    State(state): State<Arc<RwLock<AgentState>>>,
    Path((namespace, pod, container)): Path<(String, String, String)>,
    Query(params): Query<LogQueryParams>,
) -> impl IntoResponse {
    if params.follow {
        // Return streaming logs using SSE
        return handle_follow_logs(state, namespace, pod, container, params).await;
    }

    // Return static logs
    let state = state.read().await;

    // Find the container ID - try exact match first
    let container_id = match state.pod_containers.get(&(namespace.clone(), pod.clone(), container.clone())) {
        Some(id) => id.clone(),
        None => {
            // Try to find by pod name only (match any container in the pod)
            let matching: Vec<_> = state
                .pod_containers
                .iter()
                .filter(|((ns, p, _), _)| ns == &namespace && p == &pod)
                .collect();

            if matching.len() == 1 {
                // Found exactly one container in this pod, use it
                matching[0].1.clone()
            } else if matching.is_empty() {
                // Log available containers for debugging
                let known: Vec<String> = state
                    .pod_containers
                    .keys()
                    .map(|(ns, p, c)| format!("{}/{}/{}", ns, p, c))
                    .collect();

                tracing::warn!(
                    "Container not found: {}/{}/{}. Known containers: {:?}",
                    namespace,
                    pod,
                    container,
                    known
                );

                return (
                    StatusCode::NOT_FOUND,
                    format!(
                        "Container {} not found in pod {}/{}. Pod may not be running on this node or hasn't been synced yet.\nKnown containers: {}\n",
                        container,
                        namespace,
                        pod,
                        if known.is_empty() { "none".to_string() } else { known.join(", ") }
                    ),
                )
                    .into_response();
            } else {
                // Multiple containers in pod, need to specify which one
                let containers: Vec<String> = matching
                    .iter()
                    .map(|((_, _, c), _)| c.clone())
                    .collect();

                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Multiple containers in pod {}/{}. Please specify one of: {}\n",
                        namespace,
                        pod,
                        containers.join(", ")
                    ),
                )
                    .into_response();
            }
        }
    };

    // Build log options
    let options = LogOptions {
        tail_lines: params.tail_lines,
        timestamps: params.timestamps,
        since_time: params.since_seconds.map(|s| {
            Utc::now() - chrono::Duration::seconds(s)
        }),
        limit_bytes: params.limit_bytes,
    };

    // Get logs
    match state.runtime.get_logs(&container_id, &options).await {
        Ok(logs) => (StatusCode::OK, logs).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to get logs: {}\n", e),
        )
            .into_response(),
    }
}

/// Handle streaming logs with -f option.
///
/// kubectl expects `kubectl logs -f` to look like a plain HTTP body that
/// keeps producing log bytes. We do a chunked text response backed by the
/// runtime's real follow stream — Docker pumps lines into us as the
/// container produces them.
async fn handle_follow_logs(
    state: Arc<RwLock<AgentState>>,
    namespace: String,
    pod: String,
    container: String,
    params: LogQueryParams,
) -> Response {
    // Resolve container_id by (ns, pod, container) — same fallback logic as
    // the non-follow path.
    let container_id = {
        let state = state.read().await;
        match state
            .pod_containers
            .get(&(namespace.clone(), pod.clone(), container.clone()))
        {
            Some(id) => id.clone(),
            None => {
                let matching: Vec<_> = state
                    .pod_containers
                    .iter()
                    .filter(|((ns, p, _), _)| ns == &namespace && p == &pod)
                    .collect();
                if matching.len() == 1 {
                    matching[0].1.clone()
                } else if matching.is_empty() {
                    let known: Vec<String> = state
                        .pod_containers
                        .keys()
                        .map(|(ns, p, c)| format!("{}/{}/{}", ns, p, c))
                        .collect();
                    return (
                        StatusCode::NOT_FOUND,
                        format!(
                            "Container {} not found in pod {}/{}. Known: {}\n",
                            container,
                            namespace,
                            pod,
                            if known.is_empty() {
                                "none".to_string()
                            } else {
                                known.join(", ")
                            }
                        ),
                    )
                        .into_response();
                } else {
                    let containers: Vec<String> = matching
                        .iter()
                        .map(|((_, _, c), _)| c.clone())
                        .collect();
                    return (
                        StatusCode::BAD_REQUEST,
                        format!(
                            "Multiple containers in pod. Specify one of: {}\n",
                            containers.join(", ")
                        ),
                    )
                        .into_response();
                }
            }
        }
    };

    let options = LogOptions {
        tail_lines: params.tail_lines,
        timestamps: params.timestamps,
        since_time: params
            .since_seconds
            .map(|s| Utc::now() - chrono::Duration::seconds(s)),
        limit_bytes: params.limit_bytes,
    };

    let stream = {
        let s = state.read().await;
        match s.runtime.stream_logs(&container_id, &options).await {
            Ok(stream) => stream,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to start log stream: {}\n", e),
                )
                    .into_response();
            }
        }
    };

    // Map anyhow::Result<Bytes> into axum's Body item type.
    let body_stream = stream.map(|res| res.map_err(std::io::Error::other));

    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .header(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        )
        .body(Body::from_stream(body_stream))
        .unwrap()
}

// ============================================================================
// Exec Handler
// ============================================================================

/// Query parameters for exec requests
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExecQueryParams {
    pub command: Option<String>,
    #[serde(default)]
    pub stdin: bool,
    #[serde(default = "default_true")]
    pub stdout: bool,
    #[serde(default = "default_true")]
    pub stderr: bool,
    #[serde(default)]
    pub tty: bool,
}

fn default_true() -> bool {
    true
}

/// Handle exec requests via WebSocket. Uses the Kubernetes channel-prefixed
/// frame protocol: every binary frame from kubectl is `[channel_byte, data...]`.
///   channel 0 = stdin, 1 = stdout, 2 = stderr, 3 = error, 4 = resize.
///
/// kubectl sends each argv entry as a separate `command=` query param, so we
/// parse the raw query rather than `Query<T>` which would only pick the last.
async fn handle_exec(
    State(state): State<Arc<RwLock<AgentState>>>,
    Path((namespace, pod, container)): Path<(String, String, String)>,
    raw_query: axum::extract::RawQuery,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let container_id = {
        let state = state.read().await;
        match state
            .pod_containers
            .get(&(namespace.clone(), pod.clone(), container.clone()))
            .cloned()
        {
            Some(id) => id,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    format!("Container {} not found in pod {}/{}", container, namespace, pod),
                )
                    .into_response();
            }
        }
    };

    let (cmd, tty) = parse_exec_query(raw_query.0.as_deref().unwrap_or(""));
    let session_state = state.clone();

    ws.protocols([
        "v5.channel.k8s.io",
        "v4.channel.k8s.io",
        "v3.channel.k8s.io",
        "v2.channel.k8s.io",
        "channel.k8s.io",
    ])
    .on_upgrade(move |socket| async move {
        handle_exec_websocket(socket, session_state, container_id, cmd, tty).await;
    })
}

/// Pull every `command=<word>` and `tty=<bool>` out of a query string.
/// kubectl sends `?command=echo&command=hello&tty=false` style URLs.
fn parse_exec_query(q: &str) -> (Vec<String>, bool) {
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(q).unwrap_or_default();
    let cmd: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "command")
        .map(|(_, v)| v.clone())
        .collect();
    let tty = pairs
        .iter()
        .find(|(k, _)| k == "tty")
        .map(|(_, v)| v == "true" || v == "1")
        .unwrap_or(false);
    let cmd = if cmd.is_empty() {
        vec!["sh".to_string()]
    } else {
        cmd
    };
    (cmd, tty)
}

async fn handle_exec_websocket(
    socket: WebSocket,
    state: Arc<RwLock<AgentState>>,
    container_id: String,
    cmd: Vec<String>,
    tty: bool,
) {
    tracing::info!("exec session: container={} cmd={:?} tty={}", container_id, cmd, tty);

    // Spin up the exec session — this is the only point we hold the runtime
    // lock, and only briefly.
    let session = {
        let s = state.read().await;
        match s.runtime.exec(&container_id, cmd.clone(), tty).await {
            Ok(sess) => sess,
            Err(e) => {
                tracing::error!("exec start failed: {}", e);
                let mut socket = socket;
                let _ = socket
                    .send(Message::Binary(error_frame(&format!(
                        "exec failed: {}",
                        e
                    ))))
                    .await;
                let _ = socket.close().await;
                return;
            }
        }
    };

    let mut output = session.output;
    let mut input = session.input;
    let resize = session.resize;

    let (mut ws_tx, mut ws_rx) = socket.split();

    // docker output → kubectl stdout (channel 1)
    let docker_to_ws = async move {
        while let Some(chunk) = output.next().await {
            match chunk {
                Ok(bytes) if !bytes.is_empty() => {
                    let mut frame = Vec::with_capacity(bytes.len() + 1);
                    frame.push(1u8); // stdout channel
                    frame.extend_from_slice(&bytes);
                    if ws_tx.send(Message::Binary(frame)).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!("exec output err: {}", e);
                    break;
                }
            }
        }
        // Tell kubectl the exec finished successfully. Without this
        // sentinel on the error channel, kubectl logs an "abnormal closure"
        // warning when the WS just goes away after stdout ends.
        let status = br#"{"metadata":{},"status":"Success"}"#;
        let mut frame = Vec::with_capacity(status.len() + 1);
        frame.push(3u8); // error channel
        frame.extend_from_slice(status);
        let _ = ws_tx.send(Message::Binary(frame)).await;
        let _ = ws_tx.close().await;
    };

    // kubectl frames → docker stdin / resize
    let ws_to_docker = async move {
        use tokio::io::AsyncWriteExt;
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(Message::Binary(data)) if !data.is_empty() => {
                    let channel = data[0];
                    let payload = &data[1..];
                    match channel {
                        0 => {
                            if input.write_all(payload).await.is_err() {
                                break;
                            }
                            let _ = input.flush().await;
                        }
                        4 => {
                            // {"Width":W,"Height":H}
                            if let Ok(s) = std::str::from_utf8(payload) {
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
                                    let w = v.get("Width").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
                                    let h = v.get("Height").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
                                    let _ = (resize)(h, w).await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    tokio::select! {
        _ = docker_to_ws => {}
        _ = ws_to_docker => {}
    }
    tracing::info!("exec session ended");
}

/// Build a Kubernetes error-channel frame (channel 3).
fn error_frame(msg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + 1);
    out.push(3u8);
    out.extend_from_slice(msg.as_bytes());
    out
}

// ============================================================================
// Port-Forward Handler
// ============================================================================

/// Query parameters for port-forward requests
#[derive(Debug, Deserialize, Default)]
pub struct PortForwardQueryParams {
    pub ports: Option<String>,
}

/// Handle port-forward requests via WebSocket
async fn handle_portforward(
    State(_state): State<Arc<RwLock<AgentState>>>,
    Path((namespace, pod, pod_ip)): Path<(String, String, String)>,
    Query(params): Query<PortForwardQueryParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Parse ports from the query parameters
    let ports: Vec<u16> = params
        .ports
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();

    if ports.is_empty() {
        return (StatusCode::BAD_REQUEST, "No ports specified".to_string()).into_response();
    }

    tracing::info!(
        "Port-forward requested for pod {}/{} ip={} ports={:?}",
        namespace,
        pod,
        pod_ip,
        ports
    );

    ws.on_upgrade(move |socket| handle_portforward_websocket(socket, pod_ip, ports))
}

/// Handle the WebSocket connection for port-forward.
///
/// Bidirectional binary proxy between the kubectl client and `pod_ip:port`.
/// We split the WebSocket and the TCP stream so both directions can run
/// concurrently on top of the same socket.
async fn handle_portforward_websocket(socket: WebSocket, pod_ip: String, ports: Vec<u16>) {
    let port = match ports.first() {
        Some(p) => *p,
        None => {
            let (mut tx, _rx) = socket.split();
            let _ = tx
                .send(Message::Text("No ports specified".to_string()))
                .await;
            return;
        }
    };

    let target_addr = format!("{}:{}", pod_ip, port);
    tracing::info!("port-forward → {}", target_addr);

    let tcp_stream = match TcpStream::connect(&target_addr).await {
        Ok(s) => s,
        Err(e) => {
            let (mut tx, _rx) = socket.split();
            let _ = tx
                .send(Message::Text(format!(
                    "Failed to connect to {}: {}",
                    target_addr, e
                )))
                .await;
            return;
        }
    };

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut tcp_rx, mut tcp_tx) = tcp_stream.into_split();

    let ws_to_tcp = async move {
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if tcp_tx.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Text(text)) => {
                    if tcp_tx.write_all(text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        let _ = tcp_tx.shutdown().await;
    };

    let tcp_to_ws = async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if ws_tx
                        .send(Message::Binary(buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = ws_tx.close().await;
    };

    tokio::select! {
        _ = ws_to_tcp => {},
        _ = tcp_to_ws => {},
    }

    tracing::info!("port-forward session ended for {}", target_addr);
}
