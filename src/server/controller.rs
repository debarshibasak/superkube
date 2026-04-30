use chrono::Utc;
use sqlx::AnyPool;
use std::collections::HashSet;
use tokio::time::{interval, Duration};
use uuid::Uuid;

use crate::db::{
    DaemonSetRepository, DeploymentRepository, EndpointsRepository, EventRepository,
    LeaseManager, NodeRepository, PodRepository, ServiceRepository, StatefulSetRepository,
};
use crate::models::{
    DaemonSet, Deployment, Endpoints, EndpointAddress, EndpointPort, EndpointSubset,
    Event, EventSource, EventType, IntOrString, Node, ObjectMeta, ObjectReference,
    OwnerReference, Pod, PodPhase, PodStatus, RestartPolicy, Service, StatefulSet, TypeMeta,
};

/// Controller manager runs reconciliation loops for deployments and services
pub struct ControllerManager {
    pool: AnyPool,
    leases: LeaseManager,
}

/// How long a per-controller lease is valid. Comfortably longer than the
/// 5s reconcile tick so a single missed scheduling slot doesn't drop the
/// lease, but short enough that a crashed master is recovered within ~30s.
const LEASE_TTL: Duration = Duration::from_secs(30);

impl ControllerManager {
    pub fn new(pool: AnyPool, leases: LeaseManager) -> Self {
        Self { pool, leases }
    }

    /// Run all controllers. In Postgres mode each reconcile pass first
    /// acquires a named lease — if another master holds it, this pass is
    /// skipped and we try again next tick. In SQLite mode `try_acquire`
    /// short-circuits to true and the loop runs as before.
    pub async fn run(&self) {
        tracing::info!("Controller manager started");
        let mut interval = interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

            if self.leases.try_acquire("controller/deployment", LEASE_TTL).await {
                if let Err(e) = self.reconcile_deployments().await {
                    tracing::error!("Deployment controller error: {}", e);
                }
            }

            if self.leases.try_acquire("controller/statefulset", LEASE_TTL).await {
                if let Err(e) = self.reconcile_statefulsets().await {
                    tracing::error!("StatefulSet controller error: {}", e);
                }
            }

            if self.leases.try_acquire("controller/daemonset", LEASE_TTL).await {
                if let Err(e) = self.reconcile_daemonsets().await {
                    tracing::error!("DaemonSet controller error: {}", e);
                }
            }

            if self.leases.try_acquire("controller/pod", LEASE_TTL).await {
                if let Err(e) = self.reconcile_pods().await {
                    tracing::error!("Pod controller error: {}", e);
                }
            }

            if self.leases.try_acquire("controller/service", LEASE_TTL).await {
                if let Err(e) = self.reconcile_services().await {
                    tracing::error!("Service controller error: {}", e);
                }
            }
        }
    }

    /// Reconcile all deployments - ensure replica count matches
    async fn reconcile_deployments(&self) -> anyhow::Result<()> {
        let deployments = DeploymentRepository::list(&self.pool, None).await?;

        for deployment in deployments {
            if let Err(e) = self.reconcile_deployment(&deployment).await {
                tracing::error!(
                    "Failed to reconcile deployment {}/{}: {}",
                    deployment.metadata.namespace(),
                    deployment.metadata.name(),
                    e
                );
            }
        }

        Ok(())
    }

    /// Reconcile a single deployment
    async fn reconcile_deployment(&self, deployment: &Deployment) -> anyhow::Result<()> {
        let namespace = deployment.metadata.namespace();
        let deployment_name = deployment.metadata.name();
        let desired_replicas = deployment.spec.replicas();

        tracing::debug!(
            "Reconciling deployment {}/{}, desired replicas: {}",
            namespace,
            deployment_name,
            desired_replicas
        );

        // Get pods matching this deployment's selector
        let selector = deployment
            .spec
            .selector
            .match_labels
            .clone()
            .unwrap_or_default();

        // If selector is empty, we can't find pods - use deployment labels from template
        let effective_selector = if selector.is_empty() {
            deployment
                .spec
                .template
                .metadata
                .labels
                .clone()
                .unwrap_or_default()
        } else {
            selector
        };

        let pods = if effective_selector.is_empty() {
            // No selector at all, list all pods in namespace and filter by owner
            PodRepository::list(&self.pool, Some(namespace), None).await?
        } else {
            PodRepository::list(&self.pool, Some(namespace), Some(&effective_selector)).await?
        };

        // Filter pods owned by this deployment
        let deployment_uid = deployment.metadata.uid;
        let owned_pods: Vec<_> = pods
            .iter()
            .filter(|pod| {
                pod.metadata
                    .owner_references
                    .as_ref()
                    .map(|refs| refs.iter().any(|r| Some(r.uid) == deployment_uid))
                    .unwrap_or(false)
            })
            .collect();

        let current_replicas = owned_pods.len() as i32;

        tracing::debug!(
            "Deployment {}/{}: current={}, desired={}",
            namespace,
            deployment_name,
            current_replicas,
            desired_replicas
        );

        if current_replicas < desired_replicas {
            let to_create = desired_replicas - current_replicas;
            tracing::info!(
                "Deployment {}/{}: creating {} pods",
                namespace,
                deployment_name,
                to_create
            );
            emit_event(
                &self.pool,
                &deployment_ref(deployment),
                namespace,
                EventType::Normal,
                "ScalingReplicaSet",
                &format!("Scaled up replica set {} from {} to {}", deployment_name, current_replicas, desired_replicas),
                "deployment-controller",
            )
            .await;

            for _ in 0..to_create {
                if let Err(e) = self.create_pod_for_deployment(deployment).await {
                    tracing::error!(
                        "Failed to create pod for deployment {}/{}: {}",
                        namespace,
                        deployment_name,
                        e
                    );
                }
            }
        } else if current_replicas > desired_replicas {
            let to_delete = current_replicas - desired_replicas;
            tracing::info!(
                "Deployment {}/{}: deleting {} pods",
                namespace,
                deployment_name,
                to_delete
            );
            emit_event(
                &self.pool,
                &deployment_ref(deployment),
                namespace,
                EventType::Normal,
                "ScalingReplicaSet",
                &format!("Scaled down replica set {} from {} to {}", deployment_name, current_replicas, desired_replicas),
                "deployment-controller",
            )
            .await;

            for pod in owned_pods.iter().take(to_delete as usize) {
                if let Some(name) = &pod.metadata.name {
                    let _ = PodRepository::delete(&self.pool, namespace, name).await;
                }
            }
        }

        // Update deployment status
        let ready_replicas = owned_pods
            .iter()
            .filter(|p| {
                p.status
                    .as_ref()
                    .map(|s| s.phase == PodPhase::Running)
                    .unwrap_or(false)
            })
            .count() as i32;

        DeploymentRepository::update_status(
            &self.pool,
            namespace,
            deployment_name,
            ready_replicas,
            ready_replicas,
        )
        .await?;

        Ok(())
    }

    /// Create a pod for a deployment
    async fn create_pod_for_deployment(&self, deployment: &Deployment) -> anyhow::Result<()> {
        let namespace = deployment.metadata.namespace();
        let deployment_name = deployment.metadata.name();
        let deployment_uid = deployment.metadata.uid.unwrap_or_else(Uuid::new_v4);

        // Generate unique pod name
        let pod_suffix: String = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let pod_name = format!("{}-{}", deployment_name, pod_suffix);

        // Create pod from template
        let template = &deployment.spec.template;

        tracing::debug!(
            "Creating pod {} for deployment {}/{}, containers: {}",
            pod_name,
            namespace,
            deployment_name,
            template.spec.containers.len()
        );

        let pod = Pod {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Pod".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(pod_name.clone()),
                namespace: Some(namespace.to_string()),
                uid: Some(Uuid::new_v4()),
                labels: template.metadata.labels.clone(),
                annotations: template.metadata.annotations.clone(),
                owner_references: Some(vec![OwnerReference {
                    api_version: "apps/v1".to_string(),
                    kind: "Deployment".to_string(),
                    name: deployment_name.to_string(),
                    uid: deployment_uid,
                    controller: Some(true),
                    block_owner_deletion: Some(true),
                }]),
                ..Default::default()
            },
            spec: template.spec.clone(),
            status: Some(PodStatus {
                phase: PodPhase::Pending,
                ..Default::default()
            }),
        };

        PodRepository::create(&self.pool, &pod).await?;
        tracing::info!("Created pod {} in namespace {}", pod_name, namespace);

        Ok(())
    }

    /// Reconcile all services - update endpoints
    async fn reconcile_services(&self) -> anyhow::Result<()> {
        let services = ServiceRepository::list(&self.pool, None).await?;

        for service in services {
            if let Err(e) = self.reconcile_service(&service).await {
                tracing::error!(
                    "Failed to reconcile service {}/{}: {}",
                    service.metadata.namespace(),
                    service.metadata.name(),
                    e
                );
            }
        }

        Ok(())
    }

    // ========================================================================
    // StatefulSets
    // ========================================================================

    async fn reconcile_statefulsets(&self) -> anyhow::Result<()> {
        let sets = StatefulSetRepository::list(&self.pool, None).await?;
        for ss in sets {
            if let Err(e) = self.reconcile_statefulset(&ss).await {
                tracing::error!(
                    "Failed to reconcile statefulset {}/{}: {}",
                    ss.metadata.namespace(),
                    ss.metadata.name(),
                    e
                );
            }
        }
        Ok(())
    }

    /// Walk pod indices 0..replicas. Each missing index gets created (named
    /// `<set>-<i>`). For OrderedReady mode, creation halts at the first
    /// not-yet-Running pod so we stay strictly sequential.
    async fn reconcile_statefulset(&self, ss: &StatefulSet) -> anyhow::Result<()> {
        let namespace = ss.metadata.namespace();
        let name = ss.metadata.name();
        let desired = ss.spec.replicas();
        let parallel = ss.spec.is_parallel();
        let ss_uid = ss.metadata.uid.unwrap_or_else(Uuid::new_v4);

        let mut current = 0i32;
        let mut ready = 0i32;

        for i in 0..desired {
            let pod_name = format!("{}-{}", name, i);

            match PodRepository::get(&self.pool, namespace, &pod_name).await {
                Ok(pod) => {
                    current += 1;
                    let is_running = pod
                        .status
                        .as_ref()
                        .map(|s| s.phase == PodPhase::Running)
                        .unwrap_or(false);
                    if is_running {
                        ready += 1;
                    } else if !parallel {
                        // OrderedReady: don't create the next pod until this one is up.
                        break;
                    }
                }
                Err(_) => {
                    // Missing — create it.
                    let pod = pod_for_statefulset(ss, ss_uid, &pod_name);
                    if let Err(e) = PodRepository::create(&self.pool, &pod).await {
                        tracing::error!("statefulset {}/{}: failed to create {}: {}",
                            namespace, name, pod_name, e);
                        break;
                    }
                    tracing::info!("statefulset {}/{}: created pod {}", namespace, name, pod_name);
                    emit_event(
                        &self.pool,
                        &statefulset_ref(ss),
                        namespace,
                        EventType::Normal,
                        "SuccessfulCreate",
                        &format!("create Pod {} in StatefulSet {} successful", pod_name, name),
                        "statefulset-controller",
                    )
                    .await;
                    if !parallel {
                        // Wait until next reconcile to check it became Running.
                        break;
                    }
                }
            }
        }

        // Trim any pods beyond `desired` (scale-down — reverse order).
        for i in (desired..desired + 32).rev() {
            // (lo bound: desired; hi bound: 32 above, an arbitrary safety cap)
            let pod_name = format!("{}-{}", name, i);
            if PodRepository::get(&self.pool, namespace, &pod_name).await.is_ok() {
                let _ = PodRepository::delete(&self.pool, namespace, &pod_name).await;
                tracing::info!("statefulset {}/{}: deleted pod {} (scale-down)",
                    namespace, name, pod_name);
            }
        }

        StatefulSetRepository::update_status(&self.pool, namespace, name, ready, current).await?;
        Ok(())
    }

    // ========================================================================
    // DaemonSets
    // ========================================================================

    async fn reconcile_daemonsets(&self) -> anyhow::Result<()> {
        let sets = DaemonSetRepository::list(&self.pool, None).await?;
        if sets.is_empty() {
            return Ok(());
        }
        let nodes = NodeRepository::list_ready(&self.pool).await?;
        for ds in sets {
            if let Err(e) = self.reconcile_daemonset(&ds, &nodes).await {
                tracing::error!(
                    "Failed to reconcile daemonset {}/{}: {}",
                    ds.metadata.namespace(),
                    ds.metadata.name(),
                    e
                );
            }
        }
        Ok(())
    }

    /// Ensure exactly one pod from the template runs on every node that
    /// matches the template's nodeSelector (if any).
    async fn reconcile_daemonset(&self, ds: &DaemonSet, nodes: &[Node]) -> anyhow::Result<()> {
        let namespace = ds.metadata.namespace();
        let name = ds.metadata.name();
        let ds_uid = ds.metadata.uid.unwrap_or_else(Uuid::new_v4);

        let template_node_selector = ds.spec.template.spec.node_selector.as_ref();

        // Nodes that *should* run this daemon.
        let target_nodes: Vec<&Node> = nodes
            .iter()
            .filter(|n| match template_node_selector {
                Some(sel) => {
                    let labels = n.metadata.labels();
                    sel.iter().all(|(k, v)| labels.get(k) == Some(v))
                }
                None => true,
            })
            .collect();

        let target_node_names: HashSet<String> =
            target_nodes.iter().map(|n| n.metadata.name().to_string()).collect();

        // Existing pods owned by this daemonset.
        let selector = ds
            .spec
            .selector
            .match_labels
            .clone()
            .or_else(|| ds.spec.template.metadata.labels.clone())
            .unwrap_or_default();
        let existing_pods = if selector.is_empty() {
            Vec::new()
        } else {
            PodRepository::list(&self.pool, Some(namespace), Some(&selector)).await?
        };
        let owned_pods: Vec<&Pod> = existing_pods
            .iter()
            .filter(|p| {
                p.metadata
                    .owner_references
                    .as_ref()
                    .map(|refs| refs.iter().any(|r| Some(r.uid) == ds.metadata.uid))
                    .unwrap_or(false)
            })
            .collect();

        let covered_nodes: HashSet<String> = owned_pods
            .iter()
            .filter_map(|p| p.spec.node_name.clone())
            .collect();

        // Create one pod per target node that isn't yet covered.
        for node in &target_nodes {
            let node_name = node.metadata.name().to_string();
            if covered_nodes.contains(&node_name) {
                continue;
            }
            let pod_suffix: String = Uuid::new_v4().to_string()[..8].to_string();
            let pod_name = format!("{}-{}", name, pod_suffix);
            let pod = pod_for_daemonset(ds, ds_uid, &pod_name, &node_name);
            if let Err(e) = PodRepository::create(&self.pool, &pod).await {
                tracing::error!("daemonset {}/{}: failed to create pod on {}: {}",
                    namespace, name, node_name, e);
                continue;
            }
            tracing::info!("daemonset {}/{}: created pod {} on node {}",
                namespace, name, pod_name, node_name);
            emit_event(
                &self.pool,
                &daemonset_ref(ds),
                namespace,
                EventType::Normal,
                "SuccessfulCreate",
                &format!("Created pod: {}", pod_name),
                "daemonset-controller",
            )
            .await;
        }

        // Delete pods on nodes that no longer match the selector.
        for pod in &owned_pods {
            if let Some(node_name) = &pod.spec.node_name {
                if !target_node_names.contains(node_name) {
                    if let Some(pod_name) = &pod.metadata.name {
                        let _ = PodRepository::delete(&self.pool, namespace, pod_name).await;
                        tracing::info!("daemonset {}/{}: deleted pod {} (node no longer matches)",
                            namespace, name, pod_name);
                    }
                }
            }
        }

        let desired = target_nodes.len() as i32;
        let current = owned_pods.len() as i32;
        let ready = owned_pods
            .iter()
            .filter(|p| {
                p.status
                    .as_ref()
                    .map(|s| s.phase == PodPhase::Running)
                    .unwrap_or(false)
            })
            .count() as i32;

        DaemonSetRepository::update_status(&self.pool, namespace, name, desired, current, ready)
            .await?;
        Ok(())
    }

    // ========================================================================
    // Pods (lifecycle + dead-node GC + restart policy)
    // ========================================================================

    async fn reconcile_pods(&self) -> anyhow::Result<()> {
        let pods = PodRepository::list(&self.pool, None, None).await?;
        let ready_nodes: HashSet<String> = NodeRepository::list_ready(&self.pool)
            .await?
            .into_iter()
            .map(|n| n.metadata.name().to_string())
            .collect();

        for pod in pods {
            let namespace = pod.metadata.namespace().to_string();
            let pod_name = pod.metadata.name().to_string();
            let phase = pod
                .status
                .as_ref()
                .map(|s| s.phase.clone())
                .unwrap_or(PodPhase::Pending);

            // Pod assigned to a node that is no longer Ready → mark Failed
            // so a Deployment / StatefulSet / DaemonSet controller replaces it.
            if let Some(node_name) = &pod.spec.node_name {
                if !ready_nodes.contains(node_name) && phase == PodPhase::Running {
                    let new_status = PodStatus {
                        phase: PodPhase::Failed,
                        pod_ip: None,
                        host_ip: pod.status.as_ref().and_then(|s| s.host_ip.clone()),
                        ..Default::default()
                    };
                    let _ = PodRepository::update_status(&self.pool, &namespace, &pod_name, &new_status).await;
                    tracing::warn!("pod {}/{}: node {} not Ready, marking Failed",
                        namespace, pod_name, node_name);
                    emit_event(
                        &self.pool,
                        &pod_ref(&pod),
                        &namespace,
                        EventType::Warning,
                        "NodeNotReady",
                        &format!("Node {} stopped reporting ready, pod marked Failed", node_name),
                        "node-controller",
                    )
                    .await;
                }
            }

            // restartPolicy=Never + Failed → leave it alone for the user to inspect.
            // restartPolicy=Always (default) + Failed without an owner → re-pend
            // by clearing nodeName, so the scheduler picks it up again. Owned pods
            // are handled by their respective controllers.
            let owned = pod
                .metadata
                .owner_references
                .as_ref()
                .map(|r| !r.is_empty())
                .unwrap_or(false);

            let policy = pod.spec.restart_policy.clone().unwrap_or(RestartPolicy::Always);
            if !owned && phase == PodPhase::Failed && matches!(policy, RestartPolicy::Always) {
                let _ = PodRepository::delete(&self.pool, &namespace, &pod_name).await;
                let mut respun = pod.clone();
                respun.spec.node_name = None;
                respun.metadata.uid = Some(Uuid::new_v4());
                respun.status = Some(PodStatus {
                    phase: PodPhase::Pending,
                    ..Default::default()
                });
                let _ = PodRepository::create(&self.pool, &respun).await;
                tracing::info!("pod {}/{}: restartPolicy=Always, re-creating", namespace, pod_name);
            }
        }
        Ok(())
    }

    /// Reconcile a single service - update its endpoints
    async fn reconcile_service(&self, service: &Service) -> anyhow::Result<()> {
        let namespace = service.metadata.namespace();
        let service_name = service.metadata.name();

        // Get pods matching this service's selector
        let selector = match &service.spec.selector {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()), // No selector, no endpoints
        };

        let pods = PodRepository::list(&self.pool, Some(namespace), Some(selector)).await?;

        // Build endpoint addresses from running pods
        let mut addresses = Vec::new();

        for pod in pods {
            let phase = pod
                .status
                .as_ref()
                .map(|s| &s.phase)
                .unwrap_or(&PodPhase::Pending);

            if *phase != PodPhase::Running {
                continue;
            }

            if let Some(pod_ip) = pod.status.as_ref().and_then(|s| s.pod_ip.clone()) {
                addresses.push(EndpointAddress {
                    ip: pod_ip,
                    hostname: None,
                    node_name: pod.spec.node_name.clone(),
                    target_ref: Some(ObjectReference {
                        api_version: Some("v1".to_string()),
                        kind: Some("Pod".to_string()),
                        name: pod.metadata.name.clone(),
                        namespace: pod.metadata.namespace.clone(),
                        uid: pod.metadata.uid,
                        resource_version: None,
                        field_path: None,
                    }),
                });
            }
        }

        // Build endpoint ports from service
        let ports: Vec<EndpointPort> = service
            .spec
            .ports
            .iter()
            .map(|sp| {
                let target_port = match &sp.target_port {
                    Some(IntOrString::Int(p)) => *p,
                    Some(IntOrString::String(_)) => sp.port, // Would need to resolve named ports
                    None => sp.port,
                };

                EndpointPort {
                    name: sp.name.clone(),
                    port: target_port,
                    protocol: sp.protocol.clone(),
                }
            })
            .collect();

        // Create or update endpoints
        let endpoints = Endpoints {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Endpoints".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(service_name.to_string()),
                namespace: Some(namespace.to_string()),
                uid: Some(Uuid::new_v4()),
                ..Default::default()
            },
            subsets: if addresses.is_empty() {
                vec![]
            } else {
                vec![EndpointSubset {
                    addresses: Some(addresses),
                    not_ready_addresses: None,
                    ports: Some(ports),
                }]
            },
        };

        EndpointsRepository::create_or_update(&self.pool, &endpoints).await?;

        Ok(())
    }
}

// ============================================================================
// Events
// ============================================================================

/// Fire-and-forget event recorder. Failures are logged at debug — we never
/// want a busted event write to break reconciliation.
pub(super) async fn emit_event(
    pool: &AnyPool,
    involved: &ObjectReference,
    namespace: &str,
    typ: EventType,
    reason: &str,
    message: &str,
    component: &str,
) {
    let now = Utc::now();
    let kind = involved.kind.clone().unwrap_or_default().to_lowercase();
    let obj_name = involved.name.clone().unwrap_or_default();
    // Event names in k8s are `<involvedName>.<random>` — short enough that
    // duplicates within a second hit the ON CONFLICT path.
    let event_name = format!(
        "{}.{}",
        if obj_name.is_empty() { "object".into() } else { obj_name.clone() },
        format!("{:x}", Uuid::new_v4().as_u128() & 0xFFFFFFFF)
    );

    let event = Event {
        type_meta: TypeMeta {
            api_version: Some("v1".to_string()),
            kind: Some("Event".to_string()),
        },
        metadata: ObjectMeta {
            name: Some(event_name),
            namespace: Some(namespace.to_string()),
            uid: Some(Uuid::new_v4()),
            ..Default::default()
        },
        involved_object: involved.clone(),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        source: Some(EventSource {
            component: Some(component.to_string()),
            host: None,
        }),
        first_timestamp: Some(now),
        last_timestamp: Some(now),
        event_time: Some(now),
        count: Some(1),
        event_type: Some(typ),
        action: None,
        reporting_controller: Some(format!("superkube/{component}")),
        reporting_instance: None,
    };

    if let Err(e) = EventRepository::create(pool, &event).await {
        tracing::debug!("emit_event({}/{}): {}", reason, kind, e);
    }
}

fn pod_ref(pod: &Pod) -> ObjectReference {
    ObjectReference {
        api_version: Some("v1".to_string()),
        kind: Some("Pod".to_string()),
        name: pod.metadata.name.clone(),
        namespace: pod.metadata.namespace.clone(),
        uid: pod.metadata.uid,
        resource_version: None,
        field_path: None,
    }
}

fn deployment_ref(d: &Deployment) -> ObjectReference {
    ObjectReference {
        api_version: Some("apps/v1".to_string()),
        kind: Some("Deployment".to_string()),
        name: d.metadata.name.clone(),
        namespace: d.metadata.namespace.clone(),
        uid: d.metadata.uid,
        resource_version: None,
        field_path: None,
    }
}

fn statefulset_ref(s: &StatefulSet) -> ObjectReference {
    ObjectReference {
        api_version: Some("apps/v1".to_string()),
        kind: Some("StatefulSet".to_string()),
        name: s.metadata.name.clone(),
        namespace: s.metadata.namespace.clone(),
        uid: s.metadata.uid,
        resource_version: None,
        field_path: None,
    }
}

fn daemonset_ref(d: &DaemonSet) -> ObjectReference {
    ObjectReference {
        api_version: Some("apps/v1".to_string()),
        kind: Some("DaemonSet".to_string()),
        name: d.metadata.name.clone(),
        namespace: d.metadata.namespace.clone(),
        uid: d.metadata.uid,
        resource_version: None,
        field_path: None,
    }
}

// ============================================================================
// Pod builders
// ============================================================================

fn pod_for_statefulset(ss: &StatefulSet, ss_uid: Uuid, pod_name: &str) -> Pod {
    let template = &ss.spec.template;
    let namespace = ss.metadata.namespace();

    Pod {
        type_meta: TypeMeta {
            api_version: Some("v1".to_string()),
            kind: Some("Pod".to_string()),
        },
        metadata: ObjectMeta {
            name: Some(pod_name.to_string()),
            namespace: Some(namespace.to_string()),
            uid: Some(Uuid::new_v4()),
            labels: template.metadata.labels.clone(),
            annotations: template.metadata.annotations.clone(),
            owner_references: Some(vec![OwnerReference {
                api_version: "apps/v1".to_string(),
                kind: "StatefulSet".to_string(),
                name: ss.metadata.name().to_string(),
                uid: ss_uid,
                controller: Some(true),
                block_owner_deletion: Some(true),
            }]),
            ..Default::default()
        },
        spec: template.spec.clone(),
        status: Some(PodStatus {
            phase: PodPhase::Pending,
            ..Default::default()
        }),
    }
}

fn pod_for_daemonset(ds: &DaemonSet, ds_uid: Uuid, pod_name: &str, node_name: &str) -> Pod {
    let template = &ds.spec.template;
    let namespace = ds.metadata.namespace();

    let mut spec = template.spec.clone();
    // DaemonSet pods are pre-bound to a node — bypass the scheduler.
    spec.node_name = Some(node_name.to_string());

    Pod {
        type_meta: TypeMeta {
            api_version: Some("v1".to_string()),
            kind: Some("Pod".to_string()),
        },
        metadata: ObjectMeta {
            name: Some(pod_name.to_string()),
            namespace: Some(namespace.to_string()),
            uid: Some(Uuid::new_v4()),
            labels: template.metadata.labels.clone(),
            annotations: template.metadata.annotations.clone(),
            owner_references: Some(vec![OwnerReference {
                api_version: "apps/v1".to_string(),
                kind: "DaemonSet".to_string(),
                name: ds.metadata.name().to_string(),
                uid: ds_uid,
                controller: Some(true),
                block_owner_deletion: Some(true),
            }]),
            ..Default::default()
        },
        spec,
        status: Some(PodStatus {
            phase: PodPhase::Pending,
            ..Default::default()
        }),
    }
}
