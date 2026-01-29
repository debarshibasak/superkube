use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::models::*;

/// Repository for Pod operations
pub struct PodRepository;

impl PodRepository {
    pub async fn create(pool: &PgPool, pod: &Pod) -> Result<Pod> {
        let meta = &pod.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = serde_json::to_value(&meta.labels)?;
        let annotations = serde_json::to_value(&meta.annotations)?;
        let spec = serde_json::to_value(&pod.spec)?;
        let status = pod
            .status
            .as_ref()
            .map(|s| s.phase.to_string())
            .unwrap_or_else(|| "Pending".to_string());
        let node_name = pod.spec.node_name.clone();
        let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());
        let host_ip = pod.status.as_ref().and_then(|s| s.host_ip.clone());
        let container_statuses = pod
            .status
            .as_ref()
            .map(|s| serde_json::to_value(&s.container_statuses).unwrap_or_default())
            .unwrap_or_default();
        let owner_reference = serde_json::to_value(&meta.owner_references)?;

        sqlx::query(
            r#"
            INSERT INTO pods (uid, name, namespace, labels, annotations, spec, status, node_name, pod_ip, host_ip, container_statuses, owner_reference)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels,
                annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec,
                status = EXCLUDED.status,
                node_name = EXCLUDED.node_name,
                pod_ip = EXCLUDED.pod_ip,
                host_ip = EXCLUDED.host_ip,
                container_statuses = EXCLUDED.container_statuses,
                owner_reference = EXCLUDED.owner_reference,
                updated_at = NOW()
            "#,
        )
        .bind(uid)
        .bind(&name)
        .bind(&namespace)
        .bind(&labels)
        .bind(&annotations)
        .bind(&spec)
        .bind(&status)
        .bind(&node_name)
        .bind(&pod_ip)
        .bind(&host_ip)
        .bind(&container_statuses)
        .bind(&owner_reference)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &PgPool, namespace: &str, name: &str) -> Result<Pod> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec, status,
                   node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at
            FROM pods
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("pods \"{}\" not found", name)))?;

        let uid: Uuid = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: serde_json::Value = row.get("labels");
        let annotations: serde_json::Value = row.get("annotations");
        let spec: serde_json::Value = row.get("spec");
        let status: String = row.get("status");
        let node_name: Option<String> = row.get("node_name");
        let pod_ip: Option<String> = row.get("pod_ip");
        let host_ip: Option<String> = row.get("host_ip");
        let container_statuses: serde_json::Value = row.get("container_statuses");
        let owner_reference: serde_json::Value = row.get("owner_reference");
        let created_at: DateTime<Utc> = row.get("created_at");

        let mut pod = Pod {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Pod".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(uid),
                labels: serde_json::from_value(labels).ok(),
                annotations: serde_json::from_value(annotations).ok(),
                creation_timestamp: Some(created_at),
                owner_references: serde_json::from_value(owner_reference).ok(),
                ..Default::default()
            },
            spec: serde_json::from_value(spec).unwrap_or_default(),
            status: Some(PodStatus {
                phase: match status.as_str() {
                    "Running" => PodPhase::Running,
                    "Succeeded" => PodPhase::Succeeded,
                    "Failed" => PodPhase::Failed,
                    "Unknown" => PodPhase::Unknown,
                    _ => PodPhase::Pending,
                },
                pod_ip,
                host_ip,
                container_statuses: serde_json::from_value(container_statuses).ok(),
                ..Default::default()
            }),
        };

        if let Some(node) = node_name {
            pod.spec.node_name = Some(node);
        }

        Ok(pod)
    }

    pub async fn list(
        pool: &PgPool,
        namespace: Option<&str>,
        label_selector: Option<&HashMap<String, String>>,
    ) -> Result<Vec<Pod>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec, status,
                       node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at
                FROM pods
                WHERE namespace = $1
                ORDER BY created_at DESC
                "#,
            )
            .bind(ns)
            .fetch_all(pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec, status,
                       node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at
                FROM pods
                ORDER BY created_at DESC
                "#,
            )
            .fetch_all(pool)
            .await?
        };

        let mut pods = Vec::new();
        for row in rows {
            let uid: Uuid = row.get("uid");
            let name: String = row.get("name");
            let ns: String = row.get("namespace");
            let labels_val: serde_json::Value = row.get("labels");
            let annotations: serde_json::Value = row.get("annotations");
            let spec: serde_json::Value = row.get("spec");
            let status: String = row.get("status");
            let node_name: Option<String> = row.get("node_name");
            let pod_ip: Option<String> = row.get("pod_ip");
            let host_ip: Option<String> = row.get("host_ip");
            let container_statuses: serde_json::Value = row.get("container_statuses");
            let owner_reference: serde_json::Value = row.get("owner_reference");
            let created_at: DateTime<Utc> = row.get("created_at");

            let labels: Option<HashMap<String, String>> =
                serde_json::from_value(labels_val.clone()).ok();

            // Filter by label selector if provided
            if let Some(selector) = label_selector {
                if let Some(ref pod_labels) = labels {
                    let matches = selector.iter().all(|(k, v)| pod_labels.get(k) == Some(v));
                    if !matches {
                        continue;
                    }
                } else {
                    continue;
                }
            }

            let mut pod = Pod {
                type_meta: TypeMeta {
                    api_version: Some("v1".to_string()),
                    kind: Some("Pod".to_string()),
                },
                metadata: ObjectMeta {
                    name: Some(name),
                    namespace: Some(ns),
                    uid: Some(uid),
                    labels,
                    annotations: serde_json::from_value(annotations).ok(),
                    creation_timestamp: Some(created_at),
                    owner_references: serde_json::from_value(owner_reference).ok(),
                    ..Default::default()
                },
                spec: serde_json::from_value(spec).unwrap_or_default(),
                status: Some(PodStatus {
                    phase: match status.as_str() {
                        "Running" => PodPhase::Running,
                        "Succeeded" => PodPhase::Succeeded,
                        "Failed" => PodPhase::Failed,
                        "Unknown" => PodPhase::Unknown,
                        _ => PodPhase::Pending,
                    },
                    pod_ip,
                    host_ip,
                    container_statuses: serde_json::from_value(container_statuses).ok(),
                    ..Default::default()
                }),
            };

            if let Some(node) = node_name {
                pod.spec.node_name = Some(node);
            }

            pods.push(pod);
        }

        Ok(pods)
    }

    pub async fn delete(pool: &PgPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM pods WHERE namespace = $1 AND name = $2")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("pods \"{}\" not found", name)));
        }

        Ok(())
    }

    pub async fn update_status(
        pool: &PgPool,
        namespace: &str,
        name: &str,
        status: &PodStatus,
    ) -> Result<()> {
        let phase = status.phase.to_string();
        let container_statuses = serde_json::to_value(&status.container_statuses)?;

        sqlx::query(
            r#"
            UPDATE pods
            SET status = $3, pod_ip = $4, host_ip = $5, container_statuses = $6, updated_at = NOW()
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .bind(&phase)
        .bind(&status.pod_ip)
        .bind(&status.host_ip)
        .bind(&container_statuses)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn bind_to_node(
        pool: &PgPool,
        namespace: &str,
        name: &str,
        node_name: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE pods
            SET node_name = $3, updated_at = NOW()
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .bind(node_name)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn list_unscheduled(pool: &PgPool) -> Result<Vec<Pod>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec, status,
                   node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at
            FROM pods
            WHERE node_name IS NULL AND status = 'Pending'
            ORDER BY created_at ASC
            "#,
        )
        .fetch_all(pool)
        .await?;

        let mut pods = Vec::new();
        for row in rows {
            let uid: Uuid = row.get("uid");
            let name: String = row.get("name");
            let ns: String = row.get("namespace");
            let labels: serde_json::Value = row.get("labels");
            let annotations: serde_json::Value = row.get("annotations");
            let spec: serde_json::Value = row.get("spec");
            let pod_ip: Option<String> = row.get("pod_ip");
            let host_ip: Option<String> = row.get("host_ip");
            let container_statuses: serde_json::Value = row.get("container_statuses");
            let owner_reference: serde_json::Value = row.get("owner_reference");
            let created_at: DateTime<Utc> = row.get("created_at");

            let pod = Pod {
                type_meta: TypeMeta {
                    api_version: Some("v1".to_string()),
                    kind: Some("Pod".to_string()),
                },
                metadata: ObjectMeta {
                    name: Some(name),
                    namespace: Some(ns),
                    uid: Some(uid),
                    labels: serde_json::from_value(labels).ok(),
                    annotations: serde_json::from_value(annotations).ok(),
                    creation_timestamp: Some(created_at),
                    owner_references: serde_json::from_value(owner_reference).ok(),
                    ..Default::default()
                },
                spec: serde_json::from_value(spec).unwrap_or_default(),
                status: Some(PodStatus {
                    phase: PodPhase::Pending,
                    pod_ip,
                    host_ip,
                    container_statuses: serde_json::from_value(container_statuses).ok(),
                    ..Default::default()
                }),
            };
            pods.push(pod);
        }

        Ok(pods)
    }

    pub async fn list_by_node(pool: &PgPool, node_name: &str) -> Result<Vec<Pod>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec, status,
                   node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at
            FROM pods
            WHERE node_name = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(node_name)
        .fetch_all(pool)
        .await?;

        let mut pods = Vec::new();
        for row in rows {
            let uid: Uuid = row.get("uid");
            let name: String = row.get("name");
            let ns: String = row.get("namespace");
            let labels: serde_json::Value = row.get("labels");
            let annotations: serde_json::Value = row.get("annotations");
            let spec: serde_json::Value = row.get("spec");
            let status: String = row.get("status");
            let node_name_val: Option<String> = row.get("node_name");
            let pod_ip: Option<String> = row.get("pod_ip");
            let host_ip: Option<String> = row.get("host_ip");
            let container_statuses: serde_json::Value = row.get("container_statuses");
            let owner_reference: serde_json::Value = row.get("owner_reference");
            let created_at: DateTime<Utc> = row.get("created_at");

            let mut pod = Pod {
                type_meta: TypeMeta {
                    api_version: Some("v1".to_string()),
                    kind: Some("Pod".to_string()),
                },
                metadata: ObjectMeta {
                    name: Some(name),
                    namespace: Some(ns),
                    uid: Some(uid),
                    labels: serde_json::from_value(labels).ok(),
                    annotations: serde_json::from_value(annotations).ok(),
                    creation_timestamp: Some(created_at),
                    owner_references: serde_json::from_value(owner_reference).ok(),
                    ..Default::default()
                },
                spec: serde_json::from_value(spec).unwrap_or_default(),
                status: Some(PodStatus {
                    phase: match status.as_str() {
                        "Running" => PodPhase::Running,
                        "Succeeded" => PodPhase::Succeeded,
                        "Failed" => PodPhase::Failed,
                        "Unknown" => PodPhase::Unknown,
                        _ => PodPhase::Pending,
                    },
                    pod_ip,
                    host_ip,
                    container_statuses: serde_json::from_value(container_statuses).ok(),
                    ..Default::default()
                }),
            };

            if let Some(node) = node_name_val {
                pod.spec.node_name = Some(node);
            }

            pods.push(pod);
        }

        Ok(pods)
    }
}

/// Repository for Deployment operations
pub struct DeploymentRepository;

impl DeploymentRepository {
    pub async fn create(pool: &PgPool, deployment: &Deployment) -> Result<Deployment> {
        let meta = &deployment.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = serde_json::to_value(&meta.labels)?;
        let annotations = serde_json::to_value(&meta.annotations)?;
        let spec = serde_json::to_value(&deployment.spec)?;
        let replicas = deployment.spec.replicas();

        sqlx::query(
            r#"
            INSERT INTO deployments (uid, name, namespace, labels, annotations, spec, replicas)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels,
                annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec,
                replicas = EXCLUDED.replicas,
                updated_at = NOW()
            "#,
        )
        .bind(uid)
        .bind(&name)
        .bind(&namespace)
        .bind(&labels)
        .bind(&annotations)
        .bind(&spec)
        .bind(replicas)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &PgPool, namespace: &str, name: &str) -> Result<Deployment> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec,
                   replicas, ready_replicas, available_replicas, created_at
            FROM deployments
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("deployments.apps \"{}\" not found", name)))?;

        let uid: Uuid = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: serde_json::Value = row.get("labels");
        let annotations: serde_json::Value = row.get("annotations");
        let spec: serde_json::Value = row.get("spec");
        let replicas: i32 = row.get("replicas");
        let ready_replicas: i32 = row.get("ready_replicas");
        let available_replicas: i32 = row.get("available_replicas");
        let created_at: DateTime<Utc> = row.get("created_at");

        Ok(Deployment {
            type_meta: TypeMeta {
                api_version: Some("apps/v1".to_string()),
                kind: Some("Deployment".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(uid),
                labels: serde_json::from_value(labels).ok(),
                annotations: serde_json::from_value(annotations).ok(),
                creation_timestamp: Some(created_at),
                ..Default::default()
            },
            spec: serde_json::from_value(spec).unwrap_or_default(),
            status: Some(DeploymentStatus {
                replicas,
                ready_replicas,
                available_replicas,
                ..Default::default()
            }),
        })
    }

    pub async fn list(pool: &PgPool, namespace: Option<&str>) -> Result<Vec<Deployment>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec,
                       replicas, ready_replicas, available_replicas, created_at
                FROM deployments
                WHERE namespace = $1
                ORDER BY created_at DESC
                "#,
            )
            .bind(ns)
            .fetch_all(pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec,
                       replicas, ready_replicas, available_replicas, created_at
                FROM deployments
                ORDER BY created_at DESC
                "#,
            )
            .fetch_all(pool)
            .await?
        };

        let deployments = rows
            .into_iter()
            .map(|row| {
                let uid: Uuid = row.get("uid");
                let name: String = row.get("name");
                let ns: String = row.get("namespace");
                let labels: serde_json::Value = row.get("labels");
                let annotations: serde_json::Value = row.get("annotations");
                let spec: serde_json::Value = row.get("spec");
                let replicas: i32 = row.get("replicas");
                let ready_replicas: i32 = row.get("ready_replicas");
                let available_replicas: i32 = row.get("available_replicas");
                let created_at: DateTime<Utc> = row.get("created_at");

                Deployment {
                    type_meta: TypeMeta {
                        api_version: Some("apps/v1".to_string()),
                        kind: Some("Deployment".to_string()),
                    },
                    metadata: ObjectMeta {
                        name: Some(name),
                        namespace: Some(ns),
                        uid: Some(uid),
                        labels: serde_json::from_value(labels).ok(),
                        annotations: serde_json::from_value(annotations).ok(),
                        creation_timestamp: Some(created_at),
                        ..Default::default()
                    },
                    spec: serde_json::from_value(spec).unwrap_or_default(),
                    status: Some(DeploymentStatus {
                        replicas,
                        ready_replicas,
                        available_replicas,
                        ..Default::default()
                    }),
                }
            })
            .collect();

        Ok(deployments)
    }

    pub async fn delete(pool: &PgPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM deployments WHERE namespace = $1 AND name = $2")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!(
                "deployments.apps \"{}\" not found",
                name
            )));
        }

        Ok(())
    }

    pub async fn update_status(
        pool: &PgPool,
        namespace: &str,
        name: &str,
        ready_replicas: i32,
        available_replicas: i32,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE deployments
            SET ready_replicas = $3, available_replicas = $4, updated_at = NOW()
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .bind(ready_replicas)
        .bind(available_replicas)
        .execute(pool)
        .await?;

        Ok(())
    }
}

/// Repository for Service operations
pub struct ServiceRepository;

impl ServiceRepository {
    pub async fn create(pool: &PgPool, service: &Service) -> Result<Service> {
        let meta = &service.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = serde_json::to_value(&meta.labels)?;
        let spec = serde_json::to_value(&service.spec)?;
        let service_type = match service.spec.service_type {
            ServiceType::ClusterIP => "ClusterIP",
            ServiceType::NodePort => "NodePort",
            ServiceType::LoadBalancer => "LoadBalancer",
            ServiceType::ExternalName => "ExternalName",
        };
        let cluster_ip = service.spec.cluster_ip.clone();

        // Auto-assign NodePort if needed
        let node_port = if service.spec.service_type == ServiceType::NodePort {
            service
                .spec
                .ports
                .first()
                .and_then(|p| p.node_port)
                .or(Some(30000 + (uid.as_u128() % 2768) as i32))
        } else {
            None
        };

        sqlx::query(
            r#"
            INSERT INTO services (uid, name, namespace, labels, spec, service_type, cluster_ip, node_port)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels,
                spec = EXCLUDED.spec,
                service_type = EXCLUDED.service_type,
                cluster_ip = EXCLUDED.cluster_ip,
                node_port = EXCLUDED.node_port,
                updated_at = NOW()
            "#,
        )
        .bind(uid)
        .bind(&name)
        .bind(&namespace)
        .bind(&labels)
        .bind(&spec)
        .bind(service_type)
        .bind(&cluster_ip)
        .bind(node_port)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &PgPool, namespace: &str, name: &str) -> Result<Service> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, spec, service_type,
                   cluster_ip, node_port, created_at
            FROM services
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("services \"{}\" not found", name)))?;

        let uid: Uuid = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: serde_json::Value = row.get("labels");
        let spec_val: serde_json::Value = row.get("spec");
        let cluster_ip: Option<String> = row.get("cluster_ip");
        let node_port: Option<i32> = row.get("node_port");
        let created_at: DateTime<Utc> = row.get("created_at");

        let mut spec: ServiceSpec = serde_json::from_value(spec_val).unwrap_or_default();
        spec.cluster_ip = cluster_ip;

        if let Some(np) = node_port {
            if let Some(first_port) = spec.ports.first_mut() {
                first_port.node_port = Some(np);
            }
        }

        Ok(Service {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Service".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(uid),
                labels: serde_json::from_value(labels).ok(),
                creation_timestamp: Some(created_at),
                ..Default::default()
            },
            spec,
            status: None,
        })
    }

    pub async fn list(pool: &PgPool, namespace: Option<&str>) -> Result<Vec<Service>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, spec, service_type,
                       cluster_ip, node_port, created_at
                FROM services
                WHERE namespace = $1
                ORDER BY created_at DESC
                "#,
            )
            .bind(ns)
            .fetch_all(pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, spec, service_type,
                       cluster_ip, node_port, created_at
                FROM services
                ORDER BY created_at DESC
                "#,
            )
            .fetch_all(pool)
            .await?
        };

        let services = rows
            .into_iter()
            .map(|row| {
                let uid: Uuid = row.get("uid");
                let name: String = row.get("name");
                let ns: String = row.get("namespace");
                let labels: serde_json::Value = row.get("labels");
                let spec_val: serde_json::Value = row.get("spec");
                let cluster_ip: Option<String> = row.get("cluster_ip");
                let node_port: Option<i32> = row.get("node_port");
                let created_at: DateTime<Utc> = row.get("created_at");

                let mut spec: ServiceSpec = serde_json::from_value(spec_val).unwrap_or_default();
                spec.cluster_ip = cluster_ip;

                if let Some(np) = node_port {
                    if let Some(first_port) = spec.ports.first_mut() {
                        first_port.node_port = Some(np);
                    }
                }

                Service {
                    type_meta: TypeMeta {
                        api_version: Some("v1".to_string()),
                        kind: Some("Service".to_string()),
                    },
                    metadata: ObjectMeta {
                        name: Some(name),
                        namespace: Some(ns),
                        uid: Some(uid),
                        labels: serde_json::from_value(labels).ok(),
                        creation_timestamp: Some(created_at),
                        ..Default::default()
                    },
                    spec,
                    status: None,
                }
            })
            .collect();

        Ok(services)
    }

    pub async fn delete(pool: &PgPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM services WHERE namespace = $1 AND name = $2")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("services \"{}\" not found", name)));
        }

        Ok(())
    }
}

/// Repository for Node operations
pub struct NodeRepository;

impl NodeRepository {
    pub async fn create(pool: &PgPool, node: &Node) -> Result<Node> {
        let meta = &node.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let labels = serde_json::to_value(&meta.labels)?;
        let status = node
            .status
            .as_ref()
            .map(|_| "Ready")
            .unwrap_or("NotReady");
        let addresses = node
            .status
            .as_ref()
            .map(|s| serde_json::to_value(&s.addresses).unwrap_or_default())
            .unwrap_or_default();
        let capacity = node
            .status
            .as_ref()
            .map(|s| serde_json::to_value(&s.capacity).unwrap_or_default())
            .unwrap_or_default();
        let allocatable = node
            .status
            .as_ref()
            .map(|s| serde_json::to_value(&s.allocatable).unwrap_or_default())
            .unwrap_or_default();

        sqlx::query(
            r#"
            INSERT INTO nodes (uid, name, labels, status, addresses, capacity, allocatable)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (name) DO UPDATE SET
                labels = EXCLUDED.labels,
                status = EXCLUDED.status,
                addresses = EXCLUDED.addresses,
                capacity = EXCLUDED.capacity,
                allocatable = EXCLUDED.allocatable,
                updated_at = NOW()
            "#,
        )
        .bind(uid)
        .bind(&name)
        .bind(&labels)
        .bind(status)
        .bind(&addresses)
        .bind(&capacity)
        .bind(&allocatable)
        .execute(pool)
        .await?;

        Self::get(pool, &name).await
    }

    pub async fn get(pool: &PgPool, name: &str) -> Result<Node> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, labels, status, addresses, capacity, allocatable, created_at, updated_at
            FROM nodes
            WHERE name = $1
            "#,
        )
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("nodes \"{}\" not found", name)))?;

        let uid: Uuid = row.get("uid");
        let name: String = row.get("name");
        let labels: serde_json::Value = row.get("labels");
        let status: String = row.get("status");
        let addresses: serde_json::Value = row.get("addresses");
        let capacity: serde_json::Value = row.get("capacity");
        let allocatable: serde_json::Value = row.get("allocatable");
        let created_at: DateTime<Utc> = row.get("created_at");
        let updated_at: DateTime<Utc> = row.get("updated_at");

        let conditions = vec![NodeCondition {
            condition_type: NodeConditionType::Ready,
            status: if status == "Ready" {
                ConditionStatus::True
            } else {
                ConditionStatus::False
            },
            last_heartbeat_time: Some(updated_at),
            last_transition_time: Some(created_at),
            reason: Some("KubeletReady".to_string()),
            message: Some("kubelet is posting ready status".to_string()),
        }];

        Ok(Node {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Node".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                uid: Some(uid),
                labels: serde_json::from_value(labels).ok(),
                creation_timestamp: Some(created_at),
                ..Default::default()
            },
            spec: None,
            status: Some(NodeStatus {
                capacity: serde_json::from_value(capacity).ok(),
                allocatable: serde_json::from_value(allocatable).ok(),
                conditions: Some(conditions),
                addresses: serde_json::from_value(addresses).ok(),
                ..Default::default()
            }),
        })
    }

    pub async fn list(pool: &PgPool) -> Result<Vec<Node>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, labels, status, addresses, capacity, allocatable, created_at, updated_at
            FROM nodes
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(pool)
        .await?;

        let nodes = rows
            .into_iter()
            .map(|row| {
                let uid: Uuid = row.get("uid");
                let name: String = row.get("name");
                let labels: serde_json::Value = row.get("labels");
                let status: String = row.get("status");
                let addresses: serde_json::Value = row.get("addresses");
                let capacity: serde_json::Value = row.get("capacity");
                let allocatable: serde_json::Value = row.get("allocatable");
                let created_at: DateTime<Utc> = row.get("created_at");
                let updated_at: DateTime<Utc> = row.get("updated_at");

                let conditions = vec![NodeCondition {
                    condition_type: NodeConditionType::Ready,
                    status: if status == "Ready" {
                        ConditionStatus::True
                    } else {
                        ConditionStatus::False
                    },
                    last_heartbeat_time: Some(updated_at),
                    last_transition_time: Some(created_at),
                    reason: Some("KubeletReady".to_string()),
                    message: Some("kubelet is posting ready status".to_string()),
                }];

                Node {
                    type_meta: TypeMeta {
                        api_version: Some("v1".to_string()),
                        kind: Some("Node".to_string()),
                    },
                    metadata: ObjectMeta {
                        name: Some(name),
                        uid: Some(uid),
                        labels: serde_json::from_value(labels).ok(),
                        creation_timestamp: Some(created_at),
                        ..Default::default()
                    },
                    spec: None,
                    status: Some(NodeStatus {
                        capacity: serde_json::from_value(capacity).ok(),
                        allocatable: serde_json::from_value(allocatable).ok(),
                        conditions: Some(conditions),
                        addresses: serde_json::from_value(addresses).ok(),
                        ..Default::default()
                    }),
                }
            })
            .collect();

        Ok(nodes)
    }

    pub async fn delete(pool: &PgPool, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM nodes WHERE name = $1")
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("nodes \"{}\" not found", name)));
        }

        Ok(())
    }

    pub async fn update_heartbeat(pool: &PgPool, name: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE nodes
            SET status = 'Ready', updated_at = NOW()
            WHERE name = $1
            "#,
        )
        .bind(name)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn list_ready(pool: &PgPool) -> Result<Vec<Node>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, labels, status, addresses, capacity, allocatable, created_at, updated_at
            FROM nodes
            WHERE status = 'Ready'
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(pool)
        .await?;

        let nodes = rows
            .into_iter()
            .map(|row| {
                let uid: Uuid = row.get("uid");
                let name: String = row.get("name");
                let labels: serde_json::Value = row.get("labels");
                let addresses: serde_json::Value = row.get("addresses");
                let capacity: serde_json::Value = row.get("capacity");
                let allocatable: serde_json::Value = row.get("allocatable");
                let created_at: DateTime<Utc> = row.get("created_at");

                Node {
                    type_meta: TypeMeta {
                        api_version: Some("v1".to_string()),
                        kind: Some("Node".to_string()),
                    },
                    metadata: ObjectMeta {
                        name: Some(name),
                        uid: Some(uid),
                        labels: serde_json::from_value(labels).ok(),
                        creation_timestamp: Some(created_at),
                        ..Default::default()
                    },
                    spec: None,
                    status: Some(NodeStatus {
                        capacity: serde_json::from_value(capacity).ok(),
                        allocatable: serde_json::from_value(allocatable).ok(),
                        addresses: serde_json::from_value(addresses).ok(),
                        ..Default::default()
                    }),
                }
            })
            .collect();

        Ok(nodes)
    }
}

/// Repository for Endpoints operations
pub struct EndpointsRepository;

impl EndpointsRepository {
    pub async fn create_or_update(pool: &PgPool, endpoints: &Endpoints) -> Result<Endpoints> {
        let meta = &endpoints.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let subsets = serde_json::to_value(&endpoints.subsets)?;

        sqlx::query(
            r#"
            INSERT INTO endpoints (uid, name, namespace, subsets)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (name, namespace) DO UPDATE SET
                subsets = EXCLUDED.subsets,
                updated_at = NOW()
            "#,
        )
        .bind(uid)
        .bind(&name)
        .bind(&namespace)
        .bind(&subsets)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &PgPool, namespace: &str, name: &str) -> Result<Endpoints> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, subsets, created_at
            FROM endpoints
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("endpoints \"{}\" not found", name)))?;

        let uid: Uuid = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let subsets: serde_json::Value = row.get("subsets");
        let created_at: DateTime<Utc> = row.get("created_at");

        Ok(Endpoints {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Endpoints".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(uid),
                creation_timestamp: Some(created_at),
                ..Default::default()
            },
            subsets: serde_json::from_value(subsets).unwrap_or_default(),
        })
    }

    pub async fn delete(pool: &PgPool, namespace: &str, name: &str) -> Result<()> {
        sqlx::query("DELETE FROM endpoints WHERE namespace = $1 AND name = $2")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;

        Ok(())
    }
}

/// Repository for Namespace operations
pub struct NamespaceRepository;

impl NamespaceRepository {
    pub async fn create(pool: &PgPool, namespace: &Namespace) -> Result<Namespace> {
        let meta = &namespace.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let labels = serde_json::to_value(&meta.labels)?;
        let annotations = serde_json::to_value(&meta.annotations)?;
        let spec = serde_json::to_value(&namespace.spec)?;
        let status = namespace
            .status
            .as_ref()
            .map(|s| s.phase.to_string())
            .unwrap_or_else(|| "Active".to_string());

        sqlx::query(
            r#"
            INSERT INTO namespaces (uid, name, labels, annotations, spec, status)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (name) DO UPDATE SET
                labels = EXCLUDED.labels,
                annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec,
                status = EXCLUDED.status,
                updated_at = NOW()
            "#,
        )
        .bind(uid)
        .bind(&name)
        .bind(&labels)
        .bind(&annotations)
        .bind(&spec)
        .bind(&status)
        .execute(pool)
        .await?;

        Self::get(pool, &name).await
    }

    pub async fn get(pool: &PgPool, name: &str) -> Result<Namespace> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, labels, annotations, spec, status, created_at
            FROM namespaces
            WHERE name = $1
            "#,
        )
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("namespaces \"{}\" not found", name)))?;

        let uid: Uuid = row.get("uid");
        let name: String = row.get("name");
        let labels: serde_json::Value = row.get("labels");
        let annotations: serde_json::Value = row.get("annotations");
        let spec: serde_json::Value = row.get("spec");
        let status: String = row.get("status");
        let created_at: DateTime<Utc> = row.get("created_at");

        Ok(Namespace {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Namespace".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                uid: Some(uid),
                labels: serde_json::from_value(labels).ok(),
                annotations: serde_json::from_value(annotations).ok(),
                creation_timestamp: Some(created_at),
                ..Default::default()
            },
            spec: serde_json::from_value(spec).ok(),
            status: Some(NamespaceStatus {
                phase: match status.as_str() {
                    "Terminating" => NamespacePhase::Terminating,
                    _ => NamespacePhase::Active,
                },
                conditions: None,
            }),
        })
    }

    pub async fn list(pool: &PgPool) -> Result<Vec<Namespace>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, labels, annotations, spec, status, created_at
            FROM namespaces
            ORDER BY created_at ASC
            "#,
        )
        .fetch_all(pool)
        .await?;

        let namespaces = rows
            .into_iter()
            .map(|row| {
                let uid: Uuid = row.get("uid");
                let name: String = row.get("name");
                let labels: serde_json::Value = row.get("labels");
                let annotations: serde_json::Value = row.get("annotations");
                let spec: serde_json::Value = row.get("spec");
                let status: String = row.get("status");
                let created_at: DateTime<Utc> = row.get("created_at");

                Namespace {
                    type_meta: TypeMeta {
                        api_version: Some("v1".to_string()),
                        kind: Some("Namespace".to_string()),
                    },
                    metadata: ObjectMeta {
                        name: Some(name),
                        uid: Some(uid),
                        labels: serde_json::from_value(labels).ok(),
                        annotations: serde_json::from_value(annotations).ok(),
                        creation_timestamp: Some(created_at),
                        ..Default::default()
                    },
                    spec: serde_json::from_value(spec).ok(),
                    status: Some(NamespaceStatus {
                        phase: match status.as_str() {
                            "Terminating" => NamespacePhase::Terminating,
                            _ => NamespacePhase::Active,
                        },
                        conditions: None,
                    }),
                }
            })
            .collect();

        Ok(namespaces)
    }

    pub async fn delete(pool: &PgPool, name: &str) -> Result<()> {
        // Prevent deletion of system namespaces
        if matches!(name, "default" | "kube-system" | "kube-public" | "kube-node-lease") {
            return Err(Error::BadRequest(format!(
                "namespace \"{}\" is a system namespace and cannot be deleted",
                name
            )));
        }

        // Check if namespace has resources
        let pod_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pods WHERE namespace = $1")
            .bind(name)
            .fetch_one(pool)
            .await?;

        if pod_count.0 > 0 {
            return Err(Error::BadRequest(format!(
                "namespace \"{}\" still has {} pods",
                name, pod_count.0
            )));
        }

        let result = sqlx::query("DELETE FROM namespaces WHERE name = $1")
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("namespaces \"{}\" not found", name)));
        }

        Ok(())
    }

    pub async fn exists(pool: &PgPool, name: &str) -> Result<bool> {
        let result: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM namespaces WHERE name = $1 AND status = 'Active'",
        )
        .bind(name)
        .fetch_one(pool)
        .await?;

        Ok(result.0 > 0)
    }
}

/// Repository for Event operations
pub struct EventRepository;

impl EventRepository {
    pub async fn create(pool: &PgPool, event: &Event) -> Result<Event> {
        let meta = &event.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();

        let involved_object = &event.involved_object;
        let source = event.source.as_ref();
        let event_type = event
            .event_type
            .as_ref()
            .map(|t| match t {
                EventType::Normal => "Normal",
                EventType::Warning => "Warning",
            })
            .unwrap_or("Normal");

        sqlx::query(
            r#"
            INSERT INTO events (
                uid, name, namespace,
                involved_object_api_version, involved_object_kind, involved_object_name,
                involved_object_namespace, involved_object_uid, involved_object_resource_version,
                involved_object_field_path,
                reason, message,
                source_component, source_host,
                first_timestamp, last_timestamp, event_time,
                count, event_type, action,
                reporting_controller, reporting_instance
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)
            ON CONFLICT (name, namespace) DO UPDATE SET
                last_timestamp = EXCLUDED.last_timestamp,
                count = events.count + 1,
                message = EXCLUDED.message,
                updated_at = NOW()
            "#,
        )
        .bind(uid)
        .bind(&name)
        .bind(&namespace)
        .bind(&involved_object.api_version)
        .bind(&involved_object.kind)
        .bind(&involved_object.name)
        .bind(&involved_object.namespace)
        .bind(involved_object.uid.map(|u| u.to_string()))
        .bind(&involved_object.resource_version)
        .bind(&involved_object.field_path)
        .bind(&event.reason)
        .bind(&event.message)
        .bind(source.and_then(|s| s.component.as_ref()))
        .bind(source.and_then(|s| s.host.as_ref()))
        .bind(&event.first_timestamp)
        .bind(&event.last_timestamp)
        .bind(&event.event_time)
        .bind(event.count.unwrap_or(1))
        .bind(event_type)
        .bind(&event.action)
        .bind(&event.reporting_controller)
        .bind(&event.reporting_instance)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &PgPool, namespace: &str, name: &str) -> Result<Event> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace,
                   involved_object_api_version, involved_object_kind, involved_object_name,
                   involved_object_namespace, involved_object_uid, involved_object_resource_version,
                   involved_object_field_path,
                   reason, message,
                   source_component, source_host,
                   first_timestamp, last_timestamp, event_time,
                   count, event_type, action,
                   reporting_controller, reporting_instance,
                   created_at
            FROM events
            WHERE namespace = $1 AND name = $2
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("events \"{}\" not found", name)))?;

        Self::row_to_event(row)
    }

    pub async fn list(
        pool: &PgPool,
        namespace: Option<&str>,
        involved_object_name: Option<&str>,
        involved_object_kind: Option<&str>,
    ) -> Result<Vec<Event>> {
        let rows = if let Some(ns) = namespace {
            if let (Some(obj_name), Some(obj_kind)) = (involved_object_name, involved_object_kind) {
                sqlx::query(
                    r#"
                    SELECT uid, name, namespace,
                           involved_object_api_version, involved_object_kind, involved_object_name,
                           involved_object_namespace, involved_object_uid, involved_object_resource_version,
                           involved_object_field_path,
                           reason, message,
                           source_component, source_host,
                           first_timestamp, last_timestamp, event_time,
                           count, event_type, action,
                           reporting_controller, reporting_instance,
                           created_at
                    FROM events
                    WHERE namespace = $1 AND involved_object_name = $2 AND involved_object_kind = $3
                    ORDER BY last_timestamp DESC
                    "#,
                )
                .bind(ns)
                .bind(obj_name)
                .bind(obj_kind)
                .fetch_all(pool)
                .await?
            } else {
                sqlx::query(
                    r#"
                    SELECT uid, name, namespace,
                           involved_object_api_version, involved_object_kind, involved_object_name,
                           involved_object_namespace, involved_object_uid, involved_object_resource_version,
                           involved_object_field_path,
                           reason, message,
                           source_component, source_host,
                           first_timestamp, last_timestamp, event_time,
                           count, event_type, action,
                           reporting_controller, reporting_instance,
                           created_at
                    FROM events
                    WHERE namespace = $1
                    ORDER BY last_timestamp DESC
                    "#,
                )
                .bind(ns)
                .fetch_all(pool)
                .await?
            }
        } else {
            sqlx::query(
                r#"
                SELECT uid, name, namespace,
                       involved_object_api_version, involved_object_kind, involved_object_name,
                       involved_object_namespace, involved_object_uid, involved_object_resource_version,
                       involved_object_field_path,
                       reason, message,
                       source_component, source_host,
                       first_timestamp, last_timestamp, event_time,
                       count, event_type, action,
                       reporting_controller, reporting_instance,
                       created_at
                FROM events
                ORDER BY last_timestamp DESC
                "#,
            )
            .fetch_all(pool)
            .await?
        };

        let mut events = Vec::new();
        for row in rows {
            events.push(Self::row_to_event(row)?);
        }

        Ok(events)
    }

    pub async fn delete(pool: &PgPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM events WHERE namespace = $1 AND name = $2")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("events \"{}\" not found", name)));
        }

        Ok(())
    }

    fn row_to_event(row: sqlx::postgres::PgRow) -> Result<Event> {
        let uid: Uuid = row.get("uid");
        let name: String = row.get("name");
        let namespace: String = row.get("namespace");

        let involved_object_api_version: Option<String> = row.get("involved_object_api_version");
        let involved_object_kind: Option<String> = row.get("involved_object_kind");
        let involved_object_name: Option<String> = row.get("involved_object_name");
        let involved_object_namespace: Option<String> = row.get("involved_object_namespace");
        let involved_object_uid: Option<String> = row.get("involved_object_uid");
        let involved_object_resource_version: Option<String> =
            row.get("involved_object_resource_version");
        let involved_object_field_path: Option<String> = row.get("involved_object_field_path");

        let reason: Option<String> = row.get("reason");
        let message: Option<String> = row.get("message");

        let source_component: Option<String> = row.get("source_component");
        let source_host: Option<String> = row.get("source_host");

        let first_timestamp: Option<DateTime<Utc>> = row.get("first_timestamp");
        let last_timestamp: Option<DateTime<Utc>> = row.get("last_timestamp");
        let event_time: Option<DateTime<Utc>> = row.get("event_time");

        let count: Option<i32> = row.get("count");
        let event_type_str: Option<String> = row.get("event_type");
        let action: Option<String> = row.get("action");

        let reporting_controller: Option<String> = row.get("reporting_controller");
        let reporting_instance: Option<String> = row.get("reporting_instance");

        let created_at: DateTime<Utc> = row.get("created_at");

        let source = if source_component.is_some() || source_host.is_some() {
            Some(EventSource {
                component: source_component,
                host: source_host,
            })
        } else {
            None
        };

        let event_type = event_type_str.map(|t| match t.as_str() {
            "Warning" => EventType::Warning,
            _ => EventType::Normal,
        });

        // Convert uid string to Uuid if present
        let involved_object_uid_parsed = involved_object_uid
            .and_then(|s| Uuid::parse_str(&s).ok());

        Ok(Event {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Event".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace),
                uid: Some(uid),
                creation_timestamp: Some(created_at),
                ..Default::default()
            },
            involved_object: ObjectReference {
                api_version: involved_object_api_version,
                kind: involved_object_kind,
                name: involved_object_name,
                namespace: involved_object_namespace,
                uid: involved_object_uid_parsed,
                resource_version: involved_object_resource_version,
                field_path: involved_object_field_path,
            },
            reason,
            message,
            source,
            first_timestamp,
            last_timestamp,
            count,
            event_type,
            event_time,
            action,
            reporting_controller,
            reporting_instance,
        })
    }
}
