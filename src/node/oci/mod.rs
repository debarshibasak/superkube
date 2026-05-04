//! Embedded OCI runtime: pulls images and (on Linux) runs containers via
//! `libcontainer` — no host containerd / runc / CNI required.
//!
//! `bundle` is Linux-only because it's wired into the libcontainer-backed
//! `embedded` runtime; `image` is cross-platform (the wasm runtime uses
//! [`image::pull_wasm`] on every host).

#[cfg(target_os = "linux")]
pub mod bundle;
pub mod image;
