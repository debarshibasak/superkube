use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::{ObjectMeta, Protocol, TypeMeta};

/// Service is a named abstraction of software service
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: ServiceSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ServiceStatus>,
}

impl Default for Service {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Service".to_string()),
            },
            metadata: ObjectMeta::default(),
            spec: ServiceSpec::default(),
            status: None,
        }
    }
}

/// ServiceSpec describes the desired state of a Service
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    /// Label selector to find target pods
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<HashMap<String, String>>,

    /// List of ports exposed by the service
    #[serde(default)]
    pub ports: Vec<ServicePort>,

    /// Type of service (ClusterIP, NodePort, LoadBalancer)
    #[serde(rename = "type", default)]
    pub service_type: ServiceType,

    /// ClusterIP — virtual IP address. k8s spells this `clusterIP`.
    #[serde(default, rename = "clusterIP", skip_serializing_if = "Option::is_none")]
    pub cluster_ip: Option<String>,

    /// External IPs for the service. k8s spells this `externalIPs`.
    #[serde(default, rename = "externalIPs", skip_serializing_if = "Option::is_none")]
    pub external_i_ps: Option<Vec<String>>,

    /// Session affinity (None, ClientIP)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_affinity: Option<SessionAffinity>,
}

/// ServicePort defines a port exposed by the service
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    /// Name of the port
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Protocol (TCP/UDP)
    #[serde(default)]
    pub protocol: Protocol,

    /// Port exposed by the service
    pub port: i32,

    /// Target port on the pod (can be number or name)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<IntOrString>,

    /// NodePort (for NodePort/LoadBalancer services)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_port: Option<i32>,
}

// Re-use IntOrString from deployment module
pub use super::deployment::IntOrString;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum ServiceType {
    #[default]
    ClusterIP,
    NodePort,
    LoadBalancer,
    ExternalName,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum SessionAffinity {
    #[default]
    None,
    ClientIP,
}

/// ServiceStatus represents the current state of a service
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_balancer: Option<LoadBalancerStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancerStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<Vec<LoadBalancerIngress>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancerIngress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// Endpoints is a collection of endpoints for a service
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoints {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub subsets: Vec<EndpointSubset>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Endpoints".to_string()),
            },
            metadata: ObjectMeta::default(),
            subsets: Vec::new(),
        }
    }
}

/// EndpointSubset is a group of addresses with a common set of ports
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSubset {
    /// Ready addresses
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<EndpointAddress>>,

    /// Not ready addresses
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_ready_addresses: Option<Vec<EndpointAddress>>,

    /// Ports available on the endpoints
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<EndpointPort>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointAddress {
    pub ip: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<ObjectReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ObjectReference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointPort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub port: i32,
    #[serde(default)]
    pub protocol: Protocol,
}
