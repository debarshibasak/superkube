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
    ClusterRoleBindingRepository, ClusterRoleRepository, ConfigMapRepository,
    DaemonSetRepository, DeploymentRepository, EndpointsRepository, EventRepository,
    NamespaceRepository, NodeRepository, PodRepository, RoleBindingRepository, RoleRepository,
    SecretRepository, ServiceAccountRepository, ServiceRepository, StatefulSetRepository,
};
use crate::error::Result;
use crate::models::*;

use super::printers;
use super::table;
use super::AppState;

/// Pull the first three octets out of a service CIDR (e.g. "10.96.0.0/12" →
/// "10.96.0"). ClusterIPs are then assembled as `<prefix>.<last_octet>`.
fn service_cidr_prefix(cidr: &str) -> String {
    let addr_part = cidr.split('/').next().unwrap_or(cidr);
    let octets: Vec<&str> = addr_part.split('.').collect();
    if octets.len() == 4 && octets.iter().all(|o| o.parse::<u8>().is_ok()) {
        format!("{}.{}.{}", octets[0], octets[1], octets[2])
    } else {
        "10.96.0".to_string()
    }
}

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
/// metadata says "superkube" so it's clear what's actually serving.
pub async fn version_info() -> Json<Value> {
    Json(json!({
        "major": "1",
        "minor": "30",
        "gitVersion": format!("v1.30.0-superkube-{}", env!("CARGO_PKG_VERSION")),
        "gitCommit": "superkube",
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

/// /openapi/v2 — kubectl ≥1.27 asks for `Accept: application/com.github.proto-openapi.spec.v2@v1.0+protobuf`
/// (no JSON fallback) and ignores the response Content-Type when parsing.
/// Whatever bytes we return, kubectl runs `proto.Unmarshal` over them.
///
/// An empty body is a valid `gnostic.openapi.v2.Document` (no fields set →
/// all defaults), so `proto.Unmarshal([]byte{}, &Document{})` succeeds and
/// kubectl moves on with "no schemas published" → no client-side validation.
///
/// We also send `Content-Type: application/json` so any client *not* doing
/// the protobuf hot-path treats it as a missing/invalid swagger doc rather
/// than rejecting our content-type header.
pub async fn openapi_v2(_headers: HeaderMap) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::CONTENT_LENGTH, "0")
        .body(axum::body::Body::from(Vec::<u8>::new()))
        .unwrap()
}

/// /openapi/v3: kubectl asks with `Accept: application/json, */*` so plain
/// JSON is fine. Empty paths == "no schemas".
pub async fn openapi_v3(_headers: HeaderMap) -> Response {
    Json(json!({ "paths": {} })).into_response()
}

fn pick_proto_v2(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::ACCEPT)?.to_str().ok()?;
    raw.split(',')
        .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
        .find(|s| s.contains("proto-openapi"))
}

fn empty_proto_response(content_type: &str) -> Response {
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .body(axum::body::Body::from(Vec::<u8>::new()))
        .unwrap();
    if let Ok(value) = axum::http::HeaderValue::from_str(content_type) {
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    resp
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
            },
            {
                "name": "serviceaccounts",
                "singularName": "serviceaccount",
                "namespaced": true,
                "kind": "ServiceAccount",
                "shortNames": ["sa"],
                "verbs": ["create", "delete", "get", "list", "update"]
            },
            {
                "name": "secrets",
                "singularName": "secret",
                "namespaced": true,
                "kind": "Secret",
                "verbs": ["create", "delete", "get", "list", "update"]
            },
            {
                "name": "configmaps",
                "singularName": "configmap",
                "namespaced": true,
                "kind": "ConfigMap",
                "shortNames": ["cm"],
                "verbs": ["create", "delete", "get", "list", "update"]
            },
            {
                "name": "replicationcontrollers",
                "singularName": "replicationcontroller",
                "namespaced": true,
                "kind": "ReplicationController",
                "shortNames": ["rc"],
                "categories": ["all"],
                "verbs": ["get", "list"]
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
                "versions": [{"groupVersion":"apps/v1","version":"v1"}],
                "preferredVersion": {"groupVersion":"apps/v1","version":"v1"}
            },
            {
                "name": "rbac.authorization.k8s.io",
                "versions": [{"groupVersion":"rbac.authorization.k8s.io/v1","version":"v1"}],
                "preferredVersion": {"groupVersion":"rbac.authorization.k8s.io/v1","version":"v1"}
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
            },
            {
                "name": "replicasets",
                "singularName": "replicaset",
                "namespaced": true,
                "kind": "ReplicaSet",
                "shortNames": ["rs"],
                "categories": ["all"],
                "verbs": ["get", "list"]
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
        pod.metadata.namespace = Some(namespace.clone());
    }
    let pod_name = pod.metadata.name.clone().unwrap_or_default();
    tracing::info!("api: create pod {}/{}", namespace, pod_name);
    let created = PodRepository::create(&state.pool, &pod).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut pod): Json<Pod>,
) -> Result<Json<Pod>> {
    tracing::info!("api: update pod {}/{}", namespace, name);
    pod.metadata.namespace = Some(namespace);
    pod.metadata.name = Some(name);
    let updated = PodRepository::create(&state.pool, &pod).await?;
    Ok(Json(updated))
}

pub async fn delete_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete pod {}/{}", namespace, name);
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
        tracing::info!(
            "api: update pod status {}/{} phase={:?}",
            namespace,
            name,
            status.phase
        );
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
    tracing::info!(
        "api: pod logs {}/{} container={:?} follow={} tail={:?}",
        namespace,
        name,
        params.container,
        params.follow,
        params.tail_lines
    );
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
        // Pass the agent's status code through. kubectl renders a 404 as
        // "no logs available" cleanly, instead of a 200 body that says
        // "Failed to get logs from node X: HTTP 404" — which is what
        // ended up inside `kubectl cluster-info dump` files.
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Ok(Response::builder()
            .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY))
            .header(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )
            .body(axum::body::Body::from(body))
            .unwrap());
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

/// Decoded exec query params. kubectl sends each argv entry as its own
/// `command=` parameter, so `command` is a Vec.
#[derive(Debug, Default)]
pub struct ExecParams {
    pub container: Option<String>,
    pub command: Vec<String>,
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub tty: bool,
}

impl ExecParams {
    fn from_query(q: &str) -> Self {
        let pairs: Vec<(String, String)> =
            serde_urlencoded::from_str(q).unwrap_or_default();
        let mut p = ExecParams {
            stdout: true,
            stderr: true,
            ..Default::default()
        };
        for (k, v) in pairs {
            match k.as_str() {
                "container" => p.container = Some(v),
                "command" => p.command.push(v),
                "stdin" => p.stdin = v == "true" || v == "1",
                "stdout" => p.stdout = v == "true" || v == "1",
                "stderr" => p.stderr = v == "true" || v == "1",
                "tty" => p.tty = v == "true" || v == "1",
                _ => {}
            }
        }
        p
    }
}

/// Handle exec requests via WebSocket. We take the raw query string rather
/// than `Query<ExecParams>` because kubectl sends repeated `command=` query
/// params (one per argv entry), and serde_urlencoded into the typed struct
/// only keeps the last one — which then makes `kubectl exec POD -- echo hello`
/// look identical to `kubectl exec POD -- hello`.
pub async fn exec_pod(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    raw_query: axum::extract::RawQuery,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let params = ExecParams::from_query(raw_query.0.as_deref().unwrap_or(""));
    tracing::info!(
        "api: exec pod {}/{} cmd={:?} tty={} container={:?}",
        namespace,
        name,
        params.command,
        params.tty,
        params.container
    );
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

    // Build the upstream URL. Each command word becomes its own `command=`
    // query param so the agent's parser sees them all.
    let mut query_pairs: Vec<(String, String)> = Vec::new();
    if params.command.is_empty() {
        query_pairs.push(("command".to_string(), "sh".to_string()));
    } else {
        for word in &params.command {
            query_pairs.push(("command".to_string(), word.clone()));
        }
    }
    query_pairs.push(("stdin".to_string(), params.stdin.to_string()));
    query_pairs.push(("stdout".to_string(), params.stdout.to_string()));
    query_pairs.push(("stderr".to_string(), params.stderr.to_string()));
    query_pairs.push(("tty".to_string(), params.tty.to_string()));
    let query_string = serde_urlencoded::to_string(&query_pairs).unwrap_or_default();

    let exec_url = format!(
        "ws://{}:10250/exec/{}/{}/{}?{}",
        node_addr, namespace, name, container_name, query_string
    );

    // Negotiate the same WebSocket sub-protocols kubectl asks for. Without
    // this, kubectl's `Sec-WebSocket-Protocol` request goes unanswered and the
    // handshake fails. We forward whichever one we picked to the agent.
    ws.protocols([
        "v5.channel.k8s.io",
        "v4.channel.k8s.io",
        "v3.channel.k8s.io",
        "v2.channel.k8s.io",
        "channel.k8s.io",
    ])
    .on_upgrade(move |socket| {
        let chosen = socket.protocol().and_then(|p| p.to_str().ok()).map(|s| s.to_string());
        handle_exec_websocket(socket, exec_url, chosen)
    })
}

/// Bidirectional WebSocket proxy between kubectl and the node agent. Both
/// frames are binary channel-prefixed (kubernetes streaming protocol); we
/// don't need to inspect them, just shuttle bytes.
async fn handle_exec_websocket(
    client_socket: WebSocket,
    node_url: String,
    subprotocol: Option<String>,
) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut req = match node_url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            let mut s = client_socket;
            let _ = s
                .send(Message::Text(format!("bad node url: {}", e)))
                .await;
            return;
        }
    };
    if let Some(proto) = subprotocol.as_deref() {
        if let Ok(value) = proto.parse() {
            req.headers_mut().insert("Sec-WebSocket-Protocol", value);
        }
    }

    let (node_socket, _) = match connect_async(req).await {
        Ok(conn) => conn,
        Err(e) => {
            let mut s = client_socket;
            let _ = s
                .send(Message::Binary(error_channel_frame(&format!(
                    "failed to connect to node: {}",
                    e
                ))))
                .await;
            let _ = s.close().await;
            return;
        }
    };

    let (mut node_write, mut node_read) = node_socket.split();
    let (mut client_write, mut client_read) = client_socket.split();

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

    tokio::select! {
        _ = client_to_node => {},
        _ = node_to_client => {},
    }
}

fn error_channel_frame(msg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + 1);
    out.push(3u8);
    out.extend_from_slice(msg.as_bytes());
    out
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
    tracing::info!(
        "api: port-forward pod {}/{} ports={:?}",
        namespace,
        name,
        params.ports
    );
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
        service.metadata.namespace = Some(namespace.clone());
    }
    // Auto-assign ClusterIP for any type that should have one. Per the
    // Kubernetes docs, ClusterIP / NodePort / LoadBalancer all get one;
    // only ExternalName services don't.
    let needs_cluster_ip = !matches!(service.spec.service_type, ServiceType::ExternalName);
    if service.spec.cluster_ip.is_none() && needs_cluster_ip {
        let uid = service.metadata.uid.unwrap_or_else(uuid::Uuid::new_v4);
        let octet = (uid.as_u128() % 254 + 1) as u8;
        let prefix = service_cidr_prefix(&state.service_cidr);
        service.spec.cluster_ip = Some(format!("{}.{}", prefix, octet));
    }
    let svc_name = service.metadata.name.clone().unwrap_or_default();
    tracing::info!(
        "api: create service {}/{} type={:?} clusterIP={:?}",
        namespace,
        svc_name,
        service.spec.service_type,
        service.spec.cluster_ip
    );
    let created = ServiceRepository::create(&state.pool, &service).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_service(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut service): Json<Service>,
) -> Result<Json<Service>> {
    tracing::info!("api: update service {}/{}", namespace, name);
    service.metadata.namespace = Some(namespace);
    service.metadata.name = Some(name);
    let updated = ServiceRepository::create(&state.pool, &service).await?;
    Ok(Json(updated))
}

pub async fn delete_service(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete service {}/{}", namespace, name);
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
    tracing::info!(
        "api: register node {}",
        node.metadata.name.clone().unwrap_or_default()
    );
    let created = NodeRepository::create(&state.pool, &node).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_node(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(mut node): Json<Node>,
) -> Result<Json<Node>> {
    tracing::info!("api: update node {}", name);
    node.metadata.name = Some(name);
    let updated = NodeRepository::create(&state.pool, &node).await?;
    Ok(Json(updated))
}

pub async fn delete_node(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete node {}", name);
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
        deployment.metadata.namespace = Some(namespace.clone());
    }
    let dep_name = deployment.metadata.name.clone().unwrap_or_default();
    tracing::info!(
        "api: create deployment {}/{} replicas={}",
        namespace,
        dep_name,
        deployment.spec.replicas.unwrap_or(1)
    );
    let created = DeploymentRepository::create(&state.pool, &deployment).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_deployment(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut deployment): Json<Deployment>,
) -> Result<Json<Deployment>> {
    tracing::info!(
        "api: update deployment {}/{} replicas={}",
        namespace,
        name,
        deployment.spec.replicas.unwrap_or(1)
    );
    deployment.metadata.namespace = Some(namespace);
    deployment.metadata.name = Some(name);
    let updated = DeploymentRepository::create(&state.pool, &deployment).await?;
    Ok(Json(updated))
}

pub async fn delete_deployment(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete deployment {}/{}", namespace, name);
    // First delete all pods owned by this deployment
    let deployment = DeploymentRepository::get(&state.pool, &namespace, &name).await?;
    if let Some(selector) = &deployment.spec.selector.match_labels {
        let pods = PodRepository::list(&state.pool, Some(&namespace), Some(selector)).await?;
        for pod in pods {
            if let Some(pod_name) = pod.metadata.name {
                tracing::info!(
                    "api: cascading delete pod {}/{} owned by deployment {}",
                    namespace,
                    pod_name,
                    name
                );
                let _ = PodRepository::delete(&state.pool, &namespace, &pod_name).await;
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
    tracing::info!(
        "api: create namespace {}",
        namespace.metadata.name.clone().unwrap_or_default()
    );
    let created = NamespaceRepository::create(&state.pool, &namespace).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_namespace(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(mut namespace): Json<Namespace>,
) -> Result<Json<Namespace>> {
    tracing::info!("api: update namespace {}", name);
    namespace.metadata.name = Some(name);
    let updated = NamespaceRepository::create(&state.pool, &namespace).await?;
    Ok(Json(updated))
}

pub async fn delete_namespace(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete namespace {}", name);
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
    tracing::info!("api: delete event {}/{}", namespace, name);
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
        ss.metadata.namespace = Some(namespace.clone());
    }
    tracing::info!(
        "api: create statefulset {}/{} replicas={}",
        namespace,
        ss.metadata.name.clone().unwrap_or_default(),
        ss.spec.replicas.unwrap_or(1)
    );
    let created = StatefulSetRepository::create(&state.pool, &ss).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_statefulset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut ss): Json<StatefulSet>,
) -> Result<Json<StatefulSet>> {
    tracing::info!(
        "api: update statefulset {}/{} replicas={}",
        namespace,
        name,
        ss.spec.replicas.unwrap_or(1)
    );
    ss.metadata.namespace = Some(namespace);
    ss.metadata.name = Some(name);
    Ok(Json(StatefulSetRepository::create(&state.pool, &ss).await?))
}

pub async fn delete_statefulset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete statefulset {}/{}", namespace, name);
    // Best-effort: also delete the owned pods.
    if let Ok(ss) = StatefulSetRepository::get(&state.pool, &namespace, &name).await {
        if let Some(selector) = &ss.spec.selector.match_labels {
            if let Ok(pods) = PodRepository::list(&state.pool, Some(&namespace), Some(selector)).await {
                for pod in pods {
                    if let Some(pod_name) = pod.metadata.name {
                        tracing::info!(
                            "api: cascading delete pod {}/{} owned by statefulset {}",
                            namespace,
                            pod_name,
                            name
                        );
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
        ds.metadata.namespace = Some(namespace.clone());
    }
    tracing::info!(
        "api: create daemonset {}/{}",
        namespace,
        ds.metadata.name.clone().unwrap_or_default()
    );
    let created = DaemonSetRepository::create(&state.pool, &ds).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_daemonset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut ds): Json<DaemonSet>,
) -> Result<Json<DaemonSet>> {
    tracing::info!("api: update daemonset {}/{}", namespace, name);
    ds.metadata.namespace = Some(namespace);
    ds.metadata.name = Some(name);
    Ok(Json(DaemonSetRepository::create(&state.pool, &ds).await?))
}

pub async fn delete_daemonset(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete daemonset {}/{}", namespace, name);
    if let Ok(ds) = DaemonSetRepository::get(&state.pool, &namespace, &name).await {
        if let Some(selector) = &ds.spec.selector.match_labels {
            if let Ok(pods) = PodRepository::list(&state.pool, Some(&namespace), Some(selector)).await {
                for pod in pods {
                    if let Some(pod_name) = pod.metadata.name {
                        tracing::info!(
                            "api: cascading delete pod {}/{} owned by daemonset {}",
                            namespace,
                            pod_name,
                            name
                        );
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

// ============================================================================
// ServiceAccount handlers
// ============================================================================

pub async fn list_service_accounts(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = ServiceAccountRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(
        &headers, "v1", "ServiceAccountList", items,
        printers::SERVICEACCOUNT_COLUMNS, printers::serviceaccount_row,
    ))
}

pub async fn list_all_service_accounts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = ServiceAccountRepository::list(&state.pool, None).await?;
    Ok(table::list_response(
        &headers, "v1", "ServiceAccountList", items,
        printers::SERVICEACCOUNT_COLUMNS, printers::serviceaccount_row,
    ))
}

pub async fn get_service_account(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = ServiceAccountRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(&headers, item, printers::SERVICEACCOUNT_COLUMNS, printers::serviceaccount_row))
}

pub async fn create_service_account(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut sa): Json<ServiceAccount>,
) -> Result<(StatusCode, Json<ServiceAccount>)> {
    if sa.metadata.namespace.is_none() { sa.metadata.namespace = Some(namespace.clone()); }
    tracing::info!(
        "api: create serviceaccount {}/{}",
        namespace,
        sa.metadata.name.clone().unwrap_or_default()
    );
    Ok((StatusCode::CREATED, Json(ServiceAccountRepository::create(&state.pool, &sa).await?)))
}

pub async fn update_service_account(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut sa): Json<ServiceAccount>,
) -> Result<Json<ServiceAccount>> {
    tracing::info!("api: update serviceaccount {}/{}", namespace, name);
    sa.metadata.namespace = Some(namespace);
    sa.metadata.name = Some(name);
    Ok(Json(ServiceAccountRepository::create(&state.pool, &sa).await?))
}

pub async fn delete_service_account(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete serviceaccount {}/{}", namespace, name);
    ServiceAccountRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success",
        "details":{"name": name, "kind":"serviceaccounts"}})))
}

// ============================================================================
// Secret handlers
// ============================================================================

pub async fn list_secrets(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = SecretRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(&headers, "v1", "SecretList", items,
        printers::SECRET_COLUMNS, printers::secret_row))
}

pub async fn list_all_secrets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = SecretRepository::list(&state.pool, None).await?;
    Ok(table::list_response(&headers, "v1", "SecretList", items,
        printers::SECRET_COLUMNS, printers::secret_row))
}

pub async fn get_secret(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = SecretRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(&headers, item, printers::SECRET_COLUMNS, printers::secret_row))
}

pub async fn create_secret(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut s): Json<Secret>,
) -> Result<(StatusCode, Json<Secret>)> {
    if s.metadata.namespace.is_none() { s.metadata.namespace = Some(namespace.clone()); }
    tracing::info!(
        "api: create secret {}/{}",
        namespace,
        s.metadata.name.clone().unwrap_or_default()
    );
    Ok((StatusCode::CREATED, Json(SecretRepository::create(&state.pool, &s).await?)))
}

pub async fn update_secret(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut s): Json<Secret>,
) -> Result<Json<Secret>> {
    tracing::info!("api: update secret {}/{}", namespace, name);
    s.metadata.namespace = Some(namespace);
    s.metadata.name = Some(name);
    Ok(Json(SecretRepository::create(&state.pool, &s).await?))
}

pub async fn delete_secret(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete secret {}/{}", namespace, name);
    SecretRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success",
        "details":{"name": name, "kind":"secrets"}})))
}

// ============================================================================
// ConfigMap handlers
// ============================================================================

pub async fn list_config_maps(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = ConfigMapRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(&headers, "v1", "ConfigMapList", items,
        printers::CONFIGMAP_COLUMNS, printers::configmap_row))
}

pub async fn list_all_config_maps(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = ConfigMapRepository::list(&state.pool, None).await?;
    Ok(table::list_response(&headers, "v1", "ConfigMapList", items,
        printers::CONFIGMAP_COLUMNS, printers::configmap_row))
}

pub async fn get_config_map(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = ConfigMapRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(&headers, item, printers::CONFIGMAP_COLUMNS, printers::configmap_row))
}

pub async fn create_config_map(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut cm): Json<ConfigMap>,
) -> Result<(StatusCode, Json<ConfigMap>)> {
    if cm.metadata.namespace.is_none() { cm.metadata.namespace = Some(namespace.clone()); }
    tracing::info!(
        "api: create configmap {}/{}",
        namespace,
        cm.metadata.name.clone().unwrap_or_default()
    );
    Ok((StatusCode::CREATED, Json(ConfigMapRepository::create(&state.pool, &cm).await?)))
}

pub async fn update_config_map(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut cm): Json<ConfigMap>,
) -> Result<Json<ConfigMap>> {
    tracing::info!("api: update configmap {}/{}", namespace, name);
    cm.metadata.namespace = Some(namespace);
    cm.metadata.name = Some(name);
    Ok(Json(ConfigMapRepository::create(&state.pool, &cm).await?))
}

pub async fn delete_config_map(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete configmap {}/{}", namespace, name);
    ConfigMapRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success",
        "details":{"name": name, "kind":"configmaps"}})))
}

// ============================================================================
// ClusterRole handlers (cluster-scoped, rbac.authorization.k8s.io/v1)
// ============================================================================

pub async fn list_cluster_roles(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = ClusterRoleRepository::list(&state.pool).await?;
    Ok(table::list_response(&headers, "rbac.authorization.k8s.io/v1", "ClusterRoleList", items,
        printers::CLUSTERROLE_COLUMNS, printers::clusterrole_row))
}

pub async fn get_cluster_role(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = ClusterRoleRepository::get(&state.pool, &name).await?;
    Ok(table::item_response(&headers, item, printers::CLUSTERROLE_COLUMNS, printers::clusterrole_row))
}

pub async fn create_cluster_role(
    State(state): State<Arc<AppState>>,
    Json(cr): Json<ClusterRole>,
) -> Result<(StatusCode, Json<ClusterRole>)> {
    tracing::info!(
        "api: create clusterrole {}",
        cr.metadata.name.clone().unwrap_or_default()
    );
    Ok((StatusCode::CREATED, Json(ClusterRoleRepository::create(&state.pool, &cr).await?)))
}

pub async fn update_cluster_role(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(mut cr): Json<ClusterRole>,
) -> Result<Json<ClusterRole>> {
    tracing::info!("api: update clusterrole {}", name);
    cr.metadata.name = Some(name);
    Ok(Json(ClusterRoleRepository::create(&state.pool, &cr).await?))
}

pub async fn delete_cluster_role(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete clusterrole {}", name);
    ClusterRoleRepository::delete(&state.pool, &name).await?;
    Ok(Json(json!({"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success",
        "details":{"name": name, "group":"rbac.authorization.k8s.io", "kind":"clusterroles"}})))
}

// ============================================================================
// ClusterRoleBinding handlers
// ============================================================================

pub async fn list_cluster_role_bindings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = ClusterRoleBindingRepository::list(&state.pool).await?;
    Ok(table::list_response(&headers, "rbac.authorization.k8s.io/v1", "ClusterRoleBindingList", items,
        printers::CLUSTERROLEBINDING_COLUMNS, printers::clusterrolebinding_row))
}

pub async fn get_cluster_role_binding(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = ClusterRoleBindingRepository::get(&state.pool, &name).await?;
    Ok(table::item_response(&headers, item, printers::CLUSTERROLEBINDING_COLUMNS, printers::clusterrolebinding_row))
}

pub async fn create_cluster_role_binding(
    State(state): State<Arc<AppState>>,
    Json(b): Json<ClusterRoleBinding>,
) -> Result<(StatusCode, Json<ClusterRoleBinding>)> {
    tracing::info!(
        "api: create clusterrolebinding {}",
        b.metadata.name.clone().unwrap_or_default()
    );
    Ok((StatusCode::CREATED, Json(ClusterRoleBindingRepository::create(&state.pool, &b).await?)))
}

pub async fn update_cluster_role_binding(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(mut b): Json<ClusterRoleBinding>,
) -> Result<Json<ClusterRoleBinding>> {
    tracing::info!("api: update clusterrolebinding {}", name);
    b.metadata.name = Some(name);
    Ok(Json(ClusterRoleBindingRepository::create(&state.pool, &b).await?))
}

pub async fn delete_cluster_role_binding(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete clusterrolebinding {}", name);
    ClusterRoleBindingRepository::delete(&state.pool, &name).await?;
    Ok(Json(json!({"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success",
        "details":{"name": name, "group":"rbac.authorization.k8s.io", "kind":"clusterrolebindings"}})))
}

// ============================================================================
// rbac.authorization.k8s.io/v1 discovery
// ============================================================================

pub async fn rbac_v1_resources() -> Json<Value> {
    Json(json!({
        "kind": "APIResourceList",
        "groupVersion": "rbac.authorization.k8s.io/v1",
        "resources": [
            {"name":"roles","singularName":"role","namespaced":true,
             "kind":"Role","verbs":["create","delete","get","list","update"]},
            {"name":"rolebindings","singularName":"rolebinding","namespaced":true,
             "kind":"RoleBinding","verbs":["create","delete","get","list","update"]},
            {"name":"clusterroles","singularName":"clusterrole","namespaced":false,
             "kind":"ClusterRole","verbs":["create","delete","get","list","update"]},
            {"name":"clusterrolebindings","singularName":"clusterrolebinding","namespaced":false,
             "kind":"ClusterRoleBinding","verbs":["create","delete","get","list","update"]}
        ]
    }))
}

// ============================================================================
// ReplicationController — stub. We don't actually implement the controller,
// but `kubectl get all` always queries this kind, so we serve empty lists
// (with table format) to avoid 404s.
// ============================================================================

const RC_COLUMNS: &[crate::server::table::Column] = &[
    crate::server::table::Column::new("Name", "string"),
    crate::server::table::Column::new("Desired", "integer"),
    crate::server::table::Column::new("Current", "integer"),
    crate::server::table::Column::new("Ready", "integer"),
    crate::server::table::Column::new("Age", "string"),
];

fn empty_rc_response(headers: &HeaderMap) -> Response {
    let items: Vec<serde_json::Value> = Vec::new();
    table::list_response(
        headers,
        "v1",
        "ReplicationControllerList",
        items,
        RC_COLUMNS,
        |_| Vec::new(),
    )
}

pub async fn list_replication_controllers(
    State(_state): State<Arc<AppState>>,
    Path(_namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    Ok(empty_rc_response(&headers))
}

pub async fn list_all_replication_controllers(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    Ok(empty_rc_response(&headers))
}

// ============================================================================
// ReplicaSet — stub. Like ReplicationController, kubectl always queries this
// kind for `get all`. We don't implement it (Deployments don't materialize
// ReplicaSets in superkube — they own pods directly), but we serve empty
// lists so kubectl is happy.
// ============================================================================

const RS_COLUMNS: &[crate::server::table::Column] = &[
    crate::server::table::Column::new("Name", "string"),
    crate::server::table::Column::new("Desired", "integer"),
    crate::server::table::Column::new("Current", "integer"),
    crate::server::table::Column::new("Ready", "integer"),
    crate::server::table::Column::new("Age", "string"),
];

fn empty_rs_response(headers: &HeaderMap) -> Response {
    let items: Vec<serde_json::Value> = Vec::new();
    table::list_response(
        headers,
        "apps/v1",
        "ReplicaSetList",
        items,
        RS_COLUMNS,
        |_| Vec::new(),
    )
}

pub async fn list_replica_sets(
    State(_state): State<Arc<AppState>>,
    Path(_namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    Ok(empty_rs_response(&headers))
}

pub async fn list_all_replica_sets(
    State(_state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    Ok(empty_rs_response(&headers))
}

// ============================================================================
// Role handlers (namespaced, rbac.authorization.k8s.io/v1)
// ============================================================================

pub async fn list_roles(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = RoleRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(
        &headers, "rbac.authorization.k8s.io/v1", "RoleList", items,
        printers::ROLE_COLUMNS, printers::role_row,
    ))
}

pub async fn list_all_roles(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = RoleRepository::list(&state.pool, None).await?;
    Ok(table::list_response(
        &headers, "rbac.authorization.k8s.io/v1", "RoleList", items,
        printers::ROLE_COLUMNS, printers::role_row,
    ))
}

pub async fn get_role(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = RoleRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(&headers, item, printers::ROLE_COLUMNS, printers::role_row))
}

pub async fn create_role(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut r): Json<Role>,
) -> Result<(StatusCode, Json<Role>)> {
    if r.metadata.namespace.is_none() { r.metadata.namespace = Some(namespace.clone()); }
    tracing::info!(
        "api: create role {}/{}",
        namespace,
        r.metadata.name.clone().unwrap_or_default()
    );
    Ok((StatusCode::CREATED, Json(RoleRepository::create(&state.pool, &r).await?)))
}

pub async fn update_role(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut r): Json<Role>,
) -> Result<Json<Role>> {
    tracing::info!("api: update role {}/{}", namespace, name);
    r.metadata.namespace = Some(namespace);
    r.metadata.name = Some(name);
    Ok(Json(RoleRepository::create(&state.pool, &r).await?))
}

pub async fn delete_role(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete role {}/{}", namespace, name);
    RoleRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success",
        "details":{"name": name, "group":"rbac.authorization.k8s.io", "kind":"roles"}})))
}

// ============================================================================
// RoleBinding handlers (namespaced)
// ============================================================================

pub async fn list_role_bindings(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = RoleBindingRepository::list(&state.pool, Some(&namespace)).await?;
    Ok(table::list_response(
        &headers, "rbac.authorization.k8s.io/v1", "RoleBindingList", items,
        printers::ROLEBINDING_COLUMNS, printers::rolebinding_row,
    ))
}

pub async fn list_all_role_bindings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    let items = RoleBindingRepository::list(&state.pool, None).await?;
    Ok(table::list_response(
        &headers, "rbac.authorization.k8s.io/v1", "RoleBindingList", items,
        printers::ROLEBINDING_COLUMNS, printers::rolebinding_row,
    ))
}

pub async fn get_role_binding(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let item = RoleBindingRepository::get(&state.pool, &namespace, &name).await?;
    Ok(table::item_response(&headers, item, printers::ROLEBINDING_COLUMNS, printers::rolebinding_row))
}

pub async fn create_role_binding(
    State(state): State<Arc<AppState>>,
    Path(namespace): Path<String>,
    Json(mut b): Json<RoleBinding>,
) -> Result<(StatusCode, Json<RoleBinding>)> {
    if b.metadata.namespace.is_none() { b.metadata.namespace = Some(namespace.clone()); }
    tracing::info!(
        "api: create rolebinding {}/{}",
        namespace,
        b.metadata.name.clone().unwrap_or_default()
    );
    Ok((StatusCode::CREATED, Json(RoleBindingRepository::create(&state.pool, &b).await?)))
}

pub async fn update_role_binding(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(mut b): Json<RoleBinding>,
) -> Result<Json<RoleBinding>> {
    tracing::info!("api: update rolebinding {}/{}", namespace, name);
    b.metadata.namespace = Some(namespace);
    b.metadata.name = Some(name);
    Ok(Json(RoleBindingRepository::create(&state.pool, &b).await?))
}

pub async fn delete_role_binding(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    tracing::info!("api: delete rolebinding {}/{}", namespace, name);
    RoleBindingRepository::delete(&state.pool, &namespace, &name).await?;
    Ok(Json(json!({"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success",
        "details":{"name": name, "group":"rbac.authorization.k8s.io", "kind":"rolebindings"}})))
}
