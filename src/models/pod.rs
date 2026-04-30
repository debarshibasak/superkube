use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::{ObjectMeta, TypeMeta};

/// Pod is a collection of containers that run on a node
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: PodSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PodStatus>,
}

impl Default for Pod {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Pod".to_string()),
            },
            metadata: ObjectMeta::default(),
            spec: PodSpec::default(),
            status: None,
        }
    }
}

/// PodSpec describes the desired state of a pod
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodSpec {
    /// List of containers in the pod
    #[serde(default)]
    pub containers: Vec<Container>,

    /// Restart policy for all containers (Always, OnFailure, Never)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,

    /// Node selector for scheduling
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<HashMap<String, String>>,

    /// Name of the node to schedule on (set by scheduler)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,

    /// Service account name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account_name: Option<String>,

    /// Host networking mode
    #[serde(default)]
    pub host_network: bool,

    /// Scheduling preferences (node + pod affinity/anti-affinity).
    /// `requiredDuringSchedulingIgnoredDuringExecution` terms are honoured
    /// during pod placement; preferred terms are stored but not yet weighted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<super::Affinity>,
}

/// Container describes a single container in a pod
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    /// Name of the container
    pub name: String,

    /// Docker image name
    pub image: String,

    /// Image pull policy (Always, IfNotPresent, Never)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<ImagePullPolicy>,

    /// Entrypoint array (replaces Docker ENTRYPOINT)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,

    /// Arguments to the entrypoint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    /// Environment variables
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<EnvVar>>,

    /// Exposed ports
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<ContainerPort>>,

    /// Resource requirements
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,

    /// Working directory
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum RestartPolicy {
    #[default]
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum ImagePullPolicy {
    Always,
    #[default]
    IfNotPresent,
    Never,
}

/// EnvVar represents an environment variable
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// ContainerPort represents a network port in a container
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerPort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub container_port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum Protocol {
    #[default]
    TCP,
    UDP,
}

/// ResourceRequirements for CPU/memory
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<HashMap<String, String>>,
}

/// PodStatus represents the observed state of a pod
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodStatus {
    /// Current phase of the pod (Pending, Running, Succeeded, Failed, Unknown)
    #[serde(default)]
    pub phase: PodPhase,

    /// Conditions of the pod
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<PodCondition>>,

    /// IP address of the pod. Kubernetes spells it `podIP` on the wire (not
    /// `podIp`); kubectl's jsonpath / printers all key off that exact spelling.
    #[serde(default, rename = "podIP", skip_serializing_if = "Option::is_none")]
    pub pod_ip: Option<String>,

    /// IP addresses of the pod (for dual-stack).
    #[serde(default, rename = "podIPs", skip_serializing_if = "Option::is_none")]
    pub pod_i_ps: Option<Vec<PodIP>>,

    /// IP address of the host node.
    #[serde(default, rename = "hostIP", skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,

    /// Time when the pod was started
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<DateTime<Utc>>,

    /// Status of each container
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_statuses: Option<Vec<ContainerStatus>>,

    /// Human-readable message
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Brief reason for the pod's current status
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum PodPhase {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

impl std::fmt::Display for PodPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PodPhase::Pending => write!(f, "Pending"),
            PodPhase::Running => write!(f, "Running"),
            PodPhase::Succeeded => write!(f, "Succeeded"),
            PodPhase::Failed => write!(f, "Failed"),
            PodPhase::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodIP {
    pub ip: String,
}

/// ContainerStatus contains status of a container
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStatus {
    pub name: String,
    pub ready: bool,
    pub restart_count: i32,
    pub image: String,
    /// k8s spells this `imageID` on the wire (not `imageId`).
    #[serde(rename = "imageID")]
    pub image_id: String,
    /// k8s spells this `containerID` on the wire.
    #[serde(default, rename = "containerID", skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ContainerState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_state: Option<ContainerState>,
    pub started: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ContainerState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<ContainerStateWaiting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<ContainerStateRunning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminated: Option<ContainerStateTerminated>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStateWaiting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStateRunning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStateTerminated {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, rename = "containerID", skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
}

/// PodTemplateSpec for use in Deployments
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodTemplateSpec {
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: PodSpec,
}
