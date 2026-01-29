use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{LabelSelector, ObjectMeta, PodTemplateSpec, TypeMeta};

/// Deployment provides declarative updates for Pods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: DeploymentSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DeploymentStatus>,
}

impl Default for Deployment {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("apps/v1".to_string()),
                kind: Some("Deployment".to_string()),
            },
            metadata: ObjectMeta::default(),
            spec: DeploymentSpec::default(),
            status: None,
        }
    }
}

/// DeploymentSpec describes the desired state of a Deployment
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentSpec {
    /// Number of desired pods
    #[serde(default)]
    pub replicas: Option<i32>,

    /// Label selector for pods
    #[serde(default)]
    pub selector: LabelSelector,

    /// Template for creating pods
    #[serde(default)]
    pub template: PodTemplateSpec,

    /// Update strategy
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<DeploymentStrategy>,

    /// Minimum seconds a pod must be ready before considered available
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ready_seconds: Option<i32>,

    /// Number of old ReplicaSets to retain
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_history_limit: Option<i32>,

    /// Indicates deployment is paused
    #[serde(default)]
    pub paused: bool,
}

impl DeploymentSpec {
    pub fn replicas(&self) -> i32 {
        self.replicas.unwrap_or(1)
    }
}

/// DeploymentStrategy describes how to replace existing pods
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStrategy {
    /// Type of deployment (RollingUpdate, Recreate)
    #[serde(rename = "type", default)]
    pub strategy_type: DeploymentStrategyType,

    /// Rolling update config
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_update: Option<RollingUpdateDeployment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum DeploymentStrategyType {
    #[default]
    RollingUpdate,
    Recreate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollingUpdateDeployment {
    /// Max pods unavailable during update
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unavailable: Option<IntOrString>,

    /// Max pods that can be created over desired replicas
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_surge: Option<IntOrString>,
}

/// IntOrString can be an integer or a percentage string
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IntOrString {
    Int(i32),
    String(String),
}

/// DeploymentStatus represents the observed state of a Deployment
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatus {
    /// Total number of non-terminated pods
    #[serde(default)]
    pub replicas: i32,

    /// Total number of non-terminated pods with desired template
    #[serde(default)]
    pub updated_replicas: i32,

    /// Total number of ready pods
    #[serde(default)]
    pub ready_replicas: i32,

    /// Total number of available pods
    #[serde(default)]
    pub available_replicas: i32,

    /// Total number of unavailable pods
    #[serde(default)]
    pub unavailable_replicas: i32,

    /// Most recent generation observed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// Conditions of the deployment
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<DeploymentCondition>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
