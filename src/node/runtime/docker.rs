//! Docker runtime — talks to the local Docker daemon over /var/run/docker.sock.
//!
//! On macOS this is what `Docker Desktop` exposes; that daemon runs containers
//! inside a hidden Linux VM, which is the only way to get real Linux containers
//! on a Mac. On Linux you'd usually prefer the embedded libcontainer path, so
//! this module is `cfg(target_os = "macos")` for now.
//!
//! Container ID format: `docker://<id>`. We strip the prefix when calling
//! bollard so callers never see the bare Docker id.

use std::collections::HashMap;

use bollard::container::{
    Config, CreateContainerOptions, LogOutput, LogsOptions, StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::service::{HostConfig, PortBinding};
use bollard::Docker;
use bytes::Bytes;
use chrono::DateTime;
use futures::StreamExt;

use crate::models::{Container, Protocol};

use super::{ContainerInfo, ExecSession, LogOptions, LogStream, PortMapping, Runtime};

const ID_PREFIX: &str = "docker://";

pub struct DockerRuntime {
    client: Docker,
}

impl DockerRuntime {
    pub async fn new() -> anyhow::Result<Self> {
        let client = Docker::connect_with_socket_defaults()
            .map_err(|e| anyhow::anyhow!("connecting to docker: {e}"))?;

        // Fail fast if the daemon isn't reachable so the agent can fall back.
        client
            .ping()
            .await
            .map_err(|e| anyhow::anyhow!("docker ping: {e}"))?;

        Ok(Self { client })
    }

    fn strip(id: &str) -> &str {
        id.strip_prefix(ID_PREFIX).unwrap_or(id)
    }
}

/// Translate Docker's `network_settings.ports` (`HashMap<"80/tcp", Vec<PortBinding>>`)
/// into our flat list of `PortMapping`. Bindings without a HostPort get filtered.
fn parse_docker_ports(
    ports: &HashMap<String, Option<Vec<bollard::service::PortBinding>>>,
) -> Vec<PortMapping> {
    let mut out = Vec::new();
    for (key, bindings) in ports {
        let mut parts = key.splitn(2, '/');
        let container_port: i32 = match parts.next().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        let protocol = parts.next().unwrap_or("tcp").to_string();
        if let Some(bindings) = bindings {
            for b in bindings {
                if let Some(hp) = b.host_port.as_ref().and_then(|s| s.parse::<u16>().ok()) {
                    out.push(PortMapping {
                        container_port,
                        host_port: hp,
                        protocol: protocol.clone(),
                    });
                }
            }
        }
    }
    out
}

#[async_trait::async_trait]
impl Runtime for DockerRuntime {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn create_and_start_container(
        &mut self,
        name: &str,
        container: &Container,
    ) -> anyhow::Result<String> {
        let image = &container.image;
        tracing::info!("docker: pulling image {}", image);

        // 1. Pull. The stream must be drained for the pull to complete.
        let mut pull = self.client.create_image(
            Some(CreateImageOptions {
                from_image: image.clone(),
                ..Default::default()
            }),
            None,
            None,
        );
        while let Some(msg) = pull.next().await {
            match msg {
                Ok(info) => {
                    if let Some(status) = info.status {
                        tracing::debug!("docker pull: {}", status);
                    }
                }
                Err(e) => return Err(anyhow::anyhow!("docker pull {image}: {e}")),
            }
        }

        // Pod command/args → Docker entrypoint+cmd. Kubernetes semantics:
        //   - `command` overrides the image ENTRYPOINT
        //   - `args` overrides the image CMD
        // Bollard exposes both as Option<Vec<String>>; leaving them None means
        // "inherit from image".
        let entrypoint = container.command.clone();
        let cmd = container.args.clone();

        let env: Option<Vec<String>> = container.env.as_ref().map(|vars| {
            vars.iter()
                .map(|v| format!("{}={}", v.name, v.value.clone().unwrap_or_default()))
                .collect()
        });

        // Port publishing — every declared containerPort gets bound to a
        // *random* host port so superkube's NodePort proxy can reach it via
        // 127.0.0.1:<host_port>. We use random ports because we don't know
        // ahead of time what's free on the host, and the published mapping
        // is reported back via `find_container`.
        let mut exposed: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        if let Some(ports) = &container.ports {
            for p in ports {
                let proto = match p.protocol {
                    Some(Protocol::UDP) => "udp",
                    _ => "tcp",
                };
                let key = format!("{}/{}", p.container_port, proto);
                exposed.insert(key.clone(), HashMap::new());
                bindings.insert(
                    key,
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: None, // None = let Docker pick a free port
                    }]),
                );
            }
        }

        let host_config = if bindings.is_empty() {
            None
        } else {
            Some(HostConfig {
                port_bindings: Some(bindings),
                ..Default::default()
            })
        };

        tracing::info!("docker: creating container {}", name);
        let create = self
            .client
            .create_container(
                Some(CreateContainerOptions {
                    name: name.to_string(),
                    platform: None,
                }),
                Config {
                    image: Some(image.clone()),
                    hostname: Some(name.to_string()),
                    entrypoint,
                    cmd,
                    env,
                    working_dir: container.working_dir.clone(),
                    exposed_ports: if exposed.is_empty() { None } else { Some(exposed) },
                    host_config,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("docker create {name}: {e}"))?;

        self.client
            .start_container(&create.id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| anyhow::anyhow!("docker start {name}: {e}"))?;

        let id = format!("{ID_PREFIX}{}", create.id);
        tracing::info!("docker: started {} as {}", name, id);
        Ok(id)
    }

    async fn is_container_running(&self, container_id: &str) -> anyhow::Result<bool> {
        let id = Self::strip(container_id);
        match self.client.inspect_container(id, None).await {
            Ok(inspect) => Ok(inspect
                .state
                .and_then(|s| s.running)
                .unwrap_or(false)),
            // 404 → container deleted out-of-band; treat as not running.
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("docker inspect {container_id}: {e}")),
        }
    }

    async fn find_container(&self, name: &str) -> anyhow::Result<Option<ContainerInfo>> {
        // Docker's API accepts a container name in any place that takes an ID.
        match self.client.inspect_container(name, None).await {
            Ok(inspect) => {
                let id = inspect.id.clone().unwrap_or_default();
                let state = inspect.state.as_ref();
                let running = state.and_then(|s| s.running).unwrap_or(false);
                let exit_code = state.and_then(|s| s.exit_code).map(|c| c as i32);
                let restart_count = inspect.restart_count.unwrap_or(0) as i32;
                let started_at = state
                    .and_then(|s| s.started_at.as_deref())
                    .filter(|s| !s.is_empty() && *s != "0001-01-01T00:00:00Z")
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc));

                let port_mappings = inspect
                    .network_settings
                    .as_ref()
                    .and_then(|ns| ns.ports.as_ref())
                    .map(|ports| parse_docker_ports(ports))
                    .unwrap_or_default();

                Ok(Some(ContainerInfo {
                    id: format!("{ID_PREFIX}{}", id),
                    running,
                    restart_count,
                    started_at,
                    exit_code,
                    port_mappings,
                }))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("docker find_container {name}: {e}")),
        }
    }

    async fn get_logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> anyhow::Result<String> {
        let id = Self::strip(container_id);
        let tail = options
            .tail_lines
            .map(|n| n.to_string())
            .unwrap_or_else(|| "all".to_string());

        let mut stream = self.client.logs(
            id,
            Some(LogsOptions::<String> {
                stdout: true,
                stderr: true,
                follow: false,
                timestamps: options.timestamps,
                tail,
                ..Default::default()
            }),
        );

        let mut out = String::new();
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(LogOutput::StdOut { message })
                | Ok(LogOutput::StdErr { message })
                | Ok(LogOutput::Console { message })
                | Ok(LogOutput::StdIn { message }) => {
                    out.push_str(&String::from_utf8_lossy(&message));
                }
                Err(e) => return Err(anyhow::anyhow!("docker logs {container_id}: {e}")),
            }
        }

        // since_time is post-filtered: docker's "since" is a unix timestamp
        // and pre-filtering it would skip the timestamps line we need to parse.
        // For our limited use (kubectl logs --since-time), match the mock's
        // behaviour: drop log lines whose timestamp prefix is older than `since`.
        if let Some(since) = options.since_time {
            if options.timestamps {
                out = out
                    .lines()
                    .filter(|line| {
                        line.split_whitespace()
                            .next()
                            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                            .map(|dt| dt >= since)
                            .unwrap_or(true)
                    })
                    .map(|s| {
                        let mut owned = s.to_string();
                        owned.push('\n');
                        owned
                    })
                    .collect();
            }
        }

        if let Some(limit) = options.limit_bytes {
            let limit = limit as usize;
            if out.len() > limit {
                out.truncate(limit);
            }
        }

        Ok(out)
    }

    async fn exec(
        &self,
        container_id: &str,
        cmd: Vec<String>,
        tty: bool,
    ) -> anyhow::Result<ExecSession> {
        let id = Self::strip(container_id);
        let exec = self
            .client
            .create_exec(
                id,
                CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(tty),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("docker create_exec: {e}"))?;

        let result = self
            .client
            .start_exec(&exec.id, None)
            .await
            .map_err(|e| anyhow::anyhow!("docker start_exec: {e}"))?;

        match result {
            StartExecResults::Attached { output, input } => {
                let mapped = output.map(|item| match item {
                    Ok(LogOutput::StdOut { message })
                    | Ok(LogOutput::StdErr { message })
                    | Ok(LogOutput::Console { message })
                    | Ok(LogOutput::StdIn { message }) => Ok(Bytes::from(message.to_vec())),
                    Err(e) => Err(anyhow::anyhow!("exec output: {e}")),
                });

                // The closure captures the bollard client + exec id so the
                // session can resize the TTY without holding the runtime ref.
                let client = self.client.clone();
                let exec_id = exec.id.clone();
                let resize: Box<
                    dyn Fn(u16, u16) -> std::pin::Pin<
                            Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>,
                        > + Send
                        + Sync,
                > = Box::new(move |h: u16, w: u16| {
                    let client = client.clone();
                    let id = exec_id.clone();
                    Box::pin(async move {
                        client
                            .resize_exec(
                                &id,
                                ResizeExecOptions {
                                    height: h,
                                    width: w,
                                },
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("docker resize_exec: {e}"))
                    })
                });

                Ok(ExecSession {
                    id: exec.id,
                    output: Box::pin(mapped),
                    input,
                    resize,
                })
            }
            StartExecResults::Detached => {
                anyhow::bail!("docker exec detached unexpectedly");
            }
        }
    }

    async fn stream_logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> anyhow::Result<LogStream> {
        let id = Self::strip(container_id).to_string();
        let tail = options
            .tail_lines
            .map(|n| n.to_string())
            .unwrap_or_else(|| "all".to_string());

        let stream = self.client.logs(
            &id,
            Some(LogsOptions::<String> {
                stdout: true,
                stderr: true,
                follow: true,
                timestamps: options.timestamps,
                tail,
                ..Default::default()
            }),
        );

        // Map bollard's framed LogOutput into raw bytes. Drop the
        // stdout/stderr distinction — kubectl just wants a single text stream.
        let mapped = stream.map(|item| match item {
            Ok(LogOutput::StdOut { message })
            | Ok(LogOutput::StdErr { message })
            | Ok(LogOutput::Console { message })
            | Ok(LogOutput::StdIn { message }) => Ok(Bytes::from(message.to_vec())),
            Err(e) => Err(anyhow::anyhow!("docker logs stream: {e}")),
        });

        Ok(Box::pin(mapped))
    }
}
