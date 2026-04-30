use sqlx::AnyPool;
use tokio::time::{interval, Duration};

use crate::db::{LeaseManager, NodeRepository, PodRepository};
use crate::models::*;

use super::controller;

/// Scheduler assigns pending pods to nodes
pub struct Scheduler {
    pool: AnyPool,
    leases: LeaseManager,
}

/// Scheduler lease TTL. Multi-master setups must guarantee only one
/// scheduler is binding pods at a time — two schedulers picking different
/// nodes for the same pod would race on `pods.node_name`.
const SCHEDULER_LEASE_TTL: Duration = Duration::from_secs(30);

impl Scheduler {
    pub fn new(pool: AnyPool, leases: LeaseManager) -> Self {
        Self { pool, leases }
    }

    /// Run the scheduler loop. In Postgres mode the lease ensures only one
    /// master at a time picks nodes for pending pods.
    pub async fn run(&self) {
        let mut interval = interval(Duration::from_secs(2));

        loop {
            interval.tick().await;

            if !self.leases.try_acquire("scheduler", SCHEDULER_LEASE_TTL).await {
                continue;
            }

            if let Err(e) = self.schedule_pending_pods().await {
                tracing::error!("Scheduler error: {}", e);
            }
        }
    }

    /// Find and schedule all pending pods
    async fn schedule_pending_pods(&self) -> anyhow::Result<()> {
        let pods = PodRepository::list_unscheduled(&self.pool).await?;
        if pods.is_empty() {
            return Ok(());
        }

        let nodes = NodeRepository::list_ready(&self.pool).await?;
        if nodes.is_empty() {
            tracing::debug!("No ready nodes available for scheduling");
            return Ok(());
        }

        // Pre-fetch all currently-bound pods. Pod (anti-)affinity needs to
        // know which node each existing pod ended up on. Cheap because the
        // entire cluster's pod list isn't huge.
        let existing_pods = PodRepository::list(&self.pool, None, None).await.unwrap_or_default();

        for pod in pods {
            if let Err(e) = self.schedule_pod(&pod, &nodes, &existing_pods).await {
                tracing::warn!(
                    "Failed to schedule pod {}/{}: {}",
                    pod.metadata.namespace(),
                    pod.metadata.name(),
                    e
                );
            }
        }
        Ok(())
    }

    async fn schedule_pod(
        &self,
        pod: &Pod,
        nodes: &[Node],
        existing_pods: &[Pod],
    ) -> anyhow::Result<()> {
        let namespace = pod.metadata.namespace();
        let pod_name = pod.metadata.name();

        let node = find_node_for_pod(pod, nodes, existing_pods)?;
        let node_name = node.metadata.name();

        tracing::info!(
            "Scheduling pod {}/{} to node {}",
            namespace, pod_name, node_name
        );

        PodRepository::bind_to_node(&self.pool, namespace, pod_name, node_name).await?;

        controller::emit_event(
            &self.pool,
            &ObjectReference {
                api_version: Some("v1".to_string()),
                kind: Some("Pod".to_string()),
                name: pod.metadata.name.clone(),
                namespace: pod.metadata.namespace.clone(),
                uid: pod.metadata.uid,
                resource_version: None,
                field_path: None,
            },
            namespace,
            EventType::Normal,
            "Scheduled",
            &format!("Successfully assigned {}/{} to {}", namespace, pod_name, node_name),
            "scheduler",
        )
        .await;

        Ok(())
    }
}

// ============================================================================
// Predicates: a candidate node passes only if every required filter agrees.
// ============================================================================

fn find_node_for_pod<'a>(
    pod: &Pod,
    nodes: &'a [Node],
    existing_pods: &[Pod],
) -> anyhow::Result<&'a Node> {
    let node_selector = pod.spec.node_selector.as_ref();
    let affinity = pod.spec.affinity.as_ref();

    for node in nodes {
        // 1. Node must be Ready.
        if !is_node_ready(node) {
            continue;
        }

        // 2. spec.nodeSelector — simple equality on labels.
        if let Some(selector) = node_selector {
            let labels = node.metadata.labels();
            if !selector.iter().all(|(k, v)| labels.get(k) == Some(v)) {
                continue;
            }
        }

        // 3. nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution.
        if let Some(na) = affinity.and_then(|a| a.node_affinity.as_ref()) {
            if let Some(required) = na.required_during_scheduling_ignored_during_execution.as_ref() {
                let matches_any = required
                    .node_selector_terms
                    .iter()
                    .any(|term| node_matches_term(node, term));
                if !matches_any {
                    continue;
                }
            }
        }

        // 4. podAffinity / podAntiAffinity required terms.
        if let Some(pa) = affinity.and_then(|a| a.pod_affinity.as_ref()) {
            let pod_ns = pod.metadata.namespace();
            let mut all_satisfied = true;
            for term in &pa.required_during_scheduling_ignored_during_execution {
                if !pod_affinity_satisfied(node, nodes, existing_pods, term, pod_ns, true) {
                    all_satisfied = false;
                    break;
                }
            }
            if !all_satisfied {
                continue;
            }
        }
        if let Some(paa) = affinity.and_then(|a| a.pod_anti_affinity.as_ref()) {
            let pod_ns = pod.metadata.namespace();
            let mut all_satisfied = true;
            for term in &paa.required_during_scheduling_ignored_during_execution {
                if !pod_affinity_satisfied(node, nodes, existing_pods, term, pod_ns, false) {
                    all_satisfied = false;
                    break;
                }
            }
            if !all_satisfied {
                continue;
            }
        }

        return Ok(node);
    }

    anyhow::bail!("No suitable node found for pod")
}

fn is_node_ready(node: &Node) -> bool {
    node.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conds| {
            conds.iter().any(|c| {
                c.condition_type == NodeConditionType::Ready && c.status == ConditionStatus::True
            })
        })
        // No conditions reported yet — treat as Ready (heartbeat has updated nodes table to status=Ready).
        .unwrap_or(true)
}

/// Evaluate a single nodeSelectorTerm against a node's labels. ALL
/// matchExpressions inside the term must hold; we don't model matchFields yet.
fn node_matches_term(node: &Node, term: &NodeSelectorTerm) -> bool {
    let labels = node.metadata.labels();
    term.match_expressions
        .iter()
        .all(|req| match_expression(req, &labels))
}

fn match_expression(
    req: &NodeSelectorRequirement,
    labels: &std::collections::HashMap<String, String>,
) -> bool {
    // NodeSelectorRequirement.values is `Vec<String>` (always present for In/NotIn/Gt/Lt).
    let val = labels.get(&req.key);
    match req.operator.as_str() {
        "In" => val
            .map(|v| req.values.iter().any(|x| x == v))
            .unwrap_or(false),
        "NotIn" => val
            .map(|v| req.values.iter().all(|x| x != v))
            .unwrap_or(true),
        "Exists" => val.is_some(),
        "DoesNotExist" => val.is_none(),
        "Gt" => val
            .and_then(|v| v.parse::<i64>().ok())
            .zip(req.values.first().and_then(|x| x.parse::<i64>().ok()))
            .map(|(a, b)| a > b)
            .unwrap_or(false),
        "Lt" => val
            .and_then(|v| v.parse::<i64>().ok())
            .zip(req.values.first().and_then(|x| x.parse::<i64>().ok()))
            .map(|(a, b)| a < b)
            .unwrap_or(false),
        _ => true,
    }
}

/// Pod (anti-)affinity check.
///
/// `term` defines a label selector + topology key. We pass when:
///   - `affinity = true`:  candidate node shares a topology-key value with at
///                         least one existing pod whose labels match the selector.
///   - `affinity = false`: no matching existing pod is on a node that shares
///                         the candidate's topology-key value.
fn pod_affinity_satisfied(
    candidate: &Node,
    nodes: &[Node],
    existing_pods: &[Pod],
    term: &PodAffinityTerm,
    pod_namespace: &str,
    affinity: bool,
) -> bool {
    let topology_key = &term.topology_key;
    let candidate_topo = topology_value(candidate, topology_key);

    // node_name → topology value (owned strings to keep lifetimes simple).
    let node_topo: std::collections::HashMap<String, Option<String>> = nodes
        .iter()
        .map(|n| {
            let labels = n.metadata.labels();
            (n.metadata.name().to_string(), labels.get(topology_key).cloned())
        })
        .collect();

    let selector = term.label_selector.as_ref();
    let allowed_ns: Vec<String> = if term.namespaces.is_empty() {
        vec![pod_namespace.to_string()]
    } else {
        term.namespaces.clone()
    };

    let any_match = existing_pods.iter().any(|p| {
        let p_ns = p.metadata.namespace();
        if !allowed_ns.iter().any(|n| n == p_ns) {
            return false;
        }
        if !selector_matches(selector, &p.metadata.labels()) {
            return false;
        }
        let pod_node = match &p.spec.node_name {
            Some(n) => n,
            None => return false,
        };
        let pod_topo = node_topo.get(pod_node).cloned().flatten();
        match (pod_topo.as_deref(), candidate_topo.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    });

    if affinity {
        any_match
    } else {
        !any_match
    }
}

fn topology_value(node: &Node, key: &str) -> Option<String> {
    node.metadata.labels().get(key).cloned()
}

fn selector_matches(
    selector: Option<&LabelSelector>,
    labels: &std::collections::HashMap<String, String>,
) -> bool {
    let s = match selector {
        Some(s) => s,
        // No selector means "match all" per Kubernetes API semantics.
        None => return true,
    };
    if let Some(ml) = &s.match_labels {
        if !ml.iter().all(|(k, v)| labels.get(k) == Some(v)) {
            return false;
        }
    }
    if let Some(exprs) = &s.match_expressions {
        for req in exprs {
            let val = labels.get(&req.key);
            let values: &[String] = req.values.as_deref().unwrap_or(&[]);
            let ok = match req.operator.as_str() {
                "In" => val.map(|v| values.iter().any(|x| x == v)).unwrap_or(false),
                "NotIn" => val.map(|v| values.iter().all(|x| x != v)).unwrap_or(true),
                "Exists" => val.is_some(),
                "DoesNotExist" => val.is_none(),
                _ => true,
            };
            if !ok {
                return false;
            }
        }
    }
    true
}
