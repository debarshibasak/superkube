use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
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

use super::runtime::{ContainerdRuntime, LogOptions};

/// Node agent HTTP server port
const NODE_AGENT_PORT: u16 = 10250;

/// Run the node agent
pub async fn run(name: &str, server_url: &str, containerd_socket: &str) -> anyhow::Result<()> {
    let agent = NodeAgent::new(name, server_url, containerd_socket).await?;
    agent.run().await
}

/// Shared state for the node agent
struct AgentState {
    name: String,
    server_url: String,
    client: Client,
    runtime: ContainerdRuntime,
    /// Map of pod UID to container ID
    containers: HashMap<Uuid, String>,
    /// Map of (namespace, pod_name, container_name) to container ID for log lookup
    pod_containers: HashMap<(String, String, String), String>,
}

/// Node agent that manages containers on this node
struct NodeAgent {
    state: Arc<RwLock<AgentState>>,
}

impl NodeAgent {
    async fn new(name: &str, server_url: &str, containerd_socket: &str) -> anyhow::Result<Self> {
        let runtime = ContainerdRuntime::new(containerd_socket).await?;

        let state = AgentState {
            name: name.to_string(),
            server_url: server_url.trim_end_matches('/').to_string(),
            client: Client::new(),
            runtime,
            containers: HashMap::new(),
            pod_containers: HashMap::new(),
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

        let node = build_node_object(&state.name);

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
        let node = build_node_object(&state.name);

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

    /// Reconcile a single pod - ensure containers are running
    async fn reconcile_pod(&self, pod: &Pod) -> anyhow::Result<()> {
        let pod_uid = pod.metadata.uid.ok_or_else(|| anyhow::anyhow!("Pod has no UID"))?;
        let namespace = pod.metadata.namespace().to_string();
        let pod_name = pod.metadata.name().to_string();

        let current_phase = pod
            .status
            .as_ref()
            .map(|s| s.phase.clone())
            .unwrap_or(PodPhase::Pending);

        let mut state = self.state.write().await;

        // If pod is already running or terminated, check status
        if state.containers.contains_key(&pod_uid) {
            let container_id = state.containers.get(&pod_uid).unwrap().clone();

            // Check if container is still running
            let is_running = state.runtime.is_container_running(&container_id).await?;

            drop(state);

            if is_running && current_phase != PodPhase::Running {
                // Update pod status to Running
                self.update_pod_status(&namespace, &pod_name, PodPhase::Running, None)
                    .await?;
            } else if !is_running && current_phase == PodPhase::Running {
                // Container stopped, update status
                self.update_pod_status(&namespace, &pod_name, PodPhase::Succeeded, None)
                    .await?;
            }

            return Ok(());
        }

        // Pod is not running, start it
        if current_phase == PodPhase::Pending {
            tracing::info!("Starting pod {}/{}", namespace, pod_name);

            // For simplicity, we only start the first container
            // In a full implementation, we would start all containers
            if let Some(container) = pod.spec.containers.first() {
                let container_name_full = format!("{}-{}", pod_name, container.name);

                match state
                    .runtime
                    .create_and_start_container(&container_name_full, &container.image)
                    .await
                {
                    Ok(container_id) => {
                        tracing::info!(
                            "Started container {} for pod {}/{}",
                            container_id,
                            namespace,
                            pod_name
                        );
                        state.containers.insert(pod_uid, container_id.clone());
                        state.pod_containers.insert(
                            (namespace.clone(), pod_name.clone(), container.name.clone()),
                            container_id,
                        );

                        // Update pod status to Running
                        // Generate a pod IP (simplified - would use CNI in production)
                        let pod_ip = format!("10.244.0.{}", (pod_uid.as_u128() % 254 + 1) as u8);

                        drop(state);

                        self.update_pod_status(&namespace, &pod_name, PodPhase::Running, Some(&pod_ip))
                            .await?;
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to start container for pod {}/{}: {}",
                            namespace,
                            pod_name,
                            e
                        );

                        drop(state);

                        self.update_pod_status(&namespace, &pod_name, PodPhase::Failed, None)
                            .await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Update pod status on the server
    async fn update_pod_status(
        &self,
        namespace: &str,
        name: &str,
        phase: PodPhase,
        pod_ip: Option<&str>,
    ) -> anyhow::Result<()> {
        let state = self.state.read().await;

        let status = PodStatus {
            phase,
            pod_ip: pod_ip.map(|s| s.to_string()),
            host_ip: Some("127.0.0.1".to_string()),
            start_time: Some(Utc::now()),
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

/// Build the Node object for registration/heartbeat
fn build_node_object(name: &str) -> Node {
    let capacity = HashMap::from([
        ("cpu".to_string(), "4".to_string()),
        ("memory".to_string(), "8Gi".to_string()),
        ("pods".to_string(), "110".to_string()),
    ]);

    let addresses = vec![
        NodeAddress {
            address_type: NodeAddressType::Hostname,
            address: name.to_string(),
        },
        NodeAddress {
            address_type: NodeAddressType::InternalIP,
            address: "127.0.0.1".to_string(), // Would get real IP in production
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
        kernel_version: Some(std::env::consts::OS.to_string()),
        os_image: Some("Linux".to_string()),
        container_runtime_version: Some("containerd://1.7.0".to_string()),
        kubelet_version: Some("kais/0.1.0".to_string()),
        operating_system: Some(std::env::consts::OS.to_string()),
        architecture: Some(std::env::consts::ARCH.to_string()),
        ..Default::default()
    };

    Node {
        type_meta: TypeMeta {
            api_version: Some("v1".to_string()),
            kind: Some("Node".to_string()),
        },
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            uid: Some(Uuid::new_v4()),
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

/// Handle streaming logs with -f option
async fn handle_follow_logs(
    state: Arc<RwLock<AgentState>>,
    namespace: String,
    pod: String,
    container: String,
    params: LogQueryParams,
) -> axum::response::Response {
    // First get initial logs
    let (container_id, initial_logs) = {
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
                            if known.is_empty() { "none".to_string() } else { known.join(", ") }
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
        };

        let options = LogOptions {
            tail_lines: params.tail_lines,
            timestamps: params.timestamps,
            since_time: params.since_seconds.map(|s| {
                Utc::now() - chrono::Duration::seconds(s)
            }),
            limit_bytes: params.limit_bytes,
        };

        let logs = state.runtime.get_logs(&container_id, &options).await.unwrap_or_default();
        (container_id, logs)
    };

    // Create a stream that sends initial logs and then polls for new ones
    let stream = async_stream::stream! {
        // Send initial logs as one event
        if !initial_logs.is_empty() {
            yield Ok::<_, Infallible>(Event::default().data(initial_logs));
        }

        // Poll for new logs every second
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let timestamps = params.timestamps;

        loop {
            interval.tick().await;

            let state = state.read().await;
            let options = LogOptions {
                tail_lines: Some(10), // Only get recent logs
                timestamps,
                since_time: Some(Utc::now() - chrono::Duration::seconds(2)),
                limit_bytes: None,
            };

            if let Ok(logs) = state.runtime.get_logs(&container_id, &options).await {
                if !logs.is_empty() {
                    yield Ok::<_, Infallible>(Event::default().data(logs));
                }
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
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

/// Handle exec requests via WebSocket
async fn handle_exec(
    State(state): State<Arc<RwLock<AgentState>>>,
    Path((namespace, pod, container)): Path<(String, String, String)>,
    Query(params): Query<ExecQueryParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Verify the container exists
    let container_exists = {
        let state = state.read().await;
        state
            .pod_containers
            .contains_key(&(namespace.clone(), pod.clone(), container.clone()))
    };

    if !container_exists {
        return (
            StatusCode::NOT_FOUND,
            format!(
                "Container {} not found in pod {}/{}",
                container, namespace, pod
            ),
        )
            .into_response();
    }

    let command = params.command.clone().unwrap_or_else(|| "sh".to_string());

    ws.on_upgrade(move |socket| handle_exec_websocket(socket, command, params))
}

/// Handle the WebSocket connection for exec
async fn handle_exec_websocket(mut socket: WebSocket, command: String, params: ExecQueryParams) {
    tracing::info!("Exec session started for command: {}", command);

    // In a real implementation, this would:
    // 1. Use containerd's exec API to create an exec process in the container
    // 2. Connect stdin/stdout/stderr streams
    // 3. Optionally allocate a PTY

    // For now, we'll provide a mock implementation
    // Send initial message
    let _ = socket
        .send(Message::Text(format!(
            "Executing command '{}' (stdin={}, stdout={}, stderr={}, tty={})\r\n",
            command, params.stdin, params.stdout, params.stderr, params.tty
        )))
        .await;

    // Mock shell simulation
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                // Echo the command and provide mock output
                let response = match text.trim() {
                    "exit" | "quit" => {
                        let _ = socket.send(Message::Text("exit\r\n".to_string())).await;
                        break;
                    }
                    "ls" => "bin  dev  etc  home  lib  proc  root  sys  tmp  usr  var\r\n",
                    "pwd" => "/\r\n",
                    "whoami" => "root\r\n",
                    "hostname" => "container-mock\r\n",
                    "ps" => "PID   USER     TIME  COMMAND\r\n    1 root      0:00 /bin/sh\r\n",
                    "date" => &format!("{}\r\n", chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")),
                    "" => "",
                    cmd => &format!("sh: {}: command not found\r\n", cmd),
                };

                if !response.is_empty() {
                    let _ = socket.send(Message::Text(response.to_string())).await;
                }

                // Send prompt
                if params.tty {
                    let _ = socket.send(Message::Text("# ".to_string())).await;
                }
            }
            Some(Ok(Message::Binary(data))) => {
                // Handle binary data (e.g., raw terminal input)
                if let Ok(text) = String::from_utf8(data) {
                    let _ = socket.send(Message::Text(format!("received: {}\r\n", text))).await;
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                tracing::info!("Exec session ended");
                break;
            }
            Some(Err(e)) => {
                tracing::error!("Exec WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    let _ = socket.close().await;
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

/// Handle the WebSocket connection for port-forward
async fn handle_portforward_websocket(mut socket: WebSocket, pod_ip: String, ports: Vec<u16>) {
    tracing::info!(
        "Port-forward session started for {}:{:?}",
        pod_ip,
        ports
    );

    // Use the first port for simplicity
    let port = match ports.first() {
        Some(p) => *p,
        None => {
            let _ = socket
                .send(Message::Text("No ports specified".to_string()))
                .await;
            return;
        }
    };

    // Try to connect to the target address
    let target_addr = format!("{}:{}", pod_ip, port);

    match TcpStream::connect(&target_addr).await {
        Ok(tcp_stream) => {
            let _ = socket
                .send(Message::Text(format!(
                    "Connected to {}",
                    target_addr
                )))
                .await;

            // Split the TCP stream
            let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();

            // Proxy data between WebSocket and TCP
            let ws_to_tcp = async {
                while let Some(msg) = socket.recv().await {
                    match msg {
                        Ok(Message::Binary(data)) => {
                            if tcp_write.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
            };

            let tcp_to_ws = async {
                let mut buf = vec![0u8; 8192];
                loop {
                    match tcp_read.read(&mut buf).await {
                        Ok(0) => break, // Connection closed
                        Ok(n) => {
                            // We need to send through a channel or handle differently
                            // For now, just log
                            tracing::debug!("Read {} bytes from TCP", n);
                        }
                        Err(_) => break,
                    }
                }
            };

            tokio::select! {
                _ = ws_to_tcp => {},
                _ = tcp_to_ws => {},
            }
        }
        Err(e) => {
            // Connection failed - this is expected in mock mode
            // Send a mock response instead
            let _ = socket
                .send(Message::Text(format!(
                    "Port-forward mock mode: would connect to {} (error: {})\r\n\
                     In production, this would tunnel TCP traffic to the container.\r\n",
                    target_addr, e
                )))
                .await;

            // Keep the connection open for demonstration
            while let Some(msg) = socket.recv().await {
                match msg {
                    Ok(Message::Binary(data)) => {
                        // Echo back what we received
                        let _ = socket
                            .send(Message::Text(format!(
                                "Would forward {} bytes to {}\r\n",
                                data.len(),
                                target_addr
                            )))
                            .await;
                    }
                    Ok(Message::Text(text)) => {
                        let _ = socket
                            .send(Message::Text(format!(
                                "Would forward: {}\r\n",
                                text
                            )))
                            .await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        }
    }

    tracing::info!("Port-forward session ended");
    let _ = socket.close().await;
}
