use chrono::{DateTime, Utc};
use sqlx::{AnyPool, Row};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::models::*;

// ----------------------------------------------------------------------------
// Helpers — sqlx::Any only supports primitive types, so UUIDs, JSON values
// and timestamps are stored as TEXT and converted on the boundary.
// ----------------------------------------------------------------------------

fn now_str() -> String {
    Utc::now().to_rfc3339()
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn parse_dt_opt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_uid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or(Uuid::nil())
}

fn json_str<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}

fn json_from<T: serde::de::DeserializeOwned + Default>(s: &str) -> T {
    if s.is_empty() {
        T::default()
    } else {
        serde_json::from_str(s).unwrap_or_default()
    }
}

fn json_from_opt<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    if s.is_empty() || s == "null" {
        None
    } else {
        serde_json::from_str(s).ok()
    }
}

/// Repository for Pod operations
pub struct PodRepository;

impl PodRepository {
    pub async fn create(pool: &AnyPool, pod: &Pod) -> Result<Pod> {
        let meta = &pod.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(&pod.spec)?;
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
            .map(|s| json_str(&s.container_statuses).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| "[]".to_string());
        let owner_reference = json_str(&meta.owner_references)?;
        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO pods (uid, name, namespace, labels, annotations, spec, status, node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
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
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<Pod> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec, status,
                   node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at
            FROM pods
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("pods \"{}\" not found", name)))?;

        Ok(Self::row_to_pod(row))
    }

    pub async fn list(
        pool: &AnyPool,
        namespace: Option<&str>,
        label_selector: Option<&HashMap<String, String>>,
    ) -> Result<Vec<Pod>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec, status,
                       node_name, pod_ip, host_ip, container_statuses, owner_reference, created_at
                FROM pods
                WHERE namespace = ?
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
            let pod = Self::row_to_pod(row);

            if let Some(selector) = label_selector {
                let labels = pod.metadata.labels.as_ref();
                let matches = match labels {
                    Some(pod_labels) => {
                        selector.iter().all(|(k, v)| pod_labels.get(k) == Some(v))
                    }
                    None => false,
                };
                if !matches {
                    continue;
                }
            }

            pods.push(pod);
        }

        Ok(pods)
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM pods WHERE namespace = ? AND name = ?")
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
        pool: &AnyPool,
        namespace: &str,
        name: &str,
        status: &PodStatus,
    ) -> Result<()> {
        let phase = status.phase.to_string();
        let container_statuses = json_str(&status.container_statuses)?;
        let now = now_str();

        sqlx::query(
            r#"
            UPDATE pods
            SET status = ?, pod_ip = ?, host_ip = ?, container_statuses = ?, updated_at = ?
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(&phase)
        .bind(&status.pod_ip)
        .bind(&status.host_ip)
        .bind(&container_statuses)
        .bind(&now)
        .bind(namespace)
        .bind(name)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn bind_to_node(
        pool: &AnyPool,
        namespace: &str,
        name: &str,
        node_name: &str,
    ) -> Result<()> {
        let now = now_str();
        sqlx::query(
            r#"
            UPDATE pods
            SET node_name = ?, updated_at = ?
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(node_name)
        .bind(&now)
        .bind(namespace)
        .bind(name)
        .execute(pool)
        .await?;

        Ok(())
    }

    pub async fn list_unscheduled(pool: &AnyPool) -> Result<Vec<Pod>> {
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

        Ok(rows.into_iter().map(Self::row_to_pod).collect())
    }

    fn row_to_pod(row: sqlx::any::AnyRow) -> Pod {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: String = row.get("labels");
        let annotations: String = row.get("annotations");
        let spec: String = row.get("spec");
        let status: String = row.get("status");
        let node_name: Option<String> = row.get("node_name");
        let pod_ip: Option<String> = row.get("pod_ip");
        let host_ip: Option<String> = row.get("host_ip");
        let container_statuses: String = row.get("container_statuses");
        let owner_reference: String = row.get("owner_reference");
        let created_at: String = row.get("created_at");

        let mut pod = Pod {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Pod".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(parse_uid(&uid)),
                labels: json_from_opt(&labels),
                annotations: json_from_opt(&annotations),
                creation_timestamp: Some(parse_dt(&created_at)),
                owner_references: json_from_opt(&owner_reference),
                ..Default::default()
            },
            spec: json_from(&spec),
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
                container_statuses: json_from_opt(&container_statuses),
                ..Default::default()
            }),
        };

        if let Some(node) = node_name {
            pod.spec.node_name = Some(node);
        }

        pod
    }
}

/// Repository for Deployment operations
pub struct DeploymentRepository;

impl DeploymentRepository {
    pub async fn create(pool: &AnyPool, deployment: &Deployment) -> Result<Deployment> {
        let meta = &deployment.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(&deployment.spec)?;
        let replicas = deployment.spec.replicas();
        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO deployments (uid, name, namespace, labels, annotations, spec, replicas, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels,
                annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec,
                replicas = EXCLUDED.replicas,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name)
        .bind(&namespace)
        .bind(&labels)
        .bind(&annotations)
        .bind(&spec)
        .bind(replicas)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<Deployment> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec,
                   replicas, ready_replicas, available_replicas, created_at
            FROM deployments
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("deployments.apps \"{}\" not found", name)))?;

        Ok(Self::row_to_deployment(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<Deployment>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec,
                       replicas, ready_replicas, available_replicas, created_at
                FROM deployments
                WHERE namespace = ?
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

        Ok(rows.into_iter().map(Self::row_to_deployment).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM deployments WHERE namespace = ? AND name = ?")
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
        pool: &AnyPool,
        namespace: &str,
        name: &str,
        ready_replicas: i32,
        available_replicas: i32,
    ) -> Result<()> {
        let now = now_str();
        sqlx::query(
            r#"
            UPDATE deployments
            SET ready_replicas = ?, available_replicas = ?, updated_at = ?
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(ready_replicas)
        .bind(available_replicas)
        .bind(&now)
        .bind(namespace)
        .bind(name)
        .execute(pool)
        .await?;

        Ok(())
    }

    fn row_to_deployment(row: sqlx::any::AnyRow) -> Deployment {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: String = row.get("labels");
        let annotations: String = row.get("annotations");
        let spec: String = row.get("spec");
        let replicas: i32 = row.get("replicas");
        let ready_replicas: i32 = row.get("ready_replicas");
        let available_replicas: i32 = row.get("available_replicas");
        let created_at: String = row.get("created_at");

        Deployment {
            type_meta: TypeMeta {
                api_version: Some("apps/v1".to_string()),
                kind: Some("Deployment".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(parse_uid(&uid)),
                labels: json_from_opt(&labels),
                annotations: json_from_opt(&annotations),
                creation_timestamp: Some(parse_dt(&created_at)),
                ..Default::default()
            },
            spec: json_from(&spec),
            status: Some(DeploymentStatus {
                replicas,
                ready_replicas,
                available_replicas,
                // We don't track rolling-update revisions yet — assume every
                // pod is on the latest template.
                updated_replicas: ready_replicas,
                unavailable_replicas: (replicas - available_replicas).max(0),
                ..Default::default()
            }),
        }
    }
}

/// Repository for Service operations
pub struct ServiceRepository;

impl ServiceRepository {
    pub async fn create(pool: &AnyPool, service: &Service) -> Result<Service> {
        let meta = &service.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let spec = json_str(&service.spec)?;
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

        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO services (uid, name, namespace, labels, spec, service_type, cluster_ip, node_port, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels,
                spec = EXCLUDED.spec,
                service_type = EXCLUDED.service_type,
                cluster_ip = EXCLUDED.cluster_ip,
                node_port = EXCLUDED.node_port,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name)
        .bind(&namespace)
        .bind(&labels)
        .bind(&spec)
        .bind(service_type)
        .bind(&cluster_ip)
        .bind(node_port)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<Service> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, spec, service_type,
                   cluster_ip, node_port, created_at
            FROM services
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("services \"{}\" not found", name)))?;

        Ok(Self::row_to_service(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<Service>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, spec, service_type,
                       cluster_ip, node_port, created_at
                FROM services
                WHERE namespace = ?
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

        Ok(rows.into_iter().map(Self::row_to_service).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM services WHERE namespace = ? AND name = ?")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("services \"{}\" not found", name)));
        }

        Ok(())
    }

    fn row_to_service(row: sqlx::any::AnyRow) -> Service {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: String = row.get("labels");
        let spec_val: String = row.get("spec");
        let cluster_ip: Option<String> = row.get("cluster_ip");
        let node_port: Option<i32> = row.get("node_port");
        let created_at: String = row.get("created_at");

        let mut spec: ServiceSpec = json_from(&spec_val);
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
                uid: Some(parse_uid(&uid)),
                labels: json_from_opt(&labels),
                creation_timestamp: Some(parse_dt(&created_at)),
                ..Default::default()
            },
            spec,
            status: None,
        }
    }
}

/// Repository for Node operations
pub struct NodeRepository;

impl NodeRepository {
    pub async fn create(pool: &AnyPool, node: &Node) -> Result<Node> {
        let meta = &node.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let labels = json_str(&meta.labels)?;
        let status = node
            .status
            .as_ref()
            .map(|_| "Ready")
            .unwrap_or("NotReady");
        let addresses = node
            .status
            .as_ref()
            .map(|s| json_str(&s.addresses).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| "[]".to_string());
        let capacity = node
            .status
            .as_ref()
            .map(|s| json_str(&s.capacity).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());
        let allocatable = node
            .status
            .as_ref()
            .map(|s| json_str(&s.allocatable).unwrap_or_else(|_| "{}".to_string()))
            .unwrap_or_else(|| "{}".to_string());
        let node_info = node
            .status
            .as_ref()
            .and_then(|s| s.node_info.as_ref())
            .map(|info| json_str(info).unwrap_or_else(|_| "null".to_string()));
        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO nodes (uid, name, labels, status, addresses, capacity, allocatable, node_info, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name) DO UPDATE SET
                labels = EXCLUDED.labels,
                status = EXCLUDED.status,
                addresses = EXCLUDED.addresses,
                capacity = EXCLUDED.capacity,
                allocatable = EXCLUDED.allocatable,
                node_info = EXCLUDED.node_info,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name)
        .bind(&labels)
        .bind(status)
        .bind(&addresses)
        .bind(&capacity)
        .bind(&allocatable)
        .bind(&node_info)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &name).await
    }

    pub async fn get(pool: &AnyPool, name: &str) -> Result<Node> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, labels, status, addresses, capacity, allocatable, node_info, created_at, updated_at
            FROM nodes
            WHERE name = ?
            "#,
        )
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("nodes \"{}\" not found", name)))?;

        Ok(Self::row_to_node(row, true))
    }

    pub async fn list(pool: &AnyPool) -> Result<Vec<Node>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, labels, status, addresses, capacity, allocatable, node_info, created_at, updated_at
            FROM nodes
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Self::row_to_node(row, true))
            .collect())
    }

    pub async fn delete(pool: &AnyPool, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM nodes WHERE name = ?")
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("nodes \"{}\" not found", name)));
        }

        Ok(())
    }

    pub async fn list_ready(pool: &AnyPool) -> Result<Vec<Node>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, labels, status, addresses, capacity, allocatable, node_info, created_at, updated_at
            FROM nodes
            WHERE status = 'Ready'
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| Self::row_to_node(row, false))
            .collect())
    }

    fn row_to_node(row: sqlx::any::AnyRow, with_conditions: bool) -> Node {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let labels: String = row.get("labels");
        let status: String = row.get("status");
        let addresses: String = row.get("addresses");
        let capacity: String = row.get("capacity");
        let allocatable: String = row.get("allocatable");
        let node_info_raw: Option<String> = row.get("node_info");
        let created_at: String = row.get("created_at");
        let updated_at: String = row.get("updated_at");

        let created_at_dt = parse_dt(&created_at);
        let updated_at_dt = parse_dt(&updated_at);

        let conditions = if with_conditions {
            Some(vec![NodeCondition {
                condition_type: NodeConditionType::Ready,
                status: if status == "Ready" {
                    ConditionStatus::True
                } else {
                    ConditionStatus::False
                },
                last_heartbeat_time: Some(updated_at_dt),
                last_transition_time: Some(created_at_dt),
                reason: Some("KubeletReady".to_string()),
                message: Some("kubelet is posting ready status".to_string()),
            }])
        } else {
            None
        };

        let node_info = node_info_raw
            .as_deref()
            .and_then(|s| json_from_opt::<NodeSystemInfo>(s));

        Node {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Node".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                uid: Some(parse_uid(&uid)),
                labels: json_from_opt(&labels),
                creation_timestamp: Some(created_at_dt),
                ..Default::default()
            },
            spec: None,
            status: Some(NodeStatus {
                capacity: json_from_opt(&capacity),
                allocatable: json_from_opt(&allocatable),
                conditions,
                addresses: json_from_opt(&addresses),
                node_info,
                phase: NodePhase::Running,
                ..Default::default()
            }),
        }
    }
}

/// Repository for Endpoints operations
pub struct EndpointsRepository;

impl EndpointsRepository {
    pub async fn create_or_update(pool: &AnyPool, endpoints: &Endpoints) -> Result<Endpoints> {
        let meta = &endpoints.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let subsets = json_str(&endpoints.subsets)?;
        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO endpoints (uid, name, namespace, subsets, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                subsets = EXCLUDED.subsets,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name)
        .bind(&namespace)
        .bind(&subsets)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<Endpoints> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, subsets, created_at
            FROM endpoints
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("endpoints \"{}\" not found", name)))?;

        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let subsets: String = row.get("subsets");
        let created_at: String = row.get("created_at");

        Ok(Endpoints {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Endpoints".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(parse_uid(&uid)),
                creation_timestamp: Some(parse_dt(&created_at)),
                ..Default::default()
            },
            subsets: json_from(&subsets),
        })
    }

}

/// Repository for Namespace operations
pub struct NamespaceRepository;

impl NamespaceRepository {
    pub async fn create(pool: &AnyPool, namespace: &Namespace) -> Result<Namespace> {
        let meta = &namespace.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(&namespace.spec)?;
        let status = namespace
            .status
            .as_ref()
            .map(|s| s.phase.to_string())
            .unwrap_or_else(|| "Active".to_string());
        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO namespaces (uid, name, labels, annotations, spec, status, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name) DO UPDATE SET
                labels = EXCLUDED.labels,
                annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec,
                status = EXCLUDED.status,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name)
        .bind(&labels)
        .bind(&annotations)
        .bind(&spec)
        .bind(&status)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &name).await
    }

    pub async fn get(pool: &AnyPool, name: &str) -> Result<Namespace> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, labels, annotations, spec, status, created_at
            FROM namespaces
            WHERE name = ?
            "#,
        )
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("namespaces \"{}\" not found", name)))?;

        Ok(Self::row_to_namespace(row))
    }

    pub async fn list(pool: &AnyPool) -> Result<Vec<Namespace>> {
        let rows = sqlx::query(
            r#"
            SELECT uid, name, labels, annotations, spec, status, created_at
            FROM namespaces
            ORDER BY created_at ASC
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows.into_iter().map(Self::row_to_namespace).collect())
    }

    pub async fn delete(pool: &AnyPool, name: &str) -> Result<()> {
        // Prevent deletion of system namespaces
        if matches!(name, "default" | "kube-system" | "kube-public" | "kube-node-lease") {
            return Err(Error::BadRequest(format!(
                "namespace \"{}\" is a system namespace and cannot be deleted",
                name
            )));
        }

        // Check if namespace has resources
        let pod_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pods WHERE namespace = ?")
            .bind(name)
            .fetch_one(pool)
            .await?;

        if pod_count.0 > 0 {
            return Err(Error::BadRequest(format!(
                "namespace \"{}\" still has {} pods",
                name, pod_count.0
            )));
        }

        let result = sqlx::query("DELETE FROM namespaces WHERE name = ?")
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("namespaces \"{}\" not found", name)));
        }

        Ok(())
    }

    fn row_to_namespace(row: sqlx::any::AnyRow) -> Namespace {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let labels: String = row.get("labels");
        let annotations: String = row.get("annotations");
        let spec: String = row.get("spec");
        let status: String = row.get("status");
        let created_at: String = row.get("created_at");

        Namespace {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Namespace".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                uid: Some(parse_uid(&uid)),
                labels: json_from_opt(&labels),
                annotations: json_from_opt(&annotations),
                creation_timestamp: Some(parse_dt(&created_at)),
                ..Default::default()
            },
            spec: json_from_opt(&spec),
            status: Some(NamespaceStatus {
                phase: match status.as_str() {
                    "Terminating" => NamespacePhase::Terminating,
                    _ => NamespacePhase::Active,
                },
                conditions: None,
            }),
        }
    }
}

/// Repository for Event operations
pub struct EventRepository;

impl EventRepository {
    pub async fn create(pool: &AnyPool, event: &Event) -> Result<Event> {
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

        let first_ts = event.first_timestamp.map(|t| t.to_rfc3339());
        let last_ts = event.last_timestamp.map(|t| t.to_rfc3339());
        let event_ts = event.event_time.map(|t| t.to_rfc3339());
        let now = now_str();

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
                reporting_controller, reporting_instance,
                created_at, updated_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                last_timestamp = EXCLUDED.last_timestamp,
                count = events.count + 1,
                message = EXCLUDED.message,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
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
        .bind(source.and_then(|s| s.component.clone()))
        .bind(source.and_then(|s| s.host.clone()))
        .bind(&first_ts)
        .bind(&last_ts)
        .bind(&event_ts)
        .bind(event.count.unwrap_or(1))
        .bind(event_type)
        .bind(&event.action)
        .bind(&event.reporting_controller)
        .bind(&event.reporting_instance)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<Event> {
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
            WHERE namespace = ? AND name = ?
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
        pool: &AnyPool,
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
                    WHERE namespace = ? AND involved_object_name = ? AND involved_object_kind = ?
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
                    WHERE namespace = ?
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

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM events WHERE namespace = ? AND name = ?")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("events \"{}\" not found", name)));
        }

        Ok(())
    }

    fn row_to_event(row: sqlx::any::AnyRow) -> Result<Event> {
        let uid: String = row.get("uid");
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

        let first_timestamp: Option<String> = row.get("first_timestamp");
        let last_timestamp: Option<String> = row.get("last_timestamp");
        let event_time: Option<String> = row.get("event_time");

        let count: Option<i32> = row.get("count");
        let event_type_str: Option<String> = row.get("event_type");
        let action: Option<String> = row.get("action");

        let reporting_controller: Option<String> = row.get("reporting_controller");
        let reporting_instance: Option<String> = row.get("reporting_instance");

        let created_at: String = row.get("created_at");

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
                uid: Some(parse_uid(&uid)),
                creation_timestamp: Some(parse_dt(&created_at)),
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
            first_timestamp: parse_dt_opt(first_timestamp),
            last_timestamp: parse_dt_opt(last_timestamp),
            count,
            event_type,
            event_time: parse_dt_opt(event_time),
            action,
            reporting_controller,
            reporting_instance,
        })
    }
}

/// Repository for StatefulSet operations
pub struct StatefulSetRepository;

impl StatefulSetRepository {
    pub async fn create(pool: &AnyPool, ss: &StatefulSet) -> Result<StatefulSet> {
        let meta = &ss.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(&ss.spec)?;
        let replicas = ss.spec.replicas();
        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO statefulsets (uid, name, namespace, labels, annotations, spec, replicas, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels,
                annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec,
                replicas = EXCLUDED.replicas,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name)
        .bind(&namespace)
        .bind(&labels)
        .bind(&annotations)
        .bind(&spec)
        .bind(replicas)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<StatefulSet> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec,
                   replicas, ready_replicas, current_replicas, created_at
            FROM statefulsets WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("statefulsets.apps \"{}\" not found", name)))?;

        Ok(Self::row_to_statefulset(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<StatefulSet>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec,
                       replicas, ready_replicas, current_replicas, created_at
                FROM statefulsets WHERE namespace = ?
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
                       replicas, ready_replicas, current_replicas, created_at
                FROM statefulsets
                ORDER BY created_at DESC
                "#,
            )
            .fetch_all(pool)
            .await?
        };
        Ok(rows.into_iter().map(Self::row_to_statefulset).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM statefulsets WHERE namespace = ? AND name = ?")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("statefulsets.apps \"{}\" not found", name)));
        }
        Ok(())
    }

    pub async fn update_status(
        pool: &AnyPool,
        namespace: &str,
        name: &str,
        ready_replicas: i32,
        current_replicas: i32,
    ) -> Result<()> {
        let now = now_str();
        sqlx::query(
            r#"
            UPDATE statefulsets
            SET ready_replicas = ?, current_replicas = ?, updated_at = ?
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(ready_replicas)
        .bind(current_replicas)
        .bind(&now)
        .bind(namespace)
        .bind(name)
        .execute(pool)
        .await?;
        Ok(())
    }

    fn row_to_statefulset(row: sqlx::any::AnyRow) -> StatefulSet {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: String = row.get("labels");
        let annotations: String = row.get("annotations");
        let spec: String = row.get("spec");
        let replicas: i32 = row.get("replicas");
        let ready_replicas: i32 = row.get("ready_replicas");
        let current_replicas: i32 = row.get("current_replicas");
        let created_at: String = row.get("created_at");

        StatefulSet {
            type_meta: TypeMeta {
                api_version: Some("apps/v1".to_string()),
                kind: Some("StatefulSet".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(parse_uid(&uid)),
                labels: json_from_opt(&labels),
                annotations: json_from_opt(&annotations),
                creation_timestamp: Some(parse_dt(&created_at)),
                ..Default::default()
            },
            spec: json_from(&spec),
            status: Some(StatefulSetStatus {
                replicas,
                ready_replicas,
                current_replicas,
                ..Default::default()
            }),
        }
    }
}

// available_replicas / updated_replicas mirroring is intentional — we don't
// track separate revisions yet, so "current" == "updated" == ready.

/// Repository for DaemonSet operations
pub struct DaemonSetRepository;

impl DaemonSetRepository {
    pub async fn create(pool: &AnyPool, ds: &DaemonSet) -> Result<DaemonSet> {
        let meta = &ds.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(&ds.spec)?;
        let now = now_str();

        sqlx::query(
            r#"
            INSERT INTO daemonsets (uid, name, namespace, labels, annotations, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels,
                annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name)
        .bind(&namespace)
        .bind(&labels)
        .bind(&annotations)
        .bind(&spec)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;

        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<DaemonSet> {
        let row = sqlx::query(
            r#"
            SELECT uid, name, namespace, labels, annotations, spec,
                   desired_number_scheduled, current_number_scheduled, number_ready, created_at
            FROM daemonsets WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(namespace)
        .bind(name)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound(format!("daemonsets.apps \"{}\" not found", name)))?;

        Ok(Self::row_to_daemonset(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<DaemonSet>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query(
                r#"
                SELECT uid, name, namespace, labels, annotations, spec,
                       desired_number_scheduled, current_number_scheduled, number_ready, created_at
                FROM daemonsets WHERE namespace = ?
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
                       desired_number_scheduled, current_number_scheduled, number_ready, created_at
                FROM daemonsets
                ORDER BY created_at DESC
                "#,
            )
            .fetch_all(pool)
            .await?
        };
        Ok(rows.into_iter().map(Self::row_to_daemonset).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM daemonsets WHERE namespace = ? AND name = ?")
            .bind(namespace)
            .bind(name)
            .execute(pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(Error::NotFound(format!("daemonsets.apps \"{}\" not found", name)));
        }
        Ok(())
    }

    pub async fn update_status(
        pool: &AnyPool,
        namespace: &str,
        name: &str,
        desired: i32,
        current: i32,
        ready: i32,
    ) -> Result<()> {
        let now = now_str();
        sqlx::query(
            r#"
            UPDATE daemonsets
            SET desired_number_scheduled = ?, current_number_scheduled = ?,
                number_ready = ?, updated_at = ?
            WHERE namespace = ? AND name = ?
            "#,
        )
        .bind(desired)
        .bind(current)
        .bind(ready)
        .bind(&now)
        .bind(namespace)
        .bind(name)
        .execute(pool)
        .await?;
        Ok(())
    }

    fn row_to_daemonset(row: sqlx::any::AnyRow) -> DaemonSet {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let labels: String = row.get("labels");
        let annotations: String = row.get("annotations");
        let spec: String = row.get("spec");
        let desired: i32 = row.get("desired_number_scheduled");
        let current: i32 = row.get("current_number_scheduled");
        let ready: i32 = row.get("number_ready");
        let created_at: String = row.get("created_at");

        DaemonSet {
            type_meta: TypeMeta {
                api_version: Some("apps/v1".to_string()),
                kind: Some("DaemonSet".to_string()),
            },
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(ns),
                uid: Some(parse_uid(&uid)),
                labels: json_from_opt(&labels),
                annotations: json_from_opt(&annotations),
                creation_timestamp: Some(parse_dt(&created_at)),
                ..Default::default()
            },
            spec: json_from(&spec),
            status: Some(DaemonSetStatus {
                desired_number_scheduled: desired,
                current_number_scheduled: current,
                number_ready: ready,
                // No revisioning yet — every running pod is "up to date".
                updated_number_scheduled: current,
                // No minReadySeconds support yet — equate ready with available.
                number_available: ready,
                ..Default::default()
            }),
        }
    }
}


// ============================================================================
// ServiceAccount
// ============================================================================

pub struct ServiceAccountRepository;

impl ServiceAccountRepository {
    pub async fn create(pool: &AnyPool, sa: &ServiceAccount) -> Result<ServiceAccount> {
        let meta = &sa.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(sa)?;
        let now = now_str();
        sqlx::query(
            r#"
            INSERT INTO serviceaccounts (uid, name, namespace, labels, annotations, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels, annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec, updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string())
        .bind(&name).bind(&namespace).bind(&labels).bind(&annotations).bind(&spec)
        .bind(&now).bind(&now)
        .execute(pool).await?;
        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<ServiceAccount> {
        let row = sqlx::query("SELECT uid, name, namespace, spec, created_at FROM serviceaccounts WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name)
            .fetch_optional(pool).await?
            .ok_or_else(|| Error::NotFound(format!("serviceaccounts \"{}\" not found", name)))?;
        Ok(Self::row_to(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<ServiceAccount>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM serviceaccounts WHERE namespace = ? ORDER BY created_at DESC").bind(ns).fetch_all(pool).await?
        } else {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM serviceaccounts ORDER BY created_at DESC").fetch_all(pool).await?
        };
        Ok(rows.into_iter().map(Self::row_to).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM serviceaccounts WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).execute(pool).await?;
        if r.rows_affected() == 0 { return Err(Error::NotFound(format!("serviceaccounts \"{}\" not found", name))); }
        Ok(())
    }

    fn row_to(row: sqlx::any::AnyRow) -> ServiceAccount {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let spec: String = row.get("spec");
        let created_at: String = row.get("created_at");
        let mut sa: ServiceAccount = json_from(&spec);
        sa.metadata.name = Some(name);
        sa.metadata.namespace = Some(ns);
        sa.metadata.uid = Some(parse_uid(&uid));
        sa.metadata.creation_timestamp = Some(parse_dt(&created_at));
        sa
    }
}

// ============================================================================
// Secret
// ============================================================================

pub struct SecretRepository;

impl SecretRepository {
    pub async fn create(pool: &AnyPool, s: &Secret) -> Result<Secret> {
        let meta = &s.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(s)?;
        let now = now_str();
        sqlx::query(
            r#"
            INSERT INTO secrets (uid, name, namespace, labels, annotations, secret_type, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels, annotations = EXCLUDED.annotations,
                secret_type = EXCLUDED.secret_type, spec = EXCLUDED.spec,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string()).bind(&name).bind(&namespace)
        .bind(&labels).bind(&annotations).bind(&s.secret_type).bind(&spec)
        .bind(&now).bind(&now)
        .execute(pool).await?;
        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<Secret> {
        let row = sqlx::query("SELECT uid, name, namespace, spec, created_at FROM secrets WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).fetch_optional(pool).await?
            .ok_or_else(|| Error::NotFound(format!("secrets \"{}\" not found", name)))?;
        Ok(Self::row_to(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<Secret>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM secrets WHERE namespace = ? ORDER BY created_at DESC").bind(ns).fetch_all(pool).await?
        } else {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM secrets ORDER BY created_at DESC").fetch_all(pool).await?
        };
        Ok(rows.into_iter().map(Self::row_to).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM secrets WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).execute(pool).await?;
        if r.rows_affected() == 0 { return Err(Error::NotFound(format!("secrets \"{}\" not found", name))); }
        Ok(())
    }

    fn row_to(row: sqlx::any::AnyRow) -> Secret {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let spec: String = row.get("spec");
        let created_at: String = row.get("created_at");
        let mut s: Secret = json_from(&spec);
        s.metadata.name = Some(name);
        s.metadata.namespace = Some(ns);
        s.metadata.uid = Some(parse_uid(&uid));
        s.metadata.creation_timestamp = Some(parse_dt(&created_at));
        s
    }
}

// ============================================================================
// ConfigMap
// ============================================================================

pub struct ConfigMapRepository;

impl ConfigMapRepository {
    pub async fn create(pool: &AnyPool, cm: &ConfigMap) -> Result<ConfigMap> {
        let meta = &cm.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(cm)?;
        let now = now_str();
        sqlx::query(
            r#"
            INSERT INTO configmaps (uid, name, namespace, labels, annotations, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels, annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec, updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string()).bind(&name).bind(&namespace)
        .bind(&labels).bind(&annotations).bind(&spec).bind(&now).bind(&now)
        .execute(pool).await?;
        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<ConfigMap> {
        let row = sqlx::query("SELECT uid, name, namespace, spec, created_at FROM configmaps WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).fetch_optional(pool).await?
            .ok_or_else(|| Error::NotFound(format!("configmaps \"{}\" not found", name)))?;
        Ok(Self::row_to(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<ConfigMap>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM configmaps WHERE namespace = ? ORDER BY created_at DESC").bind(ns).fetch_all(pool).await?
        } else {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM configmaps ORDER BY created_at DESC").fetch_all(pool).await?
        };
        Ok(rows.into_iter().map(Self::row_to).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM configmaps WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).execute(pool).await?;
        if r.rows_affected() == 0 { return Err(Error::NotFound(format!("configmaps \"{}\" not found", name))); }
        Ok(())
    }

    fn row_to(row: sqlx::any::AnyRow) -> ConfigMap {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let spec: String = row.get("spec");
        let created_at: String = row.get("created_at");
        let mut cm: ConfigMap = json_from(&spec);
        cm.metadata.name = Some(name);
        cm.metadata.namespace = Some(ns);
        cm.metadata.uid = Some(parse_uid(&uid));
        cm.metadata.creation_timestamp = Some(parse_dt(&created_at));
        cm
    }
}

// ============================================================================
// ClusterRole (cluster-scoped)
// ============================================================================

pub struct ClusterRoleRepository;

impl ClusterRoleRepository {
    pub async fn create(pool: &AnyPool, cr: &ClusterRole) -> Result<ClusterRole> {
        let uid = cr.metadata.uid.unwrap_or_else(Uuid::new_v4);
        let name = cr.metadata.name().to_string();
        let labels = json_str(&cr.metadata.labels)?;
        let annotations = json_str(&cr.metadata.annotations)?;
        let spec = json_str(cr)?;
        let now = now_str();
        sqlx::query(
            r#"
            INSERT INTO clusterroles (uid, name, labels, annotations, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name) DO UPDATE SET
                labels = EXCLUDED.labels, annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec, updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string()).bind(&name).bind(&labels).bind(&annotations)
        .bind(&spec).bind(&now).bind(&now).execute(pool).await?;
        Self::get(pool, &name).await
    }

    pub async fn get(pool: &AnyPool, name: &str) -> Result<ClusterRole> {
        let row = sqlx::query("SELECT uid, name, spec, created_at FROM clusterroles WHERE name = ?")
            .bind(name).fetch_optional(pool).await?
            .ok_or_else(|| Error::NotFound(format!("clusterroles.rbac.authorization.k8s.io \"{}\" not found", name)))?;
        Ok(Self::row_to(row))
    }

    pub async fn list(pool: &AnyPool) -> Result<Vec<ClusterRole>> {
        let rows = sqlx::query("SELECT uid, name, spec, created_at FROM clusterroles ORDER BY created_at DESC")
            .fetch_all(pool).await?;
        Ok(rows.into_iter().map(Self::row_to).collect())
    }

    pub async fn delete(pool: &AnyPool, name: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM clusterroles WHERE name = ?").bind(name).execute(pool).await?;
        if r.rows_affected() == 0 { return Err(Error::NotFound(format!("clusterroles \"{}\" not found", name))); }
        Ok(())
    }

    fn row_to(row: sqlx::any::AnyRow) -> ClusterRole {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let spec: String = row.get("spec");
        let created_at: String = row.get("created_at");
        let mut cr: ClusterRole = json_from(&spec);
        cr.metadata.name = Some(name);
        cr.metadata.uid = Some(parse_uid(&uid));
        cr.metadata.creation_timestamp = Some(parse_dt(&created_at));
        cr
    }
}

// ============================================================================
// ClusterRoleBinding (cluster-scoped)
// ============================================================================

pub struct ClusterRoleBindingRepository;

impl ClusterRoleBindingRepository {
    pub async fn create(pool: &AnyPool, b: &ClusterRoleBinding) -> Result<ClusterRoleBinding> {
        let uid = b.metadata.uid.unwrap_or_else(Uuid::new_v4);
        let name = b.metadata.name().to_string();
        let labels = json_str(&b.metadata.labels)?;
        let annotations = json_str(&b.metadata.annotations)?;
        let spec = json_str(b)?;
        let now = now_str();
        sqlx::query(
            r#"
            INSERT INTO clusterrolebindings (uid, name, labels, annotations, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name) DO UPDATE SET
                labels = EXCLUDED.labels, annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec, updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string()).bind(&name).bind(&labels).bind(&annotations)
        .bind(&spec).bind(&now).bind(&now).execute(pool).await?;
        Self::get(pool, &name).await
    }

    pub async fn get(pool: &AnyPool, name: &str) -> Result<ClusterRoleBinding> {
        let row = sqlx::query("SELECT uid, name, spec, created_at FROM clusterrolebindings WHERE name = ?")
            .bind(name).fetch_optional(pool).await?
            .ok_or_else(|| Error::NotFound(format!("clusterrolebindings.rbac.authorization.k8s.io \"{}\" not found", name)))?;
        Ok(Self::row_to(row))
    }

    pub async fn list(pool: &AnyPool) -> Result<Vec<ClusterRoleBinding>> {
        let rows = sqlx::query("SELECT uid, name, spec, created_at FROM clusterrolebindings ORDER BY created_at DESC")
            .fetch_all(pool).await?;
        Ok(rows.into_iter().map(Self::row_to).collect())
    }

    pub async fn delete(pool: &AnyPool, name: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM clusterrolebindings WHERE name = ?").bind(name).execute(pool).await?;
        if r.rows_affected() == 0 { return Err(Error::NotFound(format!("clusterrolebindings \"{}\" not found", name))); }
        Ok(())
    }

    fn row_to(row: sqlx::any::AnyRow) -> ClusterRoleBinding {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let spec: String = row.get("spec");
        let created_at: String = row.get("created_at");
        let mut b: ClusterRoleBinding = json_from(&spec);
        b.metadata.name = Some(name);
        b.metadata.uid = Some(parse_uid(&uid));
        b.metadata.creation_timestamp = Some(parse_dt(&created_at));
        b
    }
}


// ============================================================================
// Role (namespaced)
// ============================================================================

pub struct RoleRepository;

impl RoleRepository {
    pub async fn create(pool: &AnyPool, r: &Role) -> Result<Role> {
        let meta = &r.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(r)?;
        let now = now_str();
        sqlx::query(
            r#"
            INSERT INTO roles (uid, name, namespace, labels, annotations, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels, annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec, updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string()).bind(&name).bind(&namespace)
        .bind(&labels).bind(&annotations).bind(&spec)
        .bind(&now).bind(&now).execute(pool).await?;
        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<Role> {
        let row = sqlx::query("SELECT uid, name, namespace, spec, created_at FROM roles WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).fetch_optional(pool).await?
            .ok_or_else(|| Error::NotFound(format!("roles.rbac.authorization.k8s.io \"{}\" not found", name)))?;
        Ok(Self::row_to(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<Role>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM roles WHERE namespace = ? ORDER BY created_at DESC").bind(ns).fetch_all(pool).await?
        } else {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM roles ORDER BY created_at DESC").fetch_all(pool).await?
        };
        Ok(rows.into_iter().map(Self::row_to).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM roles WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).execute(pool).await?;
        if r.rows_affected() == 0 { return Err(Error::NotFound(format!("roles \"{}\" not found", name))); }
        Ok(())
    }

    fn row_to(row: sqlx::any::AnyRow) -> Role {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let spec: String = row.get("spec");
        let created_at: String = row.get("created_at");
        let mut r: Role = json_from(&spec);
        r.metadata.name = Some(name);
        r.metadata.namespace = Some(ns);
        r.metadata.uid = Some(parse_uid(&uid));
        r.metadata.creation_timestamp = Some(parse_dt(&created_at));
        r
    }
}

// ============================================================================
// RoleBinding (namespaced)
// ============================================================================

pub struct RoleBindingRepository;

impl RoleBindingRepository {
    pub async fn create(pool: &AnyPool, b: &RoleBinding) -> Result<RoleBinding> {
        let meta = &b.metadata;
        let uid = meta.uid.unwrap_or_else(Uuid::new_v4);
        let name = meta.name().to_string();
        let namespace = meta.namespace().to_string();
        let labels = json_str(&meta.labels)?;
        let annotations = json_str(&meta.annotations)?;
        let spec = json_str(b)?;
        let now = now_str();
        sqlx::query(
            r#"
            INSERT INTO rolebindings (uid, name, namespace, labels, annotations, spec, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (name, namespace) DO UPDATE SET
                labels = EXCLUDED.labels, annotations = EXCLUDED.annotations,
                spec = EXCLUDED.spec, updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(uid.to_string()).bind(&name).bind(&namespace)
        .bind(&labels).bind(&annotations).bind(&spec)
        .bind(&now).bind(&now).execute(pool).await?;
        Self::get(pool, &namespace, &name).await
    }

    pub async fn get(pool: &AnyPool, namespace: &str, name: &str) -> Result<RoleBinding> {
        let row = sqlx::query("SELECT uid, name, namespace, spec, created_at FROM rolebindings WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).fetch_optional(pool).await?
            .ok_or_else(|| Error::NotFound(format!("rolebindings.rbac.authorization.k8s.io \"{}\" not found", name)))?;
        Ok(Self::row_to(row))
    }

    pub async fn list(pool: &AnyPool, namespace: Option<&str>) -> Result<Vec<RoleBinding>> {
        let rows = if let Some(ns) = namespace {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM rolebindings WHERE namespace = ? ORDER BY created_at DESC").bind(ns).fetch_all(pool).await?
        } else {
            sqlx::query("SELECT uid, name, namespace, spec, created_at FROM rolebindings ORDER BY created_at DESC").fetch_all(pool).await?
        };
        Ok(rows.into_iter().map(Self::row_to).collect())
    }

    pub async fn delete(pool: &AnyPool, namespace: &str, name: &str) -> Result<()> {
        let r = sqlx::query("DELETE FROM rolebindings WHERE namespace = ? AND name = ?")
            .bind(namespace).bind(name).execute(pool).await?;
        if r.rows_affected() == 0 { return Err(Error::NotFound(format!("rolebindings \"{}\" not found", name))); }
        Ok(())
    }

    fn row_to(row: sqlx::any::AnyRow) -> RoleBinding {
        let uid: String = row.get("uid");
        let name: String = row.get("name");
        let ns: String = row.get("namespace");
        let spec: String = row.get("spec");
        let created_at: String = row.get("created_at");
        let mut b: RoleBinding = json_from(&spec);
        b.metadata.name = Some(name);
        b.metadata.namespace = Some(ns);
        b.metadata.uid = Some(parse_uid(&uid));
        b.metadata.creation_timestamp = Some(parse_dt(&created_at));
        b
    }
}
