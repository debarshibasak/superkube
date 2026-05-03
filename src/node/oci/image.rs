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

/// OCI artifact media types we recognize as "this layer is a raw `.wasm`
/// module, not a tarball". Different tools have shipped different vendor
/// strings over the years; we accept the union.
const WASM_LAYER_MEDIA_TYPES: &[&str] = &[
    "application/vnd.wasm.content.layer.v1+wasm",
    "application/vnd.module.wasm.content.layer.v1+wasm",
    "application/wasm",
];

/// Pull a WASM module by OCI reference and return the path to a single
/// `.wasm` file on disk.
///
/// Two strategies, tried in order:
///
/// 1. **Wasm artifact pull.** If any layer in the manifest carries a wasm
///    media type, write the first such layer raw to `<dest>/module.wasm`.
/// 2. **Regular OCI image fallback.** Extract tar layers as in [`pull`],
///    then look for a `.wasm` file in the resulting rootfs (preferring
///    `main.wasm` / `app.wasm` / `module.wasm` / `index.wasm`, otherwise
///    the first one we find).
///
/// Local references — `file://`, `wasm://`, or an absolute path — are
/// treated as already-resolved and returned as-is, which lets the wasm
/// runtime accept both registry refs and local paths.
pub async fn pull_wasm(reference: &str, dest_root: &Path) -> anyhow::Result<PathBuf> {
    if let Some(p) = reference.strip_prefix("file://") {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = reference.strip_prefix("wasm://") {
        return Ok(PathBuf::from(p));
    }
    if reference.starts_with('/') {
        return Ok(PathBuf::from(reference));
    }

    let parsed: Reference = reference
        .parse()
        .with_context(|| format!("invalid wasm image reference: {reference}"))?;

    let safe = sanitize(&parsed.whole());
    let dir = dest_root.join(&safe);
    let module = dir.join("module.wasm");
    let sentinel = dir.join(".pulled-wasm");

    if sentinel.exists() && module.exists() {
        return Ok(module);
    }

    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::create_dir_all(&dir)?;

    let client = Client::default();
    let auth = RegistryAuth::Anonymous;
    let mut accepted: Vec<&str> = WASM_LAYER_MEDIA_TYPES.to_vec();
    accepted.extend([
        IMAGE_LAYER_MEDIA_TYPE,
        IMAGE_LAYER_GZIP_MEDIA_TYPE,
        IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
    ]);

    tracing::info!("pulling wasm artifact {}", parsed);
    let image = client
        .pull(&parsed, &auth, accepted)
        .await
        .with_context(|| format!("pulling {parsed}"))?;

    // Strategy 1: a layer is a raw wasm module.
    if let Some(layer) = image
        .layers
        .iter()
        .find(|l| WASM_LAYER_MEDIA_TYPES.contains(&l.media_type.as_str()))
    {
        std::fs::write(&module, &layer.data)
            .with_context(|| format!("writing wasm layer to {}", module.display()))?;
        std::fs::write(&sentinel, b"")?;
        tracing::info!(
            "pulled wasm artifact {} → {} ({} bytes)",
            parsed,
            module.display(),
            layer.data.len()
        );
        return Ok(module);
    }

    // Strategy 2: regular OCI tar layers; extract and search.
    let rootfs = dir.join("rootfs");
    std::fs::create_dir_all(&rootfs)?;
    for layer in &image.layers {
        if is_gzip_media_type(&layer.media_type) {
            let decoder = GzDecoder::new(layer.data.as_slice());
            extract_tar(decoder, &rootfs)?;
        } else if layer.media_type == IMAGE_LAYER_MEDIA_TYPE {
            extract_tar(layer.data.as_slice(), &rootfs)?;
        }
    }
    let found = find_wasm_in(&rootfs)?
        .ok_or_else(|| anyhow!("no .wasm file found in image {} layers", parsed))?;
    std::fs::copy(&found, &module).with_context(|| {
        format!(
            "copying {} → {}",
            found.display(),
            module.display()
        )
    })?;
    // Best-effort cleanup of the rootfs once we've extracted what we wanted.
    std::fs::remove_dir_all(&rootfs).ok();
    std::fs::write(&sentinel, b"")?;
    tracing::info!("pulled OCI image {} → {} (extracted)", parsed, module.display());
    Ok(module)
}

fn find_wasm_in(root: &Path) -> anyhow::Result<Option<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let e = entry?;
            let p = e.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().and_then(|x| x.to_str()) == Some("wasm") {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(root, &mut found)?;
    let preferred = ["main.wasm", "app.wasm", "module.wasm", "index.wasm"];
    for name in preferred {
        if let Some(p) = found
            .iter()
            .find(|p| p.file_name().and_then(|s| s.to_str()) == Some(name))
        {
            return Ok(Some(p.clone()));
        }
    }
    Ok(found.into_iter().next())
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
