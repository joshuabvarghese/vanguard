//! Wires `reconcile.rs`'s pure business logic into a real
//! `kube::runtime::Controller` watch loop against a live Kubernetes API
//! server. This module is intentionally thin — it does object fetch,
//! finalizer bookkeeping, and status-subresource patching, then delegates
//! everything else to `reconcile::reconcile_tenant`.
//!
//! Status patches use `Patch::Merge` with a hand-built JSON document
//! containing only the fields changed — there is no equivalent to the Go
//! version's `client.MergeFrom(base)` snapshot-diff pattern, so the bug
//! class documented in that project's POSTMORTEM.md (patch silently
//! no-ops because the snapshot was taken from the same object that was then
//! mutated) doesn't have an analogue here: the patch body is built fresh
//! each time from just the values being written.

use futures::StreamExt;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::{watcher, Controller};
use kube::Resource;
use kube::{Api, Client, ResourceExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

use crate::crd::{TenantPipeline, FINALIZER};
use crate::k8s_backend::{K8sBackend, KubeBackend};
use crate::reconcile::{
    apply_degraded_status, apply_ready_status, handle_deletion, reconcile_tenant,
};
use crate::store::Store;

const STATUS_FIELD_MANAGER: &str = "vanguard-operator-status";
const DRIFT_CHECK_INTERVAL: Duration = Duration::from_secs(15);
const ERROR_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub struct OperatorContext {
    pub client: Client,
    pub backend: Arc<dyn K8sBackend>,
    pub store: Arc<Store>,
}

/// Starts the Controller and runs until the process is asked to shut down
/// (the returned future only completes when the watch stream ends).
pub async fn run(client: Client, store: Arc<Store>) {
    let backend: Arc<dyn K8sBackend> = Arc::new(KubeBackend::new(client.clone()));
    let ctx = Arc::new(OperatorContext {
        client: client.clone(),
        backend,
        store,
    });

    let tenants: Api<TenantPipeline> = Api::all(client);

    info!("starting TenantPipeline controller");
    Controller::new(tenants, watcher::Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            match res {
                Ok(action) => info!(?action, "reconcile succeeded"),
                Err(e) => error!(error = %e, "reconcile failed"),
            }
        })
        .await;
}

async fn reconcile(
    tp: Arc<TenantPipeline>,
    ctx: Arc<OperatorContext>,
) -> Result<Action, kube::Error> {
    let tenant_id = tp.spec.tenant_id.clone();
    let name = tp.name_any();
    let api: Api<TenantPipeline> = Api::all(ctx.client.clone());

    // ── Deletion / finalizer handling ────────────────────────────────────────
    if tp.meta().deletion_timestamp.is_some() {
        if tp.finalizers().iter().any(|f| f == FINALIZER) {
            let _ = handle_deletion(ctx.backend.as_ref(), &ctx.store, &tenant_id).await;
            let mut finalizers = tp.finalizers().to_vec();
            finalizers.retain(|f| f != FINALIZER);
            let patch = json!({ "metadata": { "finalizers": finalizers } });
            api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
        }
        return Ok(Action::await_change());
    }

    if !tp.finalizers().iter().any(|f| f == FINALIZER) {
        let mut finalizers = tp.finalizers().to_vec();
        finalizers.push(FINALIZER.to_string());
        let patch = json!({ "metadata": { "finalizers": finalizers } });
        api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        return Ok(Action::requeue(Duration::from_millis(100)));
    }

    // ── Paused guard ─────────────────────────────────────────────────────────
    if tp.spec.paused {
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    // ── Drive desired state (the actual business logic) ──────────────────────
    let mut status = tp.status.clone().unwrap_or_default();
    let observed_generation = tp.meta().generation.unwrap_or(0);
    let created_at = tp.creation_timestamp().map(|t| t.0);

    match reconcile_tenant(
        ctx.backend.as_ref(),
        &ctx.store,
        &tp.spec,
        status.reconcile_count,
        created_at,
    )
    .await
    {
        Ok(outcome) => {
            apply_ready_status(&mut status, &outcome, observed_generation);
            patch_status(&api, &name, &status).await?;
            Ok(Action::requeue(DRIFT_CHECK_INTERVAL))
        }
        Err(err) => {
            apply_degraded_status(
                &ctx.store,
                &mut status,
                &tenant_id,
                &tp.spec.display_name,
                &err,
            );
            patch_status(&api, &name, &status).await?;
            Ok(Action::requeue(ERROR_RETRY_INTERVAL))
        }
    }
}

async fn patch_status(
    api: &Api<TenantPipeline>,
    name: &str,
    status: &crate::crd::TenantPipelineStatus,
) -> Result<(), kube::Error> {
    let patch = json!({ "status": status });
    api.patch_status(
        name,
        &PatchParams::apply(STATUS_FIELD_MANAGER).force(),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

fn on_error(_tp: Arc<TenantPipeline>, err: &kube::Error, _ctx: Arc<OperatorContext>) -> Action {
    error!(error = %err, "reconciler error, retrying");
    Action::requeue(ERROR_RETRY_INTERVAL)
}
