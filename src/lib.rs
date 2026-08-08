//! Vanguard library crate — shared modules used by the `vanguard` binary
//! (`src/main.rs`, a real `kube-rs` operator against any Kubernetes API
//! server), the `vanguard-demo` binary (`src/bin/demo.rs`, the identical
//! REST API and reconcile logic wired to an in-memory mock backend so the
//! project can be tried from a browser with zero setup — see the README's
//! "Live demo" section), and the `crdgen` helper binary
//! (`src/bin/crdgen.rs`, which emits the CRD manifest generated directly
//! from these Rust types via `kube::CustomResourceExt`, so the schema
//! checked into `manifests/crd/` can never drift from the actual struct).

pub mod api;
pub mod chaos;
pub mod cloud;
pub mod crd;
pub mod demo;
pub mod k8s_backend;
pub mod operator;
pub mod reconcile;
pub mod store;
pub mod tui;
