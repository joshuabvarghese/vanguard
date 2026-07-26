//! Emits the TenantPipeline CustomResourceDefinition as YAML, generated
//! directly from the Rust struct via `kube::CustomResourceExt::crd()`.
//!
//! Regenerate `manifests/crd/tenantpipeline.yaml` with:
//!   cargo run --bin crdgen > manifests/crd/tenantpipeline.yaml
//!
//! This is the idiomatic kube-rs alternative to hand-maintaining (or
//! porting from another language's) a CRD YAML that can silently drift out
//! of sync with what the operator actually reads and writes.

use kube::CustomResourceExt;
use vanguard::crd::TenantPipeline;

fn main() {
    let crd = TenantPipeline::crd();
    print!(
        "{}",
        serde_yaml::to_string(&crd).expect("CRD must serialize to YAML")
    );
}
