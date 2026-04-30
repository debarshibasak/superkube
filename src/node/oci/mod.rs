//! Embedded OCI runtime: pulls images and (on Linux) runs containers via
//! `libcontainer` — no host containerd / runc / CNI required.
//!
//! Currently in-progress. The cross-platform pieces (image pull, bundle
//! generation) live here; the `libcontainer`-driven runtime is Linux-only and
//! lives in `container.rs` (TODO).

pub mod bundle;
pub mod image;
