//! Self-heal narration for the demo, ported from `internal/chaos/engine.go`.
//! Subscribes to the store event bus; on a `ChaosInjected` event, narrates a
//! realistic-looking recovery sequence while the real reconciler (already
//! watching for the Deployment's deletion) does the actual healing.

use crate::store::{Event, EventKind, Store, TenantPhase};
use std::sync::Arc;
use std::time::Duration;

fn steps(tenant_id: &str) -> Vec<(Duration, String, EventKind)> {
    vec![
        (
            Duration::from_millis(150),
            format!("  ⚠  [{tenant_id}] drift detected — proxy deployment missing"),
            EventKind::DriftDetected,
        ),
        (
            Duration::from_millis(300),
            format!("  ⟳  [{tenant_id}] reconcile triggered by informer cache miss"),
            EventKind::ReconcileStart,
        ),
        (
            Duration::from_millis(600),
            format!("  ✓  [{tenant_id}] proxy deployment re-created"),
            EventKind::ProxyRestored,
        ),
        (
            Duration::from_millis(900),
            format!("  ✓  [{tenant_id}] configmap re-mounted and verified"),
            EventKind::ConfigReloaded,
        ),
        (
            Duration::from_millis(1100),
            format!("  ✓  [{tenant_id}] tenant pipeline back to READY ⚡ self-heal complete"),
            EventKind::ReconcileOk,
        ),
    ]
}

pub struct Engine {
    store: Arc<Store>,
}

impl Engine {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Blocks until `shutdown` fires. Spawn this in its own task.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut events = self.store.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return;
                    }
                }
                res = events.recv() => {
                    match res {
                        Ok(ev) if ev.kind == EventKind::ChaosInjected => {
                            let store = self.store.clone();
                            let tenant_id = ev.tenant_id.clone();
                            tokio::spawn(async move {
                                heal_sequence(&store, &tenant_id, None).await;
                            });
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    }
}

/// Narrates the self-healing pipeline with realistic timing. `cancel`, if
/// provided, lets tests/shutdown abort mid-sequence — mirroring the Go
/// version's `ctx.Done()` select case.
async fn heal_sequence(
    store: &Arc<Store>,
    tenant_id: &str,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) {
    let mut cancel = cancel;

    for (delay, msg, kind) in steps(tenant_id) {
        if let Some(rx) = cancel.as_mut() {
            tokio::select! {
                _ = rx.changed() => {
                    if *rx.borrow() {
                        return;
                    }
                }
                _ = tokio::time::sleep(delay) => {}
            }
        } else {
            tokio::time::sleep(delay).await;
        }

        let ts = chrono::Utc::now().format("%H:%M:%S%.3f");
        store.append_log(format!("[{ts}] {msg}"));
        store.publish(Event::new(kind, tenant_id.to_string(), msg));
    }

    if let Some(mut rec) = store.get_tenant(tenant_id) {
        rec.phase = TenantPhase::Ready;
        rec.last_reconcile_at = Some(chrono::Utc::now());
        rec.reconcile_count += 1;
        store.upsert_tenant(rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TenantRecord;
    use std::time::Duration as StdDuration;
    use tokio::time::timeout;

    fn seed_degraded(store: &Store, tenant_id: &str) {
        store.upsert_tenant(TenantRecord {
            tenant_id: tenant_id.to_string(),
            phase: TenantPhase::Degraded,
            reconcile_count: 3,
            ..Default::default()
        });
    }

    #[tokio::test]
    async fn heal_sequence_emits_full_narration_and_restores_phase() {
        let store = Arc::new(Store::new());
        seed_degraded(&store, "acme");
        let mut events = store.subscribe();

        heal_sequence(&store, "acme", None).await;

        let want = [
            EventKind::DriftDetected,
            EventKind::ReconcileStart,
            EventKind::ProxyRestored,
            EventKind::ConfigReloaded,
            EventKind::ReconcileOk,
        ];
        for kind in want {
            let ev = timeout(StdDuration::from_secs(3), events.recv())
                .await
                .expect("timed out")
                .unwrap();
            assert_eq!(ev.kind, kind);
            assert_eq!(ev.tenant_id, "acme");
        }

        let rec = store.get_tenant("acme").unwrap();
        assert_eq!(rec.phase, TenantPhase::Ready);
        assert_eq!(rec.reconcile_count, 4);
    }

    #[tokio::test]
    async fn heal_sequence_stops_on_cancellation() {
        let store = Arc::new(Store::new());
        seed_degraded(&store, "acme");

        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap(); // already "cancelled" before the sequence starts

        let store2 = store.clone();
        let handle = tokio::spawn(async move {
            heal_sequence(&store2, "acme", Some(rx)).await;
        });
        timeout(StdDuration::from_secs(2), handle)
            .await
            .expect("did not exit promptly")
            .unwrap();

        // No step should have run — phase must remain Degraded.
        assert_eq!(
            store.get_tenant("acme").unwrap().phase,
            TenantPhase::Degraded
        );
    }

    #[tokio::test]
    async fn engine_run_only_triggers_on_chaos_injected_events() {
        let store = Arc::new(Store::new());
        seed_degraded(&store, "acme");
        let engine = Engine::new(store.clone());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let engine_store = store.clone();
        let handle = tokio::spawn(async move {
            Engine::new(engine_store).run(shutdown_rx).await;
        });
        let _ = &engine; // constructed above only to mirror the public API shape

        store.publish(Event::new(EventKind::ReconcileStart, "acme", "unrelated"));
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        assert_eq!(
            store.get_tenant("acme").unwrap().phase,
            TenantPhase::Degraded,
            "unrelated event must not trigger heal"
        );

        store.publish(Event::new(EventKind::ChaosInjected, "acme", "boom"));

        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(4);
        loop {
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for chaos-triggered heal to complete");
            }
            if let Some(rec) = store.get_tenant("acme") {
                if rec.phase == TenantPhase::Ready {
                    break;
                }
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }

        shutdown_tx.send(true).unwrap();
        handle.await.unwrap();
    }
}
