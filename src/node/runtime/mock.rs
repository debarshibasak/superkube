use chrono::{DateTime, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use crate::models::Container;

use super::{ContainerInfo, LogOptions, Runtime};

/// In-memory runtime stub: lets the rest of the system run end-to-end without
/// a real container engine. Useful on platforms where neither Docker nor
/// libcontainer is available, and as a baseline for tests.
pub struct ContainerdRuntime {
    socket_path: String,
    containers: HashMap<String, ContainerState>,
    /// Reverse index: container_name → container_id. Lets `find_container`
    /// behave like Docker (lookup by name).
    by_name: HashMap<String, String>,
}

struct ContainerState {
    id: String,
    name: String,
    image: String,
    running: bool,
    started_at: DateTime<Utc>,
    logs: Vec<LogEntry>,
}

struct LogEntry {
    timestamp: DateTime<Utc>,
    message: String,
}

impl ContainerdRuntime {
    pub async fn new(socket_path: &str) -> anyhow::Result<Self> {
        tracing::info!("Initializing mock runtime at {}", socket_path);
        if !std::path::Path::new(socket_path).exists() {
            tracing::warn!(
                "containerd socket not found at {}, using mock runtime",
                socket_path
            );
        }
        Ok(Self {
            socket_path: socket_path.to_string(),
            containers: HashMap::new(),
            by_name: HashMap::new(),
        })
    }
}

#[async_trait::async_trait]
impl Runtime for ContainerdRuntime {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn create_and_start_container(
        &mut self,
        name: &str,
        container: &Container,
    ) -> anyhow::Result<String> {
        let image = &container.image;
        tracing::info!("Creating mock container {} with image {}", name, image);

        let container_id = format!("containerd://{}", Uuid::new_v4());
        let now = Utc::now();
        let logs = vec![
            LogEntry {
                timestamp: now,
                message: format!("Starting container {} with image {}", name, image),
            },
            LogEntry {
                timestamp: now + chrono::Duration::milliseconds(100),
                message: "Initializing application...".to_string(),
            },
            LogEntry {
                timestamp: now + chrono::Duration::milliseconds(200),
                message: "Application started successfully".to_string(),
            },
            LogEntry {
                timestamp: now + chrono::Duration::milliseconds(300),
                message: "Listening for connections...".to_string(),
            },
        ];

        self.containers.insert(
            container_id.clone(),
            ContainerState {
                id: container_id.clone(),
                name: name.to_string(),
                image: image.clone(),
                running: true,
                started_at: now,
                logs,
            },
        );
        self.by_name.insert(name.to_string(), container_id.clone());

        tracing::info!("Mock container {} started with ID {}", name, container_id);
        Ok(container_id)
    }

    async fn is_container_running(&self, container_id: &str) -> anyhow::Result<bool> {
        Ok(self
            .containers
            .get(container_id)
            .map(|c| c.running)
            .unwrap_or(false))
    }

    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>> {
        let id = match self.by_name.get(name) {
            Some(id) => id,
            None => return Ok(None),
        };
        let c = match self.containers.get(id) {
            Some(c) => c,
            None => return Ok(None),
        };
        Ok(Some(ContainerInfo {
            id: c.id.clone(),
            running: c.running,
            restart_count: 0,
            started_at: Some(c.started_at),
            exit_code: None,
            ip: None,
            port_mappings: Vec::new(),
        }))
    }

    async fn get_logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> anyhow::Result<String> {
        let container = self
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("Container {} not found", container_id))?;

        let mut logs: Vec<&LogEntry> = container.logs.iter().collect();

        if let Some(since) = options.since_time {
            logs.retain(|entry| entry.timestamp >= since);
        }

        if let Some(tail) = options.tail_lines {
            let tail = tail as usize;
            let len = logs.len();
            if len > tail {
                logs = logs.into_iter().skip(len - tail).collect();
            }
        }

        let mut output = String::new();
        for entry in logs {
            if options.timestamps {
                output.push_str(&format!(
                    "{} {}\n",
                    entry.timestamp.format("%Y-%m-%dT%H:%M:%S%.9fZ"),
                    entry.message
                ));
            } else {
                output.push_str(&entry.message);
                output.push('\n');
            }
        }

        if let Some(limit) = options.limit_bytes {
            let limit = limit as usize;
            if output.len() > limit {
                output.truncate(limit);
            }
        }

        Ok(output)
    }
}
