use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::{ObjectMeta, TypeMeta};

/// Node is a worker machine in the cluster
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<NodeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<NodeStatus>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Node".to_string()),
            },
            metadata: ObjectMeta::default(),
            spec: None,
            status: None,
        }
    }
}

/// NodeSpec describes the attributes of a node
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeSpec {
    /// Pod CIDR assigned to this node
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_cidr: Option<String>,

    /// Provider-specific node ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,

    /// If set, pods won't be scheduled on this node
    #[serde(default)]
    pub unschedulable: bool,

    /// Taints for the node
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taints: Option<Vec<Taint>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Taint {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub effect: TaintEffect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_added: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaintEffect {
    NoSchedule,
    PreferNoSchedule,
    NoExecute,
}

/// NodeStatus represents the observed state of a Node
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    /// Capacity represents the total resources of a node
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<HashMap<String, String>>,

    /// Allocatable represents schedulable resources
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocatable: Option<HashMap<String, String>>,

    /// Conditions for the node
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<NodeCondition>>,

    /// Addresses of the node
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<NodeAddress>>,

    /// Node system info
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_info: Option<NodeSystemInfo>,

    /// List of container images on the node
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ContainerImage>>,

    /// Current phase of the node
    #[serde(default)]
    pub phase: NodePhase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeCondition {
    #[serde(rename = "type")]
    pub condition_type: NodeConditionType,
    pub status: ConditionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NodeConditionType {
    Ready,
    MemoryPressure,
    DiskPressure,
    PIDPressure,
    NetworkUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeAddress {
    #[serde(rename = "type")]
    pub address_type: NodeAddressType,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeAddressType {
    Hostname,
    InternalIP,
    ExternalIP,
    InternalDNS,
    ExternalDNS,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeSystemInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_runtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubelet_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube_proxy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operating_system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerImage {
    pub names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum NodePhase {
    #[default]
    Pending,
    Running,
    Terminated,
}
