//! Pull OCI images from a registry and unpack their layers into a rootfs.
//!
//! Layout on disk (under `dest_root`):
//!
//! ```text
//! <dest_root>/
//! └── <safe-name>/
//!     ├── rootfs/        # unpacked layers, applied in order
//!     ├── config.json    # OCI image config (entrypoint, env, ...)
//!     └── .pulled        # sentinel: present iff pull completed
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use flate2::read::GzDecoder;
use oci_distribution::manifest::{
    IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
};
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::{Client, Reference};
use serde::{Deserialize, Serialize};

/// Subset of the OCI image config we care about for running a container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

/// Result of a successful pull.
pub struct PulledImage {
    /// Directory containing the merged rootfs.
    pub rootfs: PathBuf,
    /// Parsed OCI image config (entrypoint, env, ...).
    pub config: ImageConfig,
}

/// Pull `reference` (e.g. `"nginx:latest"`, `"docker.io/library/alpine:3.20"`)
/// into `dest_root`. Re-uses an existing extraction if the sentinel exists.
pub async fn pull(reference: &str, dest_root: &Path) -> anyhow::Result<PulledImage> {
    let reference: Reference = reference
        .parse()
        .with_context(|| format!("invalid image reference: {reference}"))?;

    let safe_name = sanitize(&reference.whole());
    let image_dir = dest_root.join(&safe_name);
    let rootfs = image_dir.join("rootfs");
    let config_path = image_dir.join("config.json");
    let sentinel = image_dir.join(".pulled");

    if sentinel.exists() && config_path.exists() && rootfs.exists() {
        let config: ImageConfig = serde_json::from_slice(&std::fs::read(&config_path)?)
            .context("re-reading cached image config")?;
        return Ok(PulledImage { rootfs, config });
    }

    // Fresh pull. Wipe any half-extracted state.
    if image_dir.exists() {
        std::fs::remove_dir_all(&image_dir).ok();
    }
    std::fs::create_dir_all(&rootfs)?;

    let client = Client::default();
    let auth = RegistryAuth::Anonymous;
    let accepted = vec![
        IMAGE_LAYER_MEDIA_TYPE,
        IMAGE_LAYER_GZIP_MEDIA_TYPE,
        IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
    ];

    tracing::info!("pulling image {}", reference);
    let image = client
        .pull(&reference, &auth, accepted)
        .await
        .with_context(|| format!("pulling {reference}"))?;

    // Apply layers in order. Each layer is a (gzipped) tarball; non-gzipped
    // layers also exist in some registries.
    for (i, layer) in image.layers.iter().enumerate() {
        tracing::debug!(
            "extracting layer {}/{} ({} bytes, type={})",
            i + 1,
            image.layers.len(),
            layer.data.len(),
            layer.media_type,
        );

        if is_gzip_media_type(&layer.media_type) {
            let decoder = GzDecoder::new(layer.data.as_slice());
            extract_tar(decoder, &rootfs)?;
        } else {
            extract_tar(layer.data.as_slice(), &rootfs)?;
        }
    }

    // Parse the OCI image config (small JSON blob).
    let config = parse_config(&image.config.data).unwrap_or_default();

    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    std::fs::write(&sentinel, b"")?;

    Ok(PulledImage { rootfs, config })
}

fn is_gzip_media_type(t: &str) -> bool {
    t == IMAGE_LAYER_GZIP_MEDIA_TYPE || t == IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE
}

/// Extract a tarball, honouring opaque-dir + whiteout markers per the
/// OCI image-spec layer rules.
fn extract_tar<R: Read>(reader: R, target: &Path) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(true);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        // Opaque whiteout: ".wh..wh..opq" means "remove all siblings then keep this dir".
        if file_name == ".wh..wh..opq" {
            if let Some(parent) = path.parent() {
                let dir = target.join(parent);
                clear_directory(&dir).ok();
            }
            continue;
        }

        // Whiteout: ".wh.foo" means "delete sibling 'foo'".
        if let Some(deleted) = file_name.strip_prefix(".wh.") {
            let parent = path.parent().unwrap_or_else(|| Path::new(""));
            let victim = target.join(parent).join(deleted);
            if victim.is_dir() {
                std::fs::remove_dir_all(&victim).ok();
            } else if victim.exists() || victim.is_symlink() {
                std::fs::remove_file(&victim).ok();
            }
            continue;
        }

        // Skip absolute / parent-traversal entries (defence-in-depth).
        if path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            continue;
        }

        entry.unpack_in(target)?;
    }

    Ok(())
}

fn clear_directory(dir: &Path) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Parse the OCI image config blob into the subset we care about. The blob
/// looks like `{ "config": { "Entrypoint": [...], "Cmd": [...], "Env": [...], ... }, ... }`.
fn parse_config(data: &[u8]) -> anyhow::Result<ImageConfig> {
    let v: serde_json::Value = serde_json::from_slice(data).context("parsing image config")?;
    let cfg = v
        .get("config")
        .ok_or_else(|| anyhow!("image config missing `config` field"))?;

    let entrypoint = string_array(cfg.get("Entrypoint"));
    let cmd = string_array(cfg.get("Cmd"));
    let env = string_array(cfg.get("Env"));
    let working_dir = cfg
        .get("WorkingDir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let user = cfg
        .get("User")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Ok(ImageConfig {
        entrypoint,
        cmd,
        env,
        working_dir,
        user,
    })
}

fn string_array(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Map an image reference to a filesystem-safe directory name.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}
