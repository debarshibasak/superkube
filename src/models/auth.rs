//! Auth-shaped resources: ServiceAccount, Secret, ConfigMap, ClusterRole,
//! ClusterRoleBinding. Stored only — we don't yet enforce RBAC.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{ObjectMeta, TypeMeta};

// --- ServiceAccount ----------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccount {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<ObjectReferenceLite>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_secrets: Option<Vec<ObjectReferenceLite>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automount_service_account_token: Option<bool>,
}

impl Default for ServiceAccount {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".into()),
                kind: Some("ServiceAccount".into()),
            },
            metadata: ObjectMeta::default(),
            secrets: None,
            image_pull_secrets: None,
            automount_service_account_token: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ObjectReferenceLite {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// --- Secret ------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// Secret type: "Opaque", "kubernetes.io/service-account-token", etc.
    #[serde(default = "default_secret_type", rename = "type")]
    pub secret_type: String,
    /// Base64-encoded values keyed by name (Kubernetes encodes on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<HashMap<String, String>>,
    /// Plain-text values (decoded by the API server when reading).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub string_data: Option<HashMap<String, String>>,
    #[serde(default)]
    pub immutable: Option<bool>,
}

fn default_secret_type() -> String {
    "Opaque".to_string()
}

impl Default for Secret {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".into()),
                kind: Some("Secret".into()),
            },
            metadata: ObjectMeta::default(),
            secret_type: default_secret_type(),
            data: None,
            string_data: None,
            immutable: None,
        }
    }
}

// --- ConfigMap ---------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMap {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_data: Option<HashMap<String, String>>,
    #[serde(default)]
    pub immutable: Option<bool>,
}

impl Default for ConfigMap {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".into()),
                kind: Some("ConfigMap".into()),
            },
            metadata: ObjectMeta::default(),
            data: None,
            binary_data: None,
            immutable: None,
        }
    }
}

// --- ClusterRole / ClusterRoleBinding ----------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRole {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregation_rule: Option<serde_json::Value>,
}

impl Default for ClusterRole {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("rbac.authorization.k8s.io/v1".into()),
                kind: Some("ClusterRole".into()),
            },
            metadata: ObjectMeta::default(),
            rules: Vec::new(),
            aggregation_rule: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRule {
    #[serde(default)]
    pub verbs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_resource_ur_ls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRoleBinding {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub subjects: Vec<RbacSubject>,
    #[serde(default)]
    pub role_ref: RoleRef,
}

impl Default for ClusterRoleBinding {
    fn default() -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("rbac.authorization.k8s.io/v1".into()),
                kind: Some("ClusterRoleBinding".into()),
            },
            metadata: ObjectMeta::default(),
            subjects: Vec::new(),
            role_ref: RoleRef::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RbacSubject {
    pub kind: String,
    #[serde(default)]
    pub api_group: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RoleRef {
    #[serde(default)]
    pub api_group: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
}
