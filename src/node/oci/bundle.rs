//! Build an OCI runtime bundle (`config.json` + rootfs path) from a Pod
//! container spec and the image config we pulled.
//!
//! An OCI bundle directory looks like:
//!
//! ```text
//! bundle/
//! ├── config.json    # OCI runtime spec (this file generates it)
//! └── rootfs/        # extracted image layers (caller links/copies this in)
//! ```
//!
//! The runtime spec captured here:
//!   - merges Pod command/args with image Entrypoint/Cmd per Kubernetes rules
//!   - merges env vars (pod overrides image)
//!   - sets working dir (pod > image > "/")
//!   - sets user (parsed from image config; "0" if missing)
//!   - declares the standard set of Linux namespaces (pid/net/ipc/uts/mount)
//!   - mounts /proc, /sys, /dev, /dev/pts, /dev/shm with sane defaults
//!
//! Resource limits, seccomp profiles, AppArmor, and user namespaces are
//! intentionally left out for the first slice — they layer on top later.

use std::path::Path;

use anyhow::{Context, Result};
use oci_spec::runtime::{
    LinuxBuilder, LinuxNamespaceBuilder, LinuxNamespaceType, MountBuilder, ProcessBuilder,
    RootBuilder, Spec, SpecBuilder, UserBuilder,
};

use crate::models::Container;

use super::image::ImageConfig;

/// Inputs needed to build a bundle for one container in a pod.
pub struct BundleInputs<'a> {
    /// Pod-level hostname (typically the pod name).
    pub hostname: &'a str,
    /// Path to the rootfs *as it should appear in config.json*.
    /// Convention: `"rootfs"` (i.e. relative to the bundle dir).
    pub rootfs_path: &'a str,
    /// The Pod's container spec.
    pub container: &'a Container,
    /// The image config we pulled for this container.
    pub image: &'a ImageConfig,
    /// Optional pre-created network namespace path. When present, libcontainer
    /// joins this existing netns (set up by our mini-CNI) instead of creating
    /// a fresh one. Convention: `/var/run/netns/<pod_name>`.
    pub netns_path: Option<&'a str>,
}

/// Generate an OCI runtime spec and write it to `<bundle_dir>/config.json`.
pub fn write_config(bundle_dir: &Path, inputs: &BundleInputs<'_>) -> Result<()> {
    let spec = build_spec(inputs)?;

    std::fs::create_dir_all(bundle_dir)?;
    let path = bundle_dir.join("config.json");
    let bytes = serde_json::to_vec_pretty(&spec).context("serializing OCI runtime spec")?;
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Pure builder — exposed for tests so we don't need to hit the filesystem.
pub fn build_spec(inputs: &BundleInputs<'_>) -> Result<Spec> {
    let args = merge_args(inputs.container, inputs.image);
    let env = merge_env(inputs.container, inputs.image);
    let cwd = pick_cwd(inputs.container, inputs.image);
    let (uid, gid) = parse_user(inputs.image.user.as_deref());

    let process = ProcessBuilder::default()
        .terminal(false)
        .user(UserBuilder::default().uid(uid).gid(gid).build()?)
        .args(args)
        .env(env)
        .cwd(cwd)
        .build()
        .context("building OCI process")?;

    let root = RootBuilder::default()
        .path(inputs.rootfs_path.to_string())
        .readonly(false)
        .build()
        .context("building OCI root")?;

    let linux = LinuxBuilder::default()
        .namespaces(default_namespaces(inputs.netns_path)?)
        .build()
        .context("building OCI linux section")?;

    let spec = SpecBuilder::default()
        .version("1.0.2-dev")
        .hostname(inputs.hostname.to_string())
        .process(process)
        .root(root)
        .mounts(default_mounts()?)
        .linux(linux)
        .build()
        .context("building OCI spec")?;

    Ok(spec)
}

/// Kubernetes command/args → OCI args, per the documented merge rules:
///   pod.command? + pod.args?  →  args
///   else fall back to image.entrypoint + image.cmd
fn merge_args(c: &Container, img: &ImageConfig) -> Vec<String> {
    let entrypoint = c
        .command
        .clone()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| img.entrypoint.clone());
    let cmd = c
        .args
        .clone()
        .or_else(|| {
            // If pod.command was set but pod.args wasn't, drop the image cmd
            // (Kubernetes semantics: command overrides entrypoint AND clears cmd).
            if c.command.is_some() {
                Some(Vec::new())
            } else {
                None
            }
        })
        .unwrap_or_else(|| img.cmd.clone());

    let mut out = entrypoint;
    out.extend(cmd);
    if out.is_empty() {
        // Should never happen for a real image, but don't blow up — runc would
        // also error here. Pick a sentinel so the failure is obvious.
        out.push("/bin/sh".to_string());
    }
    out
}

/// Image env first, then pod env. Pod entries with the same key win because
/// they appear later — most OCI runtimes treat the last value as authoritative.
fn merge_env(c: &Container, img: &ImageConfig) -> Vec<String> {
    let mut env = img.env.clone();
    if let Some(pod_env) = &c.env {
        for v in pod_env {
            let value = v.value.as_deref().unwrap_or("");
            env.push(format!("{}={}", v.name, value));
        }
    }
    if env.iter().all(|e| !e.starts_with("PATH=")) {
        env.push("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
    }
    env
}

fn pick_cwd(c: &Container, img: &ImageConfig) -> String {
    c.working_dir
        .clone()
        .or_else(|| img.working_dir.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/".to_string())
}

/// Parse the image config's User field (e.g. "1000", "1000:1000", "root").
/// We can't resolve names without an /etc/passwd lookup inside the rootfs
/// (which we don't do in this slice), so non-numeric → 0.
fn parse_user(s: Option<&str>) -> (u32, u32) {
    let s = match s {
        Some(s) if !s.is_empty() => s,
        _ => return (0, 0),
    };
    let mut parts = s.splitn(2, ':');
    let uid = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let gid = parts.next().and_then(|p| p.parse().ok()).unwrap_or(uid);
    (uid, gid)
}

fn default_namespaces(
    netns_path: Option<&str>,
) -> Result<Vec<oci_spec::runtime::LinuxNamespace>> {
    let mut out = Vec::new();
    for kind in [
        LinuxNamespaceType::Pid,
        LinuxNamespaceType::Ipc,
        LinuxNamespaceType::Uts,
        LinuxNamespaceType::Mount,
    ] {
        out.push(
            LinuxNamespaceBuilder::default()
                .typ(kind)
                .build()
                .context("building namespace")?,
        );
    }
    // Network namespace: join the pre-built one if the caller passed a path,
    // otherwise let libcontainer create a fresh (and isolated, no-network) one.
    let net_ns = match netns_path {
        Some(p) => LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Network)
            .path(std::path::PathBuf::from(p))
            .build(),
        None => LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::Network)
            .build(),
    }
    .context("building network namespace")?;
    out.push(net_ns);
    Ok(out)
}

/// The standard set of mounts every OCI container expects.
fn default_mounts() -> Result<Vec<oci_spec::runtime::Mount>> {
    let mut mounts = Vec::new();

    mounts.push(
        MountBuilder::default()
            .destination("/proc")
            .typ("proc")
            .source("proc")
            .build()
            .context("mount /proc")?,
    );

    mounts.push(
        MountBuilder::default()
            .destination("/dev")
            .typ("tmpfs")
            .source("tmpfs")
            .options(vec![
                "nosuid".to_string(),
                "strictatime".to_string(),
                "mode=755".to_string(),
                "size=65536k".to_string(),
            ])
            .build()
            .context("mount /dev")?,
    );

    mounts.push(
        MountBuilder::default()
            .destination("/dev/pts")
            .typ("devpts")
            .source("devpts")
            .options(vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "newinstance".to_string(),
                "ptmxmode=0666".to_string(),
                "mode=0620".to_string(),
                "gid=5".to_string(),
            ])
            .build()
            .context("mount /dev/pts")?,
    );

    mounts.push(
        MountBuilder::default()
            .destination("/dev/shm")
            .typ("tmpfs")
            .source("shm")
            .options(vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
                "mode=1777".to_string(),
                "size=65536k".to_string(),
            ])
            .build()
            .context("mount /dev/shm")?,
    );

    mounts.push(
        MountBuilder::default()
            .destination("/sys")
            .typ("sysfs")
            .source("sysfs")
            .options(vec![
                "nosuid".to_string(),
                "noexec".to_string(),
                "nodev".to_string(),
                "ro".to_string(),
            ])
            .build()
            .context("mount /sys")?,
    );

    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EnvVar;

    fn img() -> ImageConfig {
        ImageConfig {
            entrypoint: vec!["/docker-entrypoint.sh".into()],
            cmd: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            env: vec!["PATH=/usr/local/sbin:/usr/bin".into(), "NGINX_VERSION=1.27".into()],
            working_dir: Some("/".into()),
            user: Some("0".into()),
        }
    }

    fn container(name: &str, image: &str) -> Container {
        Container {
            name: name.into(),
            image: image.into(),
            image_pull_policy: None,
            command: None,
            args: None,
            env: None,
            ports: None,
            resources: None,
            working_dir: None,
        }
    }

    #[test]
    fn merge_uses_image_when_pod_silent() {
        let c = container("c", "nginx");
        let args = merge_args(&c, &img());
        assert_eq!(
            args,
            vec!["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
    }

    #[test]
    fn pod_command_overrides_entrypoint_and_drops_image_cmd() {
        let mut c = container("c", "nginx");
        c.command = Some(vec!["sleep".into()]);
        let args = merge_args(&c, &img());
        assert_eq!(args, vec!["sleep"]);
    }

    #[test]
    fn pod_args_only_keeps_image_entrypoint() {
        let mut c = container("c", "nginx");
        c.args = Some(vec!["-V".into()]);
        let args = merge_args(&c, &img());
        assert_eq!(args, vec!["/docker-entrypoint.sh", "-V"]);
    }

    #[test]
    fn pod_env_appended_after_image_env() {
        let mut c = container("c", "nginx");
        c.env = Some(vec![EnvVar {
            name: "HELLO".into(),
            value: Some("world".into()),
        }]);
        let env = merge_env(&c, &img());
        assert!(env.contains(&"NGINX_VERSION=1.27".into()));
        assert!(env.contains(&"HELLO=world".into()));
        assert_eq!(env.last().unwrap(), "HELLO=world");
    }

    #[test]
    fn build_spec_smoke_test() {
        let c = container("c", "nginx");
        let inputs = BundleInputs {
            hostname: "demo",
            rootfs_path: "rootfs",
            container: &c,
            image: &img(),
            netns_path: None,
        };
        let spec = build_spec(&inputs).unwrap();
        assert_eq!(spec.hostname().as_deref(), Some("demo"));
        let p = spec.process().as_ref().unwrap();
        assert!(p.args().as_ref().unwrap().contains(&"nginx".to_string()));
    }
}
