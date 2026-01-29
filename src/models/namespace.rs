use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ObjectMeta, TypeMeta};

/// Namespace provides a scope for Names
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Namespace {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<NamespaceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<NamespaceStatus>,
}

impl Default for Namespace {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Namespace".to_string()),
            },
            metadata: ObjectMeta::default(),
            spec: None,
            status: None,
        }
    }
}

/// NamespaceSpec describes the attributes of a namespace
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceSpec {
    /// Finalizers is an opaque list of values that must be empty to permanently remove object
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalizers: Option<Vec<String>>,
}

/// NamespaceStatus represents the current status of a namespace
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceStatus {
    /// Phase is the current lifecycle phase of the namespace
    #[serde(default)]
    pub phase: NamespacePhase,

    /// Conditions of the namespace
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<NamespaceCondition>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum NamespacePhase {
    #[default]
    Active,
    Terminating,
}

impl std::fmt::Display for NamespacePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamespacePhase::Active => write!(f, "Active"),
            NamespacePhase::Terminating => write!(f, "Terminating"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
