use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// TypeMeta describes the API version and kind of an object
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TypeMeta {
    /// API version of the object (e.g., "v1", "apps/v1")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,

    /// Kind of the object (e.g., "Pod", "Deployment")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// ObjectMeta contains metadata that all persisted resources must have
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMeta {
    /// Name of the object, unique within namespace
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Namespace of the object
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    /// Unique identifier for the object
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<Uuid>,

    /// Resource version for optimistic concurrency control
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<String>,

    /// Labels for organizing and selecting objects
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,

    /// Annotations for storing non-identifying metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<HashMap<String, String>>,

    /// Timestamp when the object was created
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_timestamp: Option<DateTime<Utc>>,

    /// References to the owner objects
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_references: Option<Vec<OwnerReference>>,
}

impl ObjectMeta {
    pub fn new(name: &str, namespace: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            uid: Some(Uuid::new_v4()),
            creation_timestamp: Some(Utc::now()),
            ..Default::default()
        }
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("")
    }

    pub fn namespace(&self) -> &str {
        self.namespace.as_deref().unwrap_or("default")
    }

    pub fn labels(&self) -> HashMap<String, String> {
        self.labels.clone().unwrap_or_default()
    }
}

/// OwnerReference contains information about the owning object
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerReference {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: Uuid,
    #[serde(default)]
    pub controller: Option<bool>,
    #[serde(default)]
    pub block_owner_deletion: Option<bool>,
}

/// LabelSelector for selecting objects by labels
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_labels: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_expressions: Option<Vec<LabelSelectorRequirement>>,
}

impl LabelSelector {
    pub fn matches(&self, labels: &HashMap<String, String>) -> bool {
        // Check matchLabels
        if let Some(match_labels) = &self.match_labels {
            for (key, value) in match_labels {
                if labels.get(key) != Some(value) {
                    return false;
                }
            }
        }
        true
    }
}

/// LabelSelectorRequirement for complex label matching
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelectorRequirement {
    pub key: String,
    pub operator: String, // In, NotIn, Exists, DoesNotExist
    #[serde(default)]
    pub values: Option<Vec<String>>,
}

/// K8s-style list response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct List<T> {
    pub api_version: String,
    pub kind: String,
    pub metadata: ListMeta,
    pub items: Vec<T>,
}

impl<T> List<T> {
    pub fn new(api_version: &str, kind: &str, items: Vec<T>) -> Self {
        Self {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            metadata: ListMeta::default(),
            items,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_token: Option<String>,
}
