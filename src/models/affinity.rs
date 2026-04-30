//! Pod scheduling preferences — node affinity, pod (anti-)affinity.
//!
//! We currently honour the `requiredDuringSchedulingIgnoredDuringExecution`
//! variant of each affinity type during scheduling. Preferred-during-scheduling
//! is parsed and stored, but not yet weighted into placement decisions.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Affinity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_affinity: Option<NodeAffinity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_affinity: Option<PodAffinity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_anti_affinity: Option<PodAntiAffinity>,
}

// ----- Node affinity ---------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeAffinity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_during_scheduling_ignored_during_execution: Option<NodeSelector>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferred_during_scheduling_ignored_during_execution: Vec<PreferredSchedulingTerm>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelector {
    #[serde(default)]
    pub node_selector_terms: Vec<NodeSelectorTerm>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorTerm {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_expressions: Vec<NodeSelectorRequirement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_fields: Vec<NodeSelectorRequirement>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorRequirement {
    pub key: String,
    pub operator: String, // In, NotIn, Exists, DoesNotExist, Gt, Lt
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PreferredSchedulingTerm {
    pub weight: i32,
    pub preference: NodeSelectorTerm,
}

// ----- Pod affinity / anti-affinity ------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodAffinity {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_during_scheduling_ignored_during_execution: Vec<PodAffinityTerm>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferred_during_scheduling_ignored_during_execution: Vec<WeightedPodAffinityTerm>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodAntiAffinity {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_during_scheduling_ignored_during_execution: Vec<PodAffinityTerm>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferred_during_scheduling_ignored_during_execution: Vec<WeightedPodAffinityTerm>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodAffinityTerm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_selector: Option<super::LabelSelector>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    pub topology_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WeightedPodAffinityTerm {
    pub weight: i32,
    pub pod_affinity_term: PodAffinityTerm,
}
