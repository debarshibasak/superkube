use sqlx::PgPool;
use tokio::time::{interval, Duration};
use uuid::Uuid;

use crate::db::{DeploymentRepository, EndpointsRepository, PodRepository, ServiceRepository};
use crate::models::{
    Deployment, Endpoints, EndpointAddress, EndpointPort, EndpointSubset,
    IntOrString, ObjectMeta, ObjectReference, OwnerReference, Pod, PodPhase, PodStatus,
    Service, TypeMeta,
};

/// Controller manager runs reconciliation loops for deployments and services
pub struct ControllerManager {
    pool: PgPool,
}

impl ControllerManager {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run all controllers
    pub async fn run(&self) {
        tracing::info!("Controller manager started");
        let mut interval = interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

            if let Err(e) = self.reconcile_deployments().await {
                tracing::error!("Deployment controller error: {}", e);
            }

            if let Err(e) = self.reconcile_services().await {
                tracing::error!("Service controller error: {}", e);
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
            // Need to create more pods
            let to_create = desired_replicas - current_replicas;
            tracing::info!(
                "Deployment {}/{}: creating {} pods",
                namespace,
                deployment_name,
                to_create
            );

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
            // Need to delete pods
            let to_delete = current_replicas - desired_replicas;
            tracing::info!(
                "Deployment {}/{}: deleting {} pods",
                namespace,
                deployment_name,
                to_delete
            );

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
