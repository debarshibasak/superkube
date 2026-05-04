use sqlx::AnyPool;
use tokio::time::{interval, Duration};

use crate::db::{LeaseManager, NodeRepository, PodRepository};
use crate::models::*;

use super::controller;

/// Scheduler assigns pending pods to nodes
pub struct Scheduler {
    pool: AnyPool,
    leases: LeaseManager,
    strategy: ScoringStrategy,
    bus: std::sync::Arc<super::bus::Bus>,
}

/// Scheduler lease TTL. Multi-master setups must guarantee only one
/// scheduler is binding pods at a time — two schedulers picking different
/// nodes for the same pod would race on `pods.node_name`.
const SCHEDULER_LEASE_TTL: Duration = Duration::from_secs(30);

/// How surviving candidates are ranked once the predicate phase has
/// produced more than one viable node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScoringStrategy {
    /// Prefer the node with the most free cpu+memory headroom — spreads load.
    #[default]
    LeastAllocated,
    /// Prefer the node with the least free headroom — bin-packs so idle
    /// nodes can scale down.
    MostAllocated,
}

impl ScoringStrategy {
    /// Read from `SUPERKUBE_SCHEDULER_SCORING`. Anything matching
    /// `most`/`mostAllocated`/`most-allocated`/`bin-pack`/`binpack`
    /// (case-insensitive) selects MostAllocated; everything else, including
    /// unset, defaults to LeastAllocated.
    fn from_env() -> Self {
        let v = match std::env::var("SUPERKUBE_SCHEDULER_SCORING") {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        let v = v.trim().to_ascii_lowercase();
        match v.as_str() {
            "most" | "mostallocated" | "most-allocated" | "binpack" | "bin-pack" => {
                Self::MostAllocated
            }
            _ => Self::LeastAllocated,
        }
    }
}

impl Scheduler {
    pub fn new(
        pool: AnyPool,
        leases: LeaseManager,
        bus: std::sync::Arc<super::bus::Bus>,
    ) -> Self {
        let strategy = ScoringStrategy::from_env();
        tracing::info!("Scheduler scoring strategy: {:?}", strategy);
        Self { pool, leases, strategy, bus }
    }

    /// Run the scheduler loop. In Postgres mode the lease ensures only one
    /// master at a time picks nodes for pending pods.
    pub async fn run(&self) {
        tracing::info!("Scheduler started (interval=2s, lease_ttl={:?})", SCHEDULER_LEASE_TTL);
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
            tracing::info!(
                "Scheduler: {} pending pods but no Ready nodes available",
                pods.len()
            );
            return Ok(());
        }

        tracing::info!(
            "Scheduler: scheduling {} pending pod(s) across {} Ready node(s)",
            pods.len(),
            nodes.len()
        );

        // Pre-fetch all currently-bound pods. Pod (anti-)affinity needs to
        // know which node each existing pod ended up on. Cheap because the
        // entire cluster's pod list isn't huge.
        // Mutable so we can record in-tick bindings and prevent every pod in
        // a batch from landing on the same "least-loaded" node.
        let mut existing_pods =
            PodRepository::list(&self.pool, None, None).await.unwrap_or_default();

        for pod in pods {
            match self.schedule_pod(&pod, &nodes, &existing_pods).await {
                Ok(node_name) => {
                    let mut bound = pod.clone();
                    bound.spec.node_name = Some(node_name);
                    existing_pods.push(bound);
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to schedule pod {}/{}: {}",
                        pod.metadata.namespace(),
                        pod.metadata.name(),
                        e
                    );
                }
            }
        }
        Ok(())
    }

    async fn schedule_pod(
        &self,
        pod: &Pod,
        nodes: &[Node],
        existing_pods: &[Pod],
    ) -> anyhow::Result<String> {
        let namespace = pod.metadata.namespace();
        let pod_name = pod.metadata.name();

        let node = find_node_for_pod(pod, nodes, existing_pods, self.strategy)?;
        let node_name = node.metadata.name();

        tracing::info!(
            "Scheduling pod {}/{} to node {}",
            namespace, pod_name, node_name
        );

        PodRepository::bind_to_node(&self.pool, namespace, pod_name, node_name).await?;

        // Publish a watch event so anything listening on `kubectl get pods -w`
        // sees the bind immediately. Best-effort — a missed read here just
        // means watchers see the change on the next reconcile or list.
        if let Ok(updated) =
            PodRepository::get(&self.pool, namespace, pod_name).await
        {
            self.bus
                .publish_modified("Pod", Some(namespace), pod_name, &updated);
        }

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

        Ok(node_name.to_string())
    }
}

// ============================================================================
// Predicates: a candidate node passes only if every required filter agrees.
// ============================================================================

fn find_node_for_pod<'a>(
    pod: &Pod,
    nodes: &'a [Node],
    existing_pods: &[Pod],
    strategy: ScoringStrategy,
) -> anyhow::Result<&'a Node> {
    let node_selector = pod.spec.node_selector.as_ref();
    let affinity = pod.spec.affinity.as_ref();
    let (pod_cpu_m, pod_mem_b) = sum_pod_requests(pod);

    let mut candidates: Vec<&'a Node> = Vec::new();
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

        // 5. Capacity fit: sum requests of pods already bound to this node and
        //    require allocatable >= used + this pod's requests. Pods/keys with
        //    no requests count as zero (BestEffort). Missing allocatable on a
        //    given key is treated as unconstrained for that key.
        if !node_fits(node, existing_pods, pod_cpu_m, pod_mem_b) {
            continue;
        }

        candidates.push(node);
    }

    // Pick the highest-scoring candidate. Ties go to the earlier node, which
    // gives stable ordering for callers that pre-sort `nodes`.
    candidates
        .into_iter()
        .map(|n| {
            let s = score_node(n, existing_pods, pod_cpu_m, pod_mem_b, strategy);
            (n, s)
        })
        .max_by_key(|(_, s)| *s)
        .map(|(n, _)| n)
        .ok_or_else(|| anyhow::anyhow!("No suitable node found for pod"))
}

/// Score a candidate node from 0..=100. Higher is always better; the
/// `strategy` decides what "better" means. Scoring assumes the candidate pod
/// is already placed on this node (so equally-loaded peers diverge as a
/// batch is scheduled).
///
/// LeastAllocated  → percent free (post-placement), averaged over cpu+memory.
/// MostAllocated   → percent used (post-placement), averaged over cpu+memory.
///
/// Resources whose allocatable is missing or zero are skipped from the
/// average; if neither cpu nor memory has usable data, the score is 0.
fn score_node(
    node: &Node,
    existing_pods: &[Pod],
    pod_cpu_m: u64,
    pod_mem_b: u64,
    strategy: ScoringStrategy,
) -> i64 {
    let (used_cpu_m, used_mem_b) = sum_used_on_node(node, existing_pods);
    let alloc = node.status.as_ref().and_then(|s| s.allocatable.as_ref());
    let alloc_cpu = alloc
        .and_then(|a| a.get("cpu"))
        .and_then(|s| parse_cpu_millicores(s));
    let alloc_mem = alloc
        .and_then(|a| a.get("memory"))
        .and_then(|s| parse_memory_bytes(s));

    let mut sum: f64 = 0.0;
    let mut n: u32 = 0;

    if let Some(c) = alloc_cpu.filter(|c| *c > 0) {
        let used_after = used_cpu_m.saturating_add(pod_cpu_m).min(c);
        let used_pct = (used_after as f64 / c as f64) * 100.0;
        sum += match strategy {
            ScoringStrategy::LeastAllocated => 100.0 - used_pct,
            ScoringStrategy::MostAllocated => used_pct,
        };
        n += 1;
    }
    if let Some(m) = alloc_mem.filter(|m| *m > 0) {
        let used_after = used_mem_b.saturating_add(pod_mem_b).min(m);
        let used_pct = (used_after as f64 / m as f64) * 100.0;
        sum += match strategy {
            ScoringStrategy::LeastAllocated => 100.0 - used_pct,
            ScoringStrategy::MostAllocated => used_pct,
        };
        n += 1;
    }

    if n == 0 {
        return 0;
    }
    (sum / n as f64).round() as i64
}

/// Sum cpu (millicores) and memory (bytes) requests of non-terminal pods
/// already bound to `node`.
fn sum_used_on_node(node: &Node, existing_pods: &[Pod]) -> (u64, u64) {
    let node_name = node.metadata.name();
    let mut cpu_m: u64 = 0;
    let mut mem_b: u64 = 0;
    for p in existing_pods {
        if p.spec.node_name.as_deref() != Some(node_name) {
            continue;
        }
        if let Some(s) = p.status.as_ref() {
            if matches!(s.phase, PodPhase::Succeeded | PodPhase::Failed) {
                continue;
            }
        }
        let (c, m) = sum_pod_requests(p);
        cpu_m = cpu_m.saturating_add(c);
        mem_b = mem_b.saturating_add(m);
    }
    (cpu_m, mem_b)
}

/// Sum of `containers[].resources.requests` for cpu (millicores) and memory
/// (bytes). Unparseable values are treated as zero so a malformed manifest
/// cannot wedge scheduling.
fn sum_pod_requests(pod: &Pod) -> (u64, u64) {
    let mut cpu_m: u64 = 0;
    let mut mem_b: u64 = 0;
    for c in &pod.spec.containers {
        let Some(reqs) = c.resources.as_ref().and_then(|r| r.requests.as_ref()) else {
            continue;
        };
        if let Some(v) = reqs.get("cpu") {
            cpu_m = cpu_m.saturating_add(parse_cpu_millicores(v).unwrap_or(0));
        }
        if let Some(v) = reqs.get("memory") {
            mem_b = mem_b.saturating_add(parse_memory_bytes(v).unwrap_or(0));
        }
    }
    (cpu_m, mem_b)
}

/// Whether `node`'s allocatable cpu/memory can accommodate `pod_cpu_m` /
/// `pod_mem_b` in addition to the already-bound, non-terminal pods on it.
fn node_fits(node: &Node, existing_pods: &[Pod], pod_cpu_m: u64, pod_mem_b: u64) -> bool {
    let (used_cpu_m, used_mem_b) = sum_used_on_node(node, existing_pods);

    let allocatable = node.status.as_ref().and_then(|s| s.allocatable.as_ref());

    if pod_cpu_m > 0 {
        if let Some(alloc_cpu) = allocatable
            .and_then(|a| a.get("cpu"))
            .and_then(|s| parse_cpu_millicores(s))
        {
            if used_cpu_m.saturating_add(pod_cpu_m) > alloc_cpu {
                return false;
            }
        }
    }
    if pod_mem_b > 0 {
        if let Some(alloc_mem) = allocatable
            .and_then(|a| a.get("memory"))
            .and_then(|s| parse_memory_bytes(s))
        {
            if used_mem_b.saturating_add(pod_mem_b) > alloc_mem {
                return false;
            }
        }
    }
    true
}

/// Parse a Kubernetes CPU quantity into millicores.
/// Accepts "100m", "0.5", "2", "1.5". Returns None on malformed input.
fn parse_cpu_millicores(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('m') {
        return num.trim().parse::<u64>().ok();
    }
    let cores: f64 = s.parse().ok()?;
    if cores < 0.0 || !cores.is_finite() {
        return None;
    }
    Some((cores * 1000.0).round() as u64)
}

/// Parse a Kubernetes memory quantity into bytes.
/// Binary suffixes (Ki, Mi, Gi, Ti, Pi, Ei) use 1024; decimal suffixes
/// (k, M, G, T, P, E) use 1000. Plain numbers are bytes.
fn parse_memory_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num_part, mult) = if let Some(rest) = s.strip_suffix("Ki") {
        (rest, 1024u128)
    } else if let Some(rest) = s.strip_suffix("Mi") {
        (rest, 1024u128.pow(2))
    } else if let Some(rest) = s.strip_suffix("Gi") {
        (rest, 1024u128.pow(3))
    } else if let Some(rest) = s.strip_suffix("Ti") {
        (rest, 1024u128.pow(4))
    } else if let Some(rest) = s.strip_suffix("Pi") {
        (rest, 1024u128.pow(5))
    } else if let Some(rest) = s.strip_suffix("Ei") {
        (rest, 1024u128.pow(6))
    } else if let Some(rest) = s.strip_suffix('k') {
        (rest, 1_000u128)
    } else if let Some(rest) = s.strip_suffix('M') {
        (rest, 1_000_000u128)
    } else if let Some(rest) = s.strip_suffix('G') {
        (rest, 1_000_000_000u128)
    } else if let Some(rest) = s.strip_suffix('T') {
        (rest, 1_000_000_000_000u128)
    } else if let Some(rest) = s.strip_suffix('P') {
        (rest, 1_000_000_000_000_000u128)
    } else if let Some(rest) = s.strip_suffix('E') {
        (rest, 1_000_000_000_000_000_000u128)
    } else {
        (s, 1u128)
    };

    let num: f64 = num_part.trim().parse().ok()?;
    if num < 0.0 || !num.is_finite() {
        return None;
    }
    let bytes = (num * mult as f64).round();
    if bytes < 0.0 || bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
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
