//! kubectl-shaped column definitions + row builders for every kind we expose.
//! Used by handlers in `api.rs` together with `table::list_response`.

use serde_json::{json, Value};

use crate::models::*;

use super::table::{age_str, Column};

// --- Pod ---------------------------------------------------------------

pub const POD_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Ready", "string"),
    Column::new("Status", "string"),
    Column::new("Restarts", "integer"),
    Column::new("Age", "string"),
    Column::wide("IP", "string"),
    Column::wide("Node", "string"),
    Column::wide("Nominated Node", "string"),
    Column::wide("Readiness Gates", "string"),
];

pub fn pod_row(p: &Pod) -> Vec<Value> {
    let cs = p.status.as_ref().and_then(|s| s.container_statuses.as_ref());
    let total = p.spec.containers.len();
    let ready = cs.map(|cs| cs.iter().filter(|c| c.ready).count()).unwrap_or(0);
    let restarts: i32 = cs
        .map(|cs| cs.iter().map(|c| c.restart_count).sum())
        .unwrap_or(0);
    let phase = p
        .status
        .as_ref()
        .map(|s| s.phase.to_string())
        .unwrap_or_else(|| "Pending".to_string());
    let pod_ip = p
        .status
        .as_ref()
        .and_then(|s| s.pod_ip.clone())
        .unwrap_or_else(|| "<none>".to_string());
    let node = p
        .spec
        .node_name
        .clone()
        .unwrap_or_else(|| "<none>".to_string());

    vec![
        json!(p.metadata.name()),
        json!(format!("{}/{}", ready, total)),
        json!(phase),
        json!(restarts),
        json!(age_str(p.metadata.creation_timestamp)),
        json!(pod_ip),
        json!(node),
        json!("<none>"),
        json!("<none>"),
    ]
}

// --- Node --------------------------------------------------------------

pub const NODE_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Status", "string"),
    Column::new("Roles", "string"),
    Column::new("Age", "string"),
    Column::new("Version", "string"),
    Column::wide("Internal-IP", "string"),
    Column::wide("External-IP", "string"),
    Column::wide("OS-Image", "string"),
    Column::wide("Kernel-Version", "string"),
    Column::wide("Container-Runtime", "string"),
];

pub fn node_row(n: &Node) -> Vec<Value> {
    let ready = n
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|c| c.iter().find(|c| c.condition_type == NodeConditionType::Ready))
        .map(|c| c.status == ConditionStatus::True)
        .unwrap_or(false);
    let status = if ready { "Ready" } else { "NotReady" };

    let roles = n
        .metadata
        .labels
        .as_ref()
        .map(|l| {
            let collected: Vec<&str> = l
                .keys()
                .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                .collect();
            if collected.is_empty() {
                "<none>".to_string()
            } else {
                collected.join(",")
            }
        })
        .unwrap_or_else(|| "<none>".to_string());

    let info = n.status.as_ref().and_then(|s| s.node_info.as_ref());
    let version = info
        .and_then(|i| i.kubelet_version.clone())
        .unwrap_or_else(|| "<unknown>".to_string());

    let addresses = n.status.as_ref().and_then(|s| s.addresses.as_ref());
    let internal_ip = addresses
        .and_then(|a| a.iter().find(|a| a.address_type == NodeAddressType::InternalIP))
        .map(|a| a.address.clone())
        .unwrap_or_else(|| "<none>".to_string());
    let external_ip = addresses
        .and_then(|a| a.iter().find(|a| a.address_type == NodeAddressType::ExternalIP))
        .map(|a| a.address.clone())
        .unwrap_or_else(|| "<none>".to_string());

    let os_image = info
        .and_then(|i| i.os_image.clone())
        .unwrap_or_else(|| "<unknown>".to_string());
    let kernel = info
        .and_then(|i| i.kernel_version.clone())
        .unwrap_or_else(|| "<unknown>".to_string());
    let runtime = info
        .and_then(|i| i.container_runtime_version.clone())
        .unwrap_or_else(|| "<unknown>".to_string());

    vec![
        json!(n.metadata.name()),
        json!(status),
        json!(roles),
        json!(age_str(n.metadata.creation_timestamp)),
        json!(version),
        json!(internal_ip),
        json!(external_ip),
        json!(os_image),
        json!(kernel),
        json!(runtime),
    ]
}

// --- Deployment --------------------------------------------------------

pub const DEPLOYMENT_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Ready", "string"),
    Column::new("Up-to-date", "integer"),
    Column::new("Available", "integer"),
    Column::new("Age", "string"),
];

pub fn deployment_row(d: &Deployment) -> Vec<Value> {
    let s = d.status.as_ref();
    let ready = s.map(|s| s.ready_replicas).unwrap_or(0);
    let updated = s.map(|s| s.updated_replicas).unwrap_or(0);
    let available = s.map(|s| s.available_replicas).unwrap_or(0);
    let desired = d.spec.replicas();
    vec![
        json!(d.metadata.name()),
        json!(format!("{}/{}", ready, desired)),
        json!(updated),
        json!(available),
        json!(age_str(d.metadata.creation_timestamp)),
    ]
}

// --- StatefulSet -------------------------------------------------------

pub const STATEFULSET_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Ready", "string"),
    Column::new("Age", "string"),
];

pub fn statefulset_row(s: &StatefulSet) -> Vec<Value> {
    let ready = s.status.as_ref().map(|s| s.ready_replicas).unwrap_or(0);
    let desired = s.spec.replicas();
    vec![
        json!(s.metadata.name()),
        json!(format!("{}/{}", ready, desired)),
        json!(age_str(s.metadata.creation_timestamp)),
    ]
}

// --- DaemonSet ---------------------------------------------------------

pub const DAEMONSET_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Desired", "integer"),
    Column::new("Current", "integer"),
    Column::new("Ready", "integer"),
    Column::new("Up-to-date", "integer"),
    Column::new("Available", "integer"),
    Column::new("Node Selector", "string"),
    Column::new("Age", "string"),
];

pub fn daemonset_row(d: &DaemonSet) -> Vec<Value> {
    let s = d.status.as_ref();
    let desired = s.map(|s| s.desired_number_scheduled).unwrap_or(0);
    let current = s.map(|s| s.current_number_scheduled).unwrap_or(0);
    let ready = s.map(|s| s.number_ready).unwrap_or(0);
    let updated = s.map(|s| s.updated_number_scheduled).unwrap_or(0);
    let available = s.map(|s| s.number_available).unwrap_or(0);

    let selector = d
        .spec
        .template
        .spec
        .node_selector
        .as_ref()
        .map(|m| {
            let mut entries: Vec<String> =
                m.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            entries.sort();
            entries.join(",")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<none>".to_string());

    vec![
        json!(d.metadata.name()),
        json!(desired),
        json!(current),
        json!(ready),
        json!(updated),
        json!(available),
        json!(selector),
        json!(age_str(d.metadata.creation_timestamp)),
    ]
}

// --- Service -----------------------------------------------------------

pub const SERVICE_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Type", "string"),
    Column::new("Cluster-IP", "string"),
    Column::new("External-IP", "string"),
    Column::new("Ports", "string"),
    Column::new("Age", "string"),
    Column::wide("Selector", "string"),
];

pub fn service_row(s: &Service) -> Vec<Value> {
    let svc_type = match s.spec.service_type {
        ServiceType::ClusterIP => "ClusterIP",
        ServiceType::NodePort => "NodePort",
        ServiceType::LoadBalancer => "LoadBalancer",
        ServiceType::ExternalName => "ExternalName",
    };
    let cluster_ip = s
        .spec
        .cluster_ip
        .clone()
        .unwrap_or_else(|| "<none>".to_string());
    let external_ip = match s.spec.service_type {
        ServiceType::LoadBalancer => "<pending>".to_string(),
        _ => "<none>".to_string(),
    };
    let ports = s
        .spec
        .ports
        .iter()
        .map(|p| {
            let proto = match p.protocol {
                Protocol::UDP => "UDP",
                Protocol::TCP => "TCP",
            };
            match p.node_port {
                Some(np) => format!("{}:{}/{}", p.port, np, proto),
                None => format!("{}/{}", p.port, proto),
            }
        })
        .collect::<Vec<_>>()
        .join(",");

    let selector = s
        .spec
        .selector
        .as_ref()
        .map(|m| {
            let mut entries: Vec<String> =
                m.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            entries.sort();
            entries.join(",")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<none>".to_string());

    vec![
        json!(s.metadata.name()),
        json!(svc_type),
        json!(cluster_ip),
        json!(external_ip),
        json!(if ports.is_empty() { "<none>".to_string() } else { ports }),
        json!(age_str(s.metadata.creation_timestamp)),
        json!(selector),
    ]
}

// --- Namespace ---------------------------------------------------------

pub const NAMESPACE_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Status", "string"),
    Column::new("Age", "string"),
];

pub fn namespace_row(n: &Namespace) -> Vec<Value> {
    let phase = n
        .status
        .as_ref()
        .map(|s| match s.phase {
            NamespacePhase::Active => "Active",
            NamespacePhase::Terminating => "Terminating",
        })
        .unwrap_or("Active");
    vec![
        json!(n.metadata.name()),
        json!(phase),
        json!(age_str(n.metadata.creation_timestamp)),
    ]
}

// --- Event -------------------------------------------------------------

pub const EVENT_COLUMNS: &[Column] = &[
    Column::new("Last Seen", "string"),
    Column::new("Type", "string"),
    Column::new("Reason", "string"),
    Column::new("Object", "string"),
    Column::new("Message", "string"),
];

pub fn event_row(e: &Event) -> Vec<Value> {
    let last = e
        .last_timestamp
        .or(e.first_timestamp)
        .or(e.event_time);
    let last_seen = age_str(last);
    let typ = e
        .event_type
        .as_ref()
        .map(|t| match t {
            EventType::Normal => "Normal",
            EventType::Warning => "Warning",
        })
        .unwrap_or("Normal");
    let reason = e.reason.clone().unwrap_or_default();
    let object = format!(
        "{}/{}",
        e.involved_object.kind.clone().unwrap_or_default().to_lowercase(),
        e.involved_object.name.clone().unwrap_or_default()
    );
    let message = e.message.clone().unwrap_or_default();
    vec![
        json!(last_seen),
        json!(typ),
        json!(reason),
        json!(object),
        json!(message),
    ]
}

// --- ServiceAccount ----------------------------------------------------

pub const SERVICEACCOUNT_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Secrets", "integer"),
    Column::new("Age", "string"),
];

pub fn serviceaccount_row(sa: &ServiceAccount) -> Vec<Value> {
    let secrets = sa.secrets.as_ref().map(|s| s.len()).unwrap_or(0);
    vec![
        json!(sa.metadata.name()),
        json!(secrets),
        json!(age_str(sa.metadata.creation_timestamp)),
    ]
}

// --- Secret ------------------------------------------------------------

pub const SECRET_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Type", "string"),
    Column::new("Data", "integer"),
    Column::new("Age", "string"),
];

pub fn secret_row(s: &Secret) -> Vec<Value> {
    let n = s.data.as_ref().map(|m| m.len()).unwrap_or(0)
        + s.string_data.as_ref().map(|m| m.len()).unwrap_or(0);
    vec![
        json!(s.metadata.name()),
        json!(s.secret_type),
        json!(n),
        json!(age_str(s.metadata.creation_timestamp)),
    ]
}

// --- ConfigMap ---------------------------------------------------------

pub const CONFIGMAP_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Data", "integer"),
    Column::new("Age", "string"),
];

pub fn configmap_row(cm: &ConfigMap) -> Vec<Value> {
    let n = cm.data.as_ref().map(|m| m.len()).unwrap_or(0)
        + cm.binary_data.as_ref().map(|m| m.len()).unwrap_or(0);
    vec![
        json!(cm.metadata.name()),
        json!(n),
        json!(age_str(cm.metadata.creation_timestamp)),
    ]
}

// --- ClusterRole / ClusterRoleBinding ----------------------------------

pub const CLUSTERROLE_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Created At", "string"),
];

pub fn clusterrole_row(cr: &ClusterRole) -> Vec<Value> {
    let created = cr
        .metadata
        .creation_timestamp
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "<unknown>".to_string());
    vec![json!(cr.metadata.name()), json!(created)]
}

pub const CLUSTERROLEBINDING_COLUMNS: &[Column] = &[
    Column::new("Name", "string"),
    Column::new("Role", "string"),
    Column::new("Age", "string"),
];

pub fn clusterrolebinding_row(b: &ClusterRoleBinding) -> Vec<Value> {
    let role = format!("{}/{}", b.role_ref.kind, b.role_ref.name);
    vec![
        json!(b.metadata.name()),
        json!(role),
        json!(age_str(b.metadata.creation_timestamp)),
    ]
}
