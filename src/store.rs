//! Shared in-memory state store and event bus connecting the operator, REST
//! API, and TUI without an external message broker. Ported from
//! `pkg/config/store.go`; the Go version's `GetTenant`/`ListTenants` used to
//! return live pointers into the map, which raced with in-place mutation
//! elsewhere (see that file's doc comment). This port returns owned clones
//! from every read, which the borrow checker would in fact reject doing
//! unsafely — but the *design* intent (never hand out a live reference into
//! the map) is kept explicit here rather than relied upon implicitly.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::broadcast;

pub const MAX_LOGS: usize = 200;
const EVENT_BUS_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(dead_code)] // TenantUpdated/ProxyDown mirror the Go event taxonomy; not yet wired to a call site
pub enum EventKind {
    TenantCreated,
    TenantUpdated,
    TenantDeleted,
    NamespaceReady,
    ProxyUp,
    ProxyDown,
    ProxyRestored,
    ConfigReloaded,
    ReconcileStart,
    ReconcileOk,
    ReconcileError,
    DriftDetected,
    ChaosInjected,
}

impl EventKind {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::TenantCreated => "TENANT_CREATED",
            EventKind::TenantUpdated => "TENANT_UPDATED",
            EventKind::TenantDeleted => "TENANT_DELETED",
            EventKind::NamespaceReady => "NAMESPACE_READY",
            EventKind::ProxyUp => "PROXY_UP",
            EventKind::ProxyDown => "PROXY_DOWN",
            EventKind::ProxyRestored => "PROXY_RESTORED",
            EventKind::ConfigReloaded => "CONFIG_RELOADED",
            EventKind::ReconcileStart => "RECONCILE_START",
            EventKind::ReconcileOk => "RECONCILE_OK",
            EventKind::ReconcileError => "RECONCILE_ERROR",
            EventKind::DriftDetected => "DRIFT_DETECTED",
            EventKind::ChaosInjected => "CHAOS_INJECTED",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub kind: EventKind,
    pub tenant_id: String,
    pub message: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl Event {
    pub fn new(kind: EventKind, tenant_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            tenant_id: tenant_id.into(),
            message: message.into(),
            timestamp: chrono::Utc::now(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[allow(dead_code)] // Terminating mirrors the Go Phase enum; matched in tui.rs, not yet constructed
pub enum TenantPhase {
    #[default]
    Provisioning,
    Ready,
    Degraded,
    Terminating,
}

impl TenantPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            TenantPhase::Provisioning => "Provisioning",
            TenantPhase::Ready => "Ready",
            TenantPhase::Degraded => "Degraded",
            TenantPhase::Terminating => "Terminating",
        }
    }
}

/// Live view of a single tenant held in the store.
#[derive(Clone, Debug, Default)]
pub struct TenantRecord {
    pub tenant_id: String,
    pub display_name: String,
    pub namespace: String,
    pub phase: TenantPhase,
    pub tier: String,
    pub rps: i32,
    pub burst: i32,
    pub max_concurrent: i32,
    pub proxy_pod_name: String,
    pub proxy_image: String,
    pub config_map_name: String,
    pub reconcile_count: i64,
    pub last_reconcile_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Central in-memory state shared between the operator task, REST API
/// handlers, and the TUI render loop. Every method takes `&self` (not `&mut
/// self`) — interior mutability via `Mutex`/`broadcast::Sender` — so a single
/// `Arc<Store>` can be cloned freely across tokio tasks.
pub struct Store {
    tenants: Mutex<HashMap<String, TenantRecord>>,
    logs: Mutex<VecDeque<String>>,
    bus: broadcast::Sender<Event>,
}

impl Store {
    pub fn new() -> Self {
        let (bus, _rx) = broadcast::channel(EVENT_BUS_CAPACITY);
        Self {
            tenants: Mutex::new(HashMap::new()),
            logs: Mutex::new(VecDeque::with_capacity(MAX_LOGS)),
            bus,
        }
    }

    // ─── Tenant CRUD ─────────────────────────────────────────────────────────

    pub fn upsert_tenant(&self, record: TenantRecord) {
        self.tenants
            .lock()
            .unwrap()
            .insert(record.tenant_id.clone(), record);
    }

    /// Returns an owned clone. Callers that want to persist a mutation must
    /// call `upsert_tenant` — there is no way to get a live handle into the
    /// map, which is what makes the Go version's chaos-engine race
    /// structurally impossible here rather than merely fixed.
    pub fn get_tenant(&self, id: &str) -> Option<TenantRecord> {
        self.tenants.lock().unwrap().get(id).cloned()
    }

    pub fn delete_tenant(&self, id: &str) {
        self.tenants.lock().unwrap().remove(id);
    }

    pub fn list_tenants(&self) -> Vec<TenantRecord> {
        self.tenants.lock().unwrap().values().cloned().collect()
    }

    // ─── Event bus ───────────────────────────────────────────────────────────

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.bus.subscribe()
    }

    /// Non-blocking by construction: `broadcast::Sender::send` never blocks
    /// on slow subscribers (it just drops the oldest buffered event for a lagging
    /// receiver, who then gets `RecvError::Lagged` on their next `recv()`).
    pub fn publish(&self, event: Event) {
        // No receivers is not an error worth propagating — the bus is
        // best-effort, mirroring the Go version's `select { default: }`.
        let _ = self.bus.send(event);
    }

    // ─── Log ring buffer ─────────────────────────────────────────────────────

    pub fn append_log(&self, line: impl Into<String>) {
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= MAX_LOGS {
            logs.pop_front();
        }
        logs.push_back(line.into());
    }

    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().iter().cloned().collect()
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn upsert_and_get_tenant() {
        let store = Store::new();
        assert!(store.get_tenant("acme").is_none());

        store.upsert_tenant(TenantRecord {
            tenant_id: "acme".into(),
            phase: TenantPhase::Provisioning,
            ..Default::default()
        });
        let rec = store.get_tenant("acme").unwrap();
        assert_eq!(rec.phase, TenantPhase::Provisioning);

        // Re-upsert must overwrite, not duplicate.
        store.upsert_tenant(TenantRecord {
            tenant_id: "acme".into(),
            phase: TenantPhase::Ready,
            ..Default::default()
        });
        assert_eq!(store.get_tenant("acme").unwrap().phase, TenantPhase::Ready);
        assert_eq!(store.list_tenants().len(), 1);
    }

    #[test]
    fn delete_tenant_is_idempotent() {
        let store = Store::new();
        store.upsert_tenant(TenantRecord {
            tenant_id: "acme".into(),
            ..Default::default()
        });
        store.delete_tenant("acme");
        assert!(store.get_tenant("acme").is_none());
        store.delete_tenant("does-not-exist"); // must not panic
    }

    #[test]
    fn get_tenant_returns_independent_copy() {
        // Regression test for the exact bug class the Go version had: a
        // caller mutating a "get" result must never affect the stored value.
        let store = Store::new();
        store.upsert_tenant(TenantRecord {
            tenant_id: "acme".into(),
            phase: TenantPhase::Degraded,
            ..Default::default()
        });

        let mut rec = store.get_tenant("acme").unwrap();
        rec.phase = TenantPhase::Ready; // mutate the caller's copy only

        let fresh = store.get_tenant("acme").unwrap();
        assert_eq!(
            fresh.phase,
            TenantPhase::Degraded,
            "mutating a get_tenant() result must not leak into the store"
        );
    }

    #[tokio::test]
    async fn publish_subscribe() {
        let store = Store::new();
        let mut rx = store.subscribe();
        store.publish(Event::new(EventKind::TenantCreated, "acme", "created"));

        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("recv error");
        assert_eq!(ev.kind, EventKind::TenantCreated);
        assert_eq!(ev.tenant_id, "acme");
    }

    #[tokio::test]
    async fn publish_fans_out_to_multiple_subscribers() {
        let store = Store::new();
        let mut a = store.subscribe();
        let mut b = store.subscribe();
        store.publish(Event::new(EventKind::ProxyUp, "acme", "up"));

        for rx in [&mut a, &mut b] {
            let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("timed out")
                .expect("recv error");
            assert_eq!(ev.kind, EventKind::ProxyUp);
        }
    }

    #[test]
    fn publish_does_not_block_on_undrained_subscriber() {
        let store = Arc::new(Store::new());
        let _slow = store.subscribe(); // never drained

        // Publish far more than the channel capacity; must never block.
        for i in 0..(EVENT_BUS_CAPACITY * 4) {
            store.publish(Event::new(EventKind::ReconcileOk, format!("t{i}"), "ok"));
        }
    }

    #[test]
    fn log_ring_buffer_caps_and_evicts_oldest() {
        let store = Store::new();
        for i in 0..(MAX_LOGS + 50) {
            store.append_log(format!("line-{i}"));
        }
        let logs = store.logs();
        assert_eq!(logs.len(), MAX_LOGS);
        assert_eq!(logs.last().unwrap(), &format!("line-{}", MAX_LOGS + 49));
        assert_eq!(logs.first().unwrap(), &"line-50".to_string());
    }

    #[test]
    fn logs_returns_independent_copy() {
        let store = Store::new();
        store.append_log("one");
        let mut logs = store.logs();
        logs[0] = "mutated".into();
        assert_eq!(store.logs()[0], "one");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_access_is_race_free() {
        let store = Arc::new(Store::new());
        let mut handles = Vec::new();

        for w in 0..8 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                let tenant_id = format!("tenant-{w}");
                for i in 0..200 {
                    store.upsert_tenant(TenantRecord {
                        tenant_id: tenant_id.clone(),
                        reconcile_count: i,
                        ..Default::default()
                    });
                    store.append_log(format!("[{tenant_id}] reconcile {i}"));
                    store.publish(Event::new(EventKind::ReconcileOk, tenant_id.clone(), "ok"));
                    let _ = store.list_tenants();
                    let _ = store.logs();
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(store.list_tenants().len(), 8);
    }
}
