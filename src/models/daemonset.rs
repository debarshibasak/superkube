use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{LabelSelector, ObjectMeta, PodTemplateSpec, TypeMeta};

/// DaemonSet ensures one pod from `template` runs on every node that matches
/// the node selector / template's `nodeSelector`. As nodes join, the controller
/// creates a pod on each; as nodes leave, the orphaned pods are removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSet {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub spec: DaemonSetSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DaemonSetStatus>,
}

impl Default for DaemonSet {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("apps/v1".to_string()),
                kind: Some("DaemonSet".to_string()),
            },
            metadata: ObjectMeta::default(),
            spec: DaemonSetSpec::default(),
            status: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSetSpec {
    #[serde(default)]
    pub selector: LabelSelector,

    #[serde(default)]
    pub template: PodTemplateSpec,

    /// Min seconds a pod must be ready before considered available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_ready_seconds: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSetStatus {
    /// Total nodes that should run the daemon (matched the selector).
    #[serde(default)]
    pub desired_number_scheduled: i32,
    /// Nodes that are running at least one daemon pod.
    #[serde(default)]
    pub current_number_scheduled: i32,
    /// Nodes that have at least one daemon pod with `Ready=True`.
    #[serde(default)]
    pub number_ready: i32,
    /// Nodes running the latest pod template — same as current for now since
    /// we don't track pod-template revisions yet.
    #[serde(default)]
    pub updated_number_scheduled: i32,
    /// Nodes that *should not* be running the daemon but are.
    #[serde(default)]
    pub number_misscheduled: i32,
    /// Total daemon pods that are available.
    #[serde(default)]
    pub number_available: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<DateTime<Utc>>,
}
