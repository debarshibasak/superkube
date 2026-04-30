use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{LabelSelector, ObjectMeta, PodTemplateSpec, TypeMeta};

/// StatefulSet manages a set of pods with stable identities (`<name>-0`,
/// `<name>-1`, ...). Pods are created in order and deleted in reverse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSet {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: StatefulSetSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatefulSetStatus>,
}

impl Default for StatefulSet {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("apps/v1".to_string()),
                kind: Some("StatefulSet".to_string()),
            },
            metadata: ObjectMeta::default(),
            spec: StatefulSetSpec::default(),
            status: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSetSpec {
    #[serde(default)]
    pub replicas: Option<i32>,

    #[serde(default)]
    pub selector: LabelSelector,

    #[serde(default)]
    pub template: PodTemplateSpec,

    /// Name of the governing headless service. Optional in our impl —
    /// we only use it for the pod hostname, if at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,

    /// OrderedReady (default) or Parallel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_management_policy: Option<PodManagementPolicy>,
}

impl StatefulSetSpec {
    pub fn replicas(&self) -> i32 {
        self.replicas.unwrap_or(1)
    }

    pub fn is_parallel(&self) -> bool {
        matches!(self.pod_management_policy, Some(PodManagementPolicy::Parallel))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum PodManagementPolicy {
    #[default]
    OrderedReady,
    Parallel,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSetStatus {
    #[serde(default)]
    pub replicas: i32,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default)]
    pub current_replicas: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<DateTime<Utc>>,
}
