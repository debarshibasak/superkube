use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_tungstenite::{connect_async, tungstenite::Message as TungsteniteMessage};

use crate::db::{
    DaemonSetRepository, DeploymentRepository, EndpointsRepository, EventRepository,
    NamespaceRepository, NodeRepository, PodRepository, ServiceRepository, StatefulSetRepository,
};
use crate::error::Result;
use crate::models::*;

use super::printers;
use super::table;
use super::AppState;

// Query parameters for list operations
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    pub label_selector: Option<String>,
    pub field_selector: Option<String>,
    pub limit: Option<i64>,
    #[serde(rename = "continue")]
    pub continue_token: Option<String>,
}

impl ListParams {
    pub fn parse_label_selector(&self) -> Option<HashMap<String, String>> {
        self.label_selector.as_ref().map(|s| {
            s.split(',')
                .filter_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    match (parts.next(), parts.next()) {
                        (Some(k), Some(v)) => Some((k.to_string(), v.to_string())),
                        _ => None,
                    }
                })
                .collect()
        })
    }
}

// ============================================================================
// Discovery Endpoints
// ============================================================================

/// `kubectl cluster-info` and `kubectl version` hit /version. We pretend to be
/// a recent-ish Kubernetes minor so client tooling doesn't complain. The build
/// metadata says "kais" so it's clear what's actually serving.
pub async fn version_info() -> Json<Value> {
    Json(json!({
        "major": "1",
        "minor": "30",
        "gitVersion": format!("v1.30.0-kais-{}", env!("CARGO_PKG_VERSION")),
        "gitCommit": "kais",
        "gitTreeState": "clean",
        "buildDate": "1970-01-01T00:00:00Z",
        "goVersion": "rust",
        "compiler": "rustc",
        "platform": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
    }))
}

pub async fn api_versions() -> Json<Value> {
    Json(json!({
        "kind": "APIVersions",
        "versions": ["v1"],
        "serverAddressByClientCIDRs": []
    }))
}

pub async fn api_v1_resources() -> Json<Value> {
    // `categories: ["all"]` is what `kubectl get all` filters on.
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "v1",
        "resources": [
            {
                "name": "pods",
                "singularName": "pod",
                "namespaced": true,
                "kind": "Pod",
                "shortNames": ["po"],
                "categories": ["all"],
                "verbs": ["create", "delete", "get", "list", "update", "watch"]
            },
            {
                "name": "services",
                "singularName": "service",
                "namespaced": true,
                "kind": "Service",
                "shortNames": ["svc"],
                "categories": ["all"],
                "verbs": ["create", "delete", "get", "list", "update"]
            },
            {
                "name": "nodes",
                "singularName": "node",
                "namespaced": false,
                "kind": "Node",
                "shortNames": ["no"],
                "verbs": ["create", "delete", "get", "list", "update"]
            },
            {
                "name": "endpoints",
                "singularName": "endpoints",
                "namespaced": true,
                "kind": "Endpoints",
                "shortNames": ["ep"],
                "verbs": ["get", "list"]
            },
            {
                "name": "namespaces",
                "singularName": "namespace",
                "namespaced": false,
                "kind": "Namespace",
                "shortNames": ["ns"],
                "verbs": ["create", "delete", "get", "list", "update"]
            },
            {
                "name": "events",
                "singularName": "event",
                "namespaced": true,
                "kind": "Event",
                "shortNames": ["ev"],
                "verbs": ["create", "delete", "get", "list"]
            }
        ]
    }))
}

pub async fn api_groups() -> Json<Value> {
    Json(json!({
        "kind": "APIGroupList",
        "apiVersion": "v1",
        "groups": [
            {
                "name": "apps",
                "versions": [
                    {
                        "groupVersion": "apps/v1",
                        "version": "v1"
                    }
                ],
                "preferredVersion": {
                    "groupVersion": "apps/v1",
                    "version": "v1"
                }
            }
        ]
    }))
}

pub async fn apps_v1_resources() -> Json<Value> {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "apps/v1",
        "resources": [
            {
                "name": "deployments",
                "singularName": "deployment",
                "namespaced": true,
                "kind": "Deployment",
                "shortNames": ["deploy"],
                "categories": ["all"],
                "verbs": ["create", "delete", "get", "list", "update", "watch"]
            },
            {
                "name": "statefulsets",
                "singularName": "statefulset",
                "namespaced": true,
                "kind": "StatefulSet",
                "shortNames": ["sts"],
                "categories": ["all"],
                "verbs": ["create", "delete", "get", "list", "update", "watch"]
            },
            {
                "name": "daemonsets",
                "singularName": "daemonset",
                "namespaced": true,
                "kind": "DaemonSet",
                "shortNames": ["ds"],
                "categories": ["all"],
                "verbs": ["create", "delete", "get", "list", "update", "watch"]
            }
        ]
    }))
}

// ============================================================================
// Pod Handlers
// ============================================================================

pub async fn list_pods(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Query(params): Query<ListParams>,
    headers: HeaderMap,
) -> Result<Response> {
    let label_selector = params.parse_label_selector();
    let pods = PodRepository::list(&state.pool, Some(&namespace), label_selector.as_ref()).await?;
    Ok(table::list_response(
        &headers, "v1", "PodList", pods, printers::POD_COLUMNS, printers::pod_row,
    ))
}

pub async fn list_all_pods(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListParams>,
    headers: HeaderMap,
) -> Result<Response> {
    let label_selector = params.parse_label_selector();
    let pods = PodRepository::list(&state.pool, None, label_selector.as_ref()).await?;
    Ok(table::list_response(
        &headers, "v1", "PodList", pods, printers::POD_COLUMNS, printers::pod_row,
    ))
}

pub async fn get_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let pod = PodRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(&headers, pod, printers::POD_COLUMNS, printers::pod_row))
}

pub async fn create_pod(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut pod): Json<Pod>,
) -> Result<(StatusCode, Json<Pod>)> {
    // Ensure namespace is set
    if pod.metadata.namespace.is_none() {
        pod.metadata.namespace = Some(namespace);
    }
    let created = PodRepository::create(&state.pool, &pod).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut pod): Json<Pod>,
) -> Result<Json<Pod>> {
    pod.metadata.namespace = Some(namespace);
    pod.metadata.name = Some(name);
    let updated = PodRepository::create(&state.pool, &pod).await?;
    Ok(Json(updated))
}

pub async fn delete_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    PodRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "kind": "pods"
        }
    })))
}

pub async fn get_pod_status(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Pod>> {
    let pod = PodRepository::get(&state.pool, &namespace, &name).await?;
    Ok(Json(pod))
}

pub async fn update_pod_status(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(pod): Json<Pod>,
) -> Result<Json<Pod>> {
    if let Some(status) = &pod.status {
        PodRepository::update_status(&state.pool, &namespace, &name, status).await?;
    }
    let updated = PodRepository::get(&state.pool, &namespace, &name).await?;
    Ok(Json(updated))
}

/// Query parameters for log requests
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LogParams {
    /// Container name (required if pod has multiple containers)
    pub container: Option<String>,
    /// Follow the log stream (not yet implemented)
    #[serde(default)]
    pub follow: bool,
    /// Return previous terminated container logs
    #[serde(default)]
    pub previous: bool,
    /// Number of lines from the end of the logs to show
    pub tail_lines: Option<i64>,
    /// Include timestamps on each line
    #[serde(default)]
    pub timestamps: bool,
    /// Relative time in seconds before the current time to show logs
    pub since_seconds: Option<i64>,
    /// Maximum bytes of logs to return
    pub limit_bytes: Option<i64>,
}

pub async fn get_pod_logs(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Query(params): Query<LogParams>,
) -> Result<Response> {
    // Get the pod to verify it exists and find the node
    let pod = PodRepository::get(&state.pool, &namespace, &name).await?;

    // Determine the container name
    let container_name = if let Some(ref container) = params.container {
        container.clone()
    } else if pod.spec.containers.len() == 1 {
        pod.spec.containers[0].name.clone()
    } else if pod.spec.containers.is_empty() {
        return Err(crate::error::Error::BadRequest(
            "pod has no containers".to_string(),
        ));
    } else {
        // Multiple containers, need to specify which one
        return Err(crate::error::Error::BadRequest(format!(
            "a]container name must be specified for pod {}, choose one of: [{}]",
            name,
            pod.spec
                .containers
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };

    // Check if pod is scheduled to a node
    let node_name = match &pod.spec.node_name {
        Some(node) => node.clone(),
        None => {
            return Ok(plain_text(format!(
                "Pod {} is not yet scheduled to a node\n",
                name
            )));
        }
    };

    let node = match crate::db::NodeRepository::get(&state.pool, &node_name).await {
        Ok(n) => n,
        Err(_) => {
            return Ok(plain_text(format!(
                "Node {} not found, cannot retrieve logs\n",
                node_name
            )));
        }
    };

    let node_address = node
        .status
        .as_ref()
        .and_then(|s| s.addresses.as_ref())
        .and_then(|addrs| {
            addrs
                .iter()
                .find(|a| a.address_type == crate::models::NodeAddressType::InternalIP)
                .map(|a| a.address.clone())
        });

    let node_addr = match node_address {
        Some(addr) => addr,
        None => {
            return Ok(plain_text(format!(
                "Node {} has no internal IP address, cannot retrieve logs\n",
                node_name
            )));
        }
    };

    let log_url = format!(
        "http://{}:10250/logs/{}/{}/{}",
        node_addr, namespace, name, container_name
    );

    let client = reqwest::Client::new();
    let mut request = client.get(&log_url);

    if let Some(tail) = params.tail_lines {
        request = request.query(&[("tailLines", tail.to_string())]);
    }
    if params.timestamps {
        request = request.query(&[("timestamps", "true")]);
    }
    if params.previous {
        request = request.query(&[("previous", "true")]);
    }
    if let Some(since) = params.since_seconds {
        request = request.query(&[("sinceSeconds", since.to_string())]);
    }
    if let Some(limit) = params.limit_bytes {
        request = request.query(&[("limitBytes", limit.to_string())]);
    }
    if params.follow {
        request = request.query(&[("follow", "true")]);
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(plain_text(format!(
                "Cannot connect to node agent on {}: {}\nMake sure the node agent is running.\n",
                node_name, e
            )));
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        return Ok(plain_text(format!(
            "Failed to get logs from node {}: HTTP {}\n",
            node_name, status
        )));
    }

    if params.follow {
        // Streaming proxy: pipe the agent's response body straight to the
        // kubectl client. Calling `.text()` here would wait for EOF, which
        // never comes when the container is still running.
        let stream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
        let body = axum::body::Body::from_stream(stream);
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(body)
            .unwrap());
    }

    match response.text().await {
        Ok(logs) => Ok(plain_text(logs)),
        Err(e) => Ok(plain_text(format!(
            "Failed to read logs from node: {}\n",
            e
        ))),
    }
}

fn plain_text(s: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(axum::body::Body::from(s))
        .unwrap()
}

/// Query parameters for exec requests
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExecParams {
    /// Container name (required if pod has multiple containers)
    pub container: Option<String>,
    /// Command to execute
    pub command: Option<String>,
    /// Stdin if true, stdin is opened
    #[serde(default)]
    pub stdin: bool,
    /// Stdout if true, stdout is returned
    #[serde(default = "default_true")]
    pub stdout: bool,
    /// Stderr if true, stderr is returned
    #[serde(default = "default_true")]
    pub stderr: bool,
    /// TTY if true, allocate a pseudo-TTY
    #[serde(default)]
    pub tty: bool,
}

fn default_true() -> bool {
    true
}

/// Handle exec requests via WebSocket
pub async fn exec_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Query(params): Query<ExecParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Get the pod to verify it exists and find the node
    let pod = match PodRepository::get(&state.pool, &namespace, &name).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                format!("Pod {}/{} not found: {}", namespace, name, e),
            )
                .into_response();
        }
    };

    // Determine the container name
    let container_name = if let Some(ref container) = params.container {
        container.clone()
    } else if pod.spec.containers.len() == 1 {
        pod.spec.containers[0].name.clone()
    } else if pod.spec.containers.is_empty() {
        return (StatusCode::BAD_REQUEST, "pod has no containers".to_string()).into_response();
    } else {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "container name must be specified for pod {}, choose one of: [{}]",
                name,
                pod.spec
                    .containers
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
            .into_response();
    };

    // Check if pod is scheduled to a node
    let node_name = match &pod.spec.node_name {
        Some(node) => node.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Pod {} is not yet scheduled to a node", name),
            )
                .into_response();
        }
    };

    // Get the node to find its address
    let node = match NodeRepository::get(&state.pool, &node_name).await {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                format!("Node {} not found", node_name),
            )
                .into_response();
        }
    };

    // Find the node's internal IP address
    let node_address = node
        .status
        .as_ref()
        .and_then(|s| s.addresses.as_ref())
        .and_then(|addrs| {
            addrs
                .iter()
                .find(|a| a.address_type == crate::models::NodeAddressType::InternalIP)
                .map(|a| a.address.clone())
        });

    let node_addr = match node_address {
        Some(addr) => addr,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Node {} has no internal IP address", node_name),
            )
                .into_response();
        }
    };

    // Build the WebSocket URL to the node agent's exec endpoint
    let exec_url = format!(
        "ws://{}:10250/exec/{}/{}/{}?command={}&stdin={}&stdout={}&stderr={}&tty={}",
        node_addr,
        namespace,
        name,
        container_name,
        params.command.as_deref().unwrap_or("sh"),
        params.stdin,
        params.stdout,
        params.stderr,
        params.tty
    );

    ws.on_upgrade(move |socket| handle_exec_websocket(socket, exec_url))
}

/// Handle the WebSocket connection for exec, proxying to node agent
async fn handle_exec_websocket(mut client_socket: WebSocket, node_url: String) {
    // Connect to the node agent's WebSocket
    let (node_socket, _) = match connect_async(&node_url).await {
        Ok(conn) => conn,
        Err(e) => {
            let _ = client_socket
                .send(Message::Text(format!("Failed to connect to node: {}", e)))
                .await;
            let _ = client_socket.close().await;
            return;
        }
    };

    let (mut node_write, mut node_read) = node_socket.split();
    let (mut client_write, mut client_read) = client_socket.split();

    // Proxy messages between client and node agent
    let client_to_node = async {
        while let Some(msg) = client_read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if node_write
                        .send(TungsteniteMessage::Text(text.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    if node_write
                        .send(TungsteniteMessage::Binary(data.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    let node_to_client = async {
        while let Some(msg) = node_read.next().await {
            match msg {
                Ok(TungsteniteMessage::Text(text)) => {
                    if client_write
                        .send(Message::Text(text.to_string()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Binary(data)) => {
                    if client_write
                        .send(Message::Binary(data.to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    // Run both directions concurrently
    tokio::select! {
        _ = client_to_node => {},
        _ = node_to_client => {},
    }
}

/// Query parameters for port-forward requests
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PortForwardParams {
    /// Ports to forward (comma-separated)
    pub ports: Option<String>,
}

/// Handle port-forward requests via WebSocket
pub async fn port_forward_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Query(params): Query<PortForwardParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Get the pod to verify it exists and find the node
    let pod = match PodRepository::get(&state.pool, &namespace, &name).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                format!("Pod {}/{} not found: {}", namespace, name, e),
            )
                .into_response();
        }
    };

    // Check if pod is scheduled to a node
    let node_name = match &pod.spec.node_name {
        Some(node) => node.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Pod {} is not yet scheduled to a node", name),
            )
                .into_response();
        }
    };

    // Get the node to find its address
    let node = match NodeRepository::get(&state.pool, &node_name).await {
        Ok(n) => n,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                format!("Node {} not found", node_name),
            )
                .into_response();
        }
    };

    // Find the node's internal IP address
    let node_address = node
        .status
        .as_ref()
        .and_then(|s| s.addresses.as_ref())
        .and_then(|addrs| {
            addrs
                .iter()
                .find(|a| a.address_type == crate::models::NodeAddressType::InternalIP)
                .map(|a| a.address.clone())
        });

    let node_addr = match node_address {
        Some(addr) => addr,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Node {} has no internal IP address", node_name),
            )
                .into_response();
        }
    };

    // Get pod IP for port forwarding
    let pod_ip = pod
        .status
        .as_ref()
        .and_then(|s| s.pod_ip.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // Build the WebSocket URL to the node agent's port-forward endpoint
    let pf_url = format!(
        "ws://{}:10250/portforward/{}/{}/{}?ports={}",
        node_addr,
        namespace,
        name,
        pod_ip,
        params.ports.as_deref().unwrap_or("")
    );

    ws.on_upgrade(move |socket| handle_portforward_websocket(socket, pf_url))
}

/// Handle the WebSocket connection for port-forward, proxying to node agent
async fn handle_portforward_websocket(mut client_socket: WebSocket, node_url: String) {
    // Connect to the node agent's WebSocket
    let (node_socket, _) = match connect_async(&node_url).await {
        Ok(conn) => conn,
        Err(e) => {
            let _ = client_socket
                .send(Message::Text(format!(
                    "Failed to connect to node: {}",
                    e
                )))
                .await;
            let _ = client_socket.close().await;
            return;
        }
    };

    let (mut node_write, mut node_read) = node_socket.split();
    let (mut client_write, mut client_read) = client_socket.split();

    // Proxy messages between client and node agent
    let client_to_node = async {
        while let Some(msg) = client_read.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if node_write
                        .send(TungsteniteMessage::Binary(data.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    let node_to_client = async {
        while let Some(msg) = node_read.next().await {
            match msg {
                Ok(TungsteniteMessage::Binary(data)) => {
                    if client_write
                        .send(Message::Binary(data.to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(TungsteniteMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    // Run both directions concurrently
    tokio::select! {
        _ = client_to_node => {},
        _ = node_to_client => {},
    }
}

// ============================================================================
// Service Handlers
// ============================================================================

pub async fn list_services(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let services = ServiceRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(
        &headers, "v1", "ServiceList", services, printers::SERVICE_COLUMNS, printers::service_row,
    ))
}

pub async fn list_all_services(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let services = ServiceRepository::list(&state.pool, None).await?;
    Ok(table::list_response(
        &headers, "v1", "ServiceList", services, printers::SERVICE_COLUMNS, printers::service_row,
    ))
}

pub async fn get_service(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let service = ServiceRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(
        &headers, service, printers::SERVICE_COLUMNS, printers::service_row,
    ))
}

pub async fn create_service(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut service): Json<Service>,
) -> Result<(StatusCode, Json<Service>)> {
    if service.metadata.namespace.is_none() {
        service.metadata.namespace = Some(namespace);
    }
    // Auto-assign ClusterIP if not set
    if service.spec.cluster_ip.is_none() && service.spec.service_type == ServiceType::ClusterIP {
        let uid = service.metadata.uid.unwrap_or_else(uuid::Uuid::new_v4);
        let octet = (uid.as_u128() % 254 + 1) as u8;
        service.spec.cluster_ip = Some(format!("10.96.0.{}", octet));
    }
    let created = ServiceRepository::create(&state.pool, &service).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_service(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut service): Json<Service>,
) -> Result<Json<Service>> {
    service.metadata.namespace = Some(namespace);
    service.metadata.name = Some(name);
    let updated = ServiceRepository::create(&state.pool, &service).await?;
    Ok(Json(updated))
}

pub async fn delete_service(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    ServiceRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "kind": "services"
        }
    })))
}

// ============================================================================
// Node Handlers
// ============================================================================

pub async fn list_nodes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let nodes = NodeRepository::list(&state.pool).await?;
    Ok(table::list_response(
        &headers, "v1", "NodeList", nodes, printers::NODE_COLUMNS, printers::node_row,
    ))
}

pub async fn get_node(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let node = NodeRepository::get(&state.pool, &name).await?;
    Ok(table::item_response(
        &headers, node, printers::NODE_COLUMNS, printers::node_row,
    ))
}

pub async fn create_node(
    State(state): State<Arc<AppState>>,
    Json(node): Json<Node>,
) -> Result<(StatusCode, Json<Node>)> {
    let created = NodeRepository::create(&state.pool, &node).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_node(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(mut node): Json<Node>,
) -> Result<Json<Node>> {
    node.metadata.name = Some(name);
    let updated = NodeRepository::create(&state.pool, &node).await?;
    Ok(Json(updated))
}

pub async fn delete_node(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    NodeRepository::delete(&state.pool, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "kind": "nodes"
        }
    })))
}

// ============================================================================
// Endpoints Handlers
// ============================================================================

pub async fn list_endpoints(
    State(_state): State<Arc<AppState>>,
    Path(_namespace): Path<String>,
) -> Result<Json<List<Endpoints>>> {
    // For now, return empty list - endpoints are managed by controller
    Ok(Json(List::new("v1", "EndpointsList", vec![])))
}

pub async fn get_endpoints(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Endpoints>> {
    let endpoints = EndpointsRepository::get(&state.pool, &namespace, &name).await?;
    Ok(Json(endpoints))
}

// ============================================================================
// Deployment Handlers
// ============================================================================

pub async fn list_deployments(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let deployments = DeploymentRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(
        &headers,
        "apps/v1",
        "DeploymentList",
        deployments,
        printers::DEPLOYMENT_COLUMNS,
        printers::deployment_row,
    ))
}

pub async fn list_all_deployments(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let deployments = DeploymentRepository::list(&state.pool, None).await?;
    Ok(table::list_response(
        &headers,
        "apps/v1",
        "DeploymentList",
        deployments,
        printers::DEPLOYMENT_COLUMNS,
        printers::deployment_row,
    ))
}

pub async fn get_deployment(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let deployment = DeploymentRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(
        &headers,
        deployment,
        printers::DEPLOYMENT_COLUMNS,
        printers::deployment_row,
    ))
}

pub async fn create_deployment(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut deployment): Json<Deployment>,
) -> Result<(StatusCode, Json<Deployment>)> {
    if deployment.metadata.namespace.is_none() {
        deployment.metadata.namespace = Some(namespace);
    }
    let created = DeploymentRepository::create(&state.pool, &deployment).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_deployment(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut deployment): Json<Deployment>,
) -> Result<Json<Deployment>> {
    deployment.metadata.namespace = Some(namespace);
    deployment.metadata.name = Some(name);
    let updated = DeploymentRepository::create(&state.pool, &deployment).await?;
    Ok(Json(updated))
}

pub async fn delete_deployment(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    // First delete all pods owned by this deployment
    let deployment = DeploymentRepository::get(&state.pool, &namespace, &name).await?;
    if let Some(selector) = &deployment.spec.selector.match_labels {
        let pods = PodRepository::list(&state.pool, Some(&namespace), Some(selector)).await?;
        for pod in pods {
            if let Some(name) = pod.metadata.name {
                let _ = PodRepository::delete(&state.pool, &namespace, &name).await;
            }
        }
    }

    DeploymentRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "group": "apps",
            "kind": "deployments"
        }
    })))
}

// ============================================================================
// Namespace Handlers
// ============================================================================

pub async fn list_namespaces(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let namespaces = NamespaceRepository::list(&state.pool).await?;
    Ok(table::list_response(
        &headers,
        "v1",
        "NamespaceList",
        namespaces,
        printers::NAMESPACE_COLUMNS,
        printers::namespace_row,
    ))
}

pub async fn get_namespace(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let namespace = NamespaceRepository::get(&state.pool, &name).await?;
    Ok(table::item_response(
        &headers,
        namespace,
        printers::NAMESPACE_COLUMNS,
        printers::namespace_row,
    ))
}

pub async fn create_namespace(
    State(state): State<Arc<AppState>>,
    Json(namespace): Json<Namespace>,
) -> Result<(StatusCode, Json<Namespace>)> {
    let created = NamespaceRepository::create(&state.pool, &namespace).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_namespace(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(mut namespace): Json<Namespace>,
) -> Result<Json<Namespace>> {
    namespace.metadata.name = Some(name);
    let updated = NamespaceRepository::create(&state.pool, &namespace).await?;
    Ok(Json(updated))
}

pub async fn delete_namespace(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    NamespaceRepository::delete(&state.pool, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "kind": "namespaces"
        }
    })))
}

// ============================================================================
// Event Handlers
// ============================================================================

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EventListParams {
    pub field_selector: Option<String>,
}

impl EventListParams {
    /// Parse field selector to extract involvedObject.name and involvedObject.kind
    pub fn parse_involved_object(&self) -> (Option<String>, Option<String>) {
        let mut name = None;
        let mut kind = None;

        if let Some(ref selector) = self.field_selector {
            for pair in selector.split(',') {
                let parts: Vec<&str> = pair.splitn(2, '=').collect();
                if parts.len() == 2 {
                    match parts[0] {
                        "involvedObject.name" => name = Some(parts[1].to_string()),
                        "involvedObject.kind" => kind = Some(parts[1].to_string()),
                        _ => {}
                    }
                }
            }
        }

        (name, kind)
    }
}

pub async fn list_events(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Query(params): Query<EventListParams>,
    headers: HeaderMap,
) -> Result<Response> {
    let (obj_name, obj_kind) = params.parse_involved_object();
    let events = EventRepository::list(
        &state.pool,
        Some(&namespace),
        obj_name.as_deref(),
        obj_kind.as_deref(),
    )
    .await?;
    Ok(table::list_response(
        &headers,
        "v1",
        "EventList",
        events,
        printers::EVENT_COLUMNS,
        printers::event_row,
    ))
}

pub async fn list_all_events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<EventListParams>,
    headers: HeaderMap,
) -> Result<Response> {
    let (obj_name, obj_kind) = params.parse_involved_object();
    let events = EventRepository::list(
        &state.pool,
        None,
        obj_name.as_deref(),
        obj_kind.as_deref(),
    )
    .await?;
    Ok(table::list_response(
        &headers,
        "v1",
        "EventList",
        events,
        printers::EVENT_COLUMNS,
        printers::event_row,
    ))
}

pub async fn get_event(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let event = EventRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(
        &headers,
        event,
        printers::EVENT_COLUMNS,
        printers::event_row,
    ))
}

pub async fn create_event(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut event): Json<Event>,
) -> Result<(StatusCode, Json<Event>)> {
    if event.metadata.namespace.is_none() {
        event.metadata.namespace = Some(namespace);
    }
    let created = EventRepository::create(&state.pool, &event).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn delete_event(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    EventRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1",
        "kind": "Status",
        "metadata": {},
        "status": "Success",
        "details": {
            "name": name,
            "kind": "events"
        }
    })))
}

// ============================================================================
// StatefulSet Handlers
// ============================================================================

pub async fn list_statefulsets(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = StatefulSetRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(
        &headers,
        "apps/v1",
        "StatefulSetList",
        items,
        printers::STATEFULSET_COLUMNS,
        printers::statefulset_row,
    ))
}

pub async fn list_all_statefulsets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = StatefulSetRepository::list(&state.pool, None).await?;
    Ok(table::list_response(
        &headers,
        "apps/v1",
        "StatefulSetList",
        items,
        printers::STATEFULSET_COLUMNS,
        printers::statefulset_row,
    ))
}

pub async fn get_statefulset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = StatefulSetRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(
        &headers,
        item,
        printers::STATEFULSET_COLUMNS,
        printers::statefulset_row,
    ))
}

pub async fn create_statefulset(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut ss): Json<StatefulSet>,
) -> Result<(StatusCode, Json<StatefulSet>)> {
    if ss.metadata.namespace.is_none() {
        ss.metadata.namespace = Some(namespace);
    }
    let created = StatefulSetRepository::create(&state.pool, &ss).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_statefulset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut ss): Json<StatefulSet>,
) -> Result<Json<StatefulSet>> {
    ss.metadata.namespace = Some(namespace);
    ss.metadata.name = Some(name);
    Ok(Json(StatefulSetRepository::create(&state.pool, &ss).await?))
}

pub async fn delete_statefulset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    // Best-effort: also delete the owned pods.
    if let Ok(ss) = StatefulSetRepository::get(&state.pool, &namespace, &name).await {
        if let Some(selector) = &ss.spec.selector.match_labels {
            if let Ok(pods) = PodRepository::list(&state.pool, Some(&namespace), Some(selector)).await {
                for pod in pods {
                    if let Some(pod_name) = pod.metadata.name {
                        let _ = PodRepository::delete(&state.pool, &namespace, &pod_name).await;
                    }
                }
            }
        }
    }
    StatefulSetRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1", "kind": "Status", "metadata": {}, "status": "Success",
        "details": {"name": name, "group": "apps", "kind": "statefulsets"}
    })))
}

// ============================================================================
// DaemonSet Handlers
// ============================================================================

pub async fn list_daemonsets(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = DaemonSetRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(
        &headers,
        "apps/v1",
        "DaemonSetList",
        items,
        printers::DAEMONSET_COLUMNS,
        printers::daemonset_row,
    ))
}

pub async fn list_all_daemonsets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = DaemonSetRepository::list(&state.pool, None).await?;
    Ok(table::list_response(
        &headers,
        "apps/v1",
        "DaemonSetList",
        items,
        printers::DAEMONSET_COLUMNS,
        printers::daemonset_row,
    ))
}

pub async fn get_daemonset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = DaemonSetRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(
        &headers,
        item,
        printers::DAEMONSET_COLUMNS,
        printers::daemonset_row,
    ))
}

pub async fn create_daemonset(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut ds): Json<DaemonSet>,
) -> Result<(StatusCode, Json<DaemonSet>)> {
    if ds.metadata.namespace.is_none() {
        ds.metadata.namespace = Some(namespace);
    }
    let created = DaemonSetRepository::create(&state.pool, &ds).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_daemonset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut ds): Json<DaemonSet>,
) -> Result<Json<DaemonSet>> {
    ds.metadata.namespace = Some(namespace);
    ds.metadata.name = Some(name);
    Ok(Json(DaemonSetRepository::create(&state.pool, &ds).await?))
}

pub async fn delete_daemonset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    if let Ok(ds) = DaemonSetRepository::get(&state.pool, &namespace, &name).await {
        if let Some(selector) = &ds.spec.selector.match_labels {
            if let Ok(pods) = PodRepository::list(&state.pool, Some(&namespace), Some(selector)).await {
                for pod in pods {
                    if let Some(pod_name) = pod.metadata.name {
                        let _ = PodRepository::delete(&state.pool, &namespace, &pod_name).await;
                    }
                }
            }
        }
    }
    DaemonSetRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({
        "apiVersion": "v1", "kind": "Status", "metadata": {}, "status": "Success",
        "details": {"name": name, "group": "apps", "kind": "daemonsets"}
    })))
}
