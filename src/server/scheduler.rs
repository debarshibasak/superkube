use sqlx::PgPool;
use tokio::time::{interval, Duration};

use crate::db::{NodeRepository, PodRepository};
use crate::models::*;

/// Scheduler assigns pending pods to nodes
pub struct Scheduler {
    pool: PgPool,
}

impl Scheduler {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run the scheduler loop
    pub async fn run(&self) {
        let mut interval = interval(Duration::from_secs(2));

        loop {
            interval.tick().await;

            if let Err(e) = self.schedule_pending_pods().await {
                tracing::error!("Scheduler error: {}", e);
            }
        }
    }

    /// Find and schedule all pending pods
    async fn schedule_pending_pods(&self) -> anyhow::Result<()> {
        // Get all unscheduled pods
        let pods = PodRepository::list_unscheduled(&self.pool).await?;

        if pods.is_empty() {
            return Ok(());
        }

        // Get all ready nodes
        let nodes = NodeRepository::list_ready(&self.pool).await?;

        if nodes.is_empty() {
            tracing::debug!("No ready nodes available for scheduling");
            return Ok(());
        }

        for pod in pods {
            if let Err(e) = self.schedule_pod(&pod, &nodes).await {
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

    /// Schedule a single pod to a node
    async fn schedule_pod(&self, pod: &Pod, nodes: &[Node]) -> anyhow::Result<()> {
        let namespace = pod.metadata.namespace();
        let pod_name = pod.metadata.name();

        // Find a suitable node
        let node = self.find_node_for_pod(pod, nodes)?;

        let node_name = node.metadata.name();

        tracing::info!(
            "Scheduling pod {}/{} to node {}",
            namespace,
            pod_name,
            node_name
        );

        // Bind pod to node
        PodRepository::bind_to_node(&self.pool, namespace, pod_name, node_name).await?;

        Ok(())
    }

    /// Find a suitable node for the pod
    fn find_node_for_pod<'a>(&self, pod: &Pod, nodes: &'a [Node]) -> anyhow::Result<&'a Node> {
        // Check node selector if specified
        let node_selector = pod.spec.node_selector.as_ref();

        for node in nodes {
            // Check if node matches node selector
            if let Some(selector) = node_selector {
                let node_labels = node.metadata.labels();
                let matches = selector.iter().all(|(k, v)| node_labels.get(k) == Some(v));
                if !matches {
                    continue;
                }
            }

            // Check node conditions
            if let Some(status) = &node.status {
                if let Some(conditions) = &status.conditions {
                    let is_ready = conditions.iter().any(|c| {
                        c.condition_type == NodeConditionType::Ready
                            && c.status == ConditionStatus::True
                    });
                    if !is_ready {
                        continue;
                    }
                }
            }

            // Node is suitable
            return Ok(node);
        }

        anyhow::bail!("No suitable node found for pod")
    }
}
