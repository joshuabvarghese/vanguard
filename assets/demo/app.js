const TENANT_LIST = document.getElementById("tenant-list");
const TENANT_COUNT = document.getElementById("tenant-count");
const LOG_STREAM = document.getElementById("log-stream");
const CONN_DOT = document.getElementById("conn-dot");
const CONN_LABEL = document.getElementById("conn-label");
const CLOCK = document.getElementById("clock");
const FORM = document.getElementById("create-form");

const TIER_PRESETS = {
  free: { requestsPerSecond: 20, burstCapacity: 40, maxConcurrent: 10 },
  pro: { requestsPerSecond: 200, burstCapacity: 400, maxConcurrent: 50 },
  enterprise: { requestsPerSecond: 5000, burstCapacity: 10000, maxConcurrent: 500 },
};

let knownLogCount = 0;
let pollFailures = 0;

function tick() {
  const now = new Date();
  CLOCK.textContent = now.toTimeString().slice(0, 8);
}
setInterval(tick, 1000);
tick();

function setConn(ok) {
  CONN_DOT.classList.toggle("dot-live", ok);
  CONN_DOT.classList.toggle("dot-down", !ok);
  CONN_LABEL.textContent = ok ? "live" : "reconnecting…";
}

function classifyLog(line) {
  if (line.includes("✗") || line.includes("degraded")) return "err";
  if (line.includes("💥") || line.includes("⚠") || line.includes("CHAOS")) return "warn";
  if (line.includes("[boot]")) return "boot";
  if (line.includes("✓") || line.includes("⚡")) return "ok";
  return "";
}

async function fetchJSON(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error(body.error || `${res.status} ${res.statusText}`);
  }
  return res.json();
}

async function refreshTenants() {
  const tenants = await fetchJSON("/api/v1/tenants");
  TENANT_COUNT.textContent = tenants.length;

  if (tenants.length === 0) {
    TENANT_LIST.innerHTML = `<div class="empty-hint">No tenants yet — provision one above.</div>`;
    return;
  }

  tenants.sort((a, b) => a.tenantId.localeCompare(b.tenantId));

  TENANT_LIST.innerHTML = tenants
    .map((t) => {
      const phase = t.phase || "Provisioning";
      return `
        <div class="tenant-card phase-${phase.toLowerCase()}" data-id="${escapeAttr(t.tenantId)}">
          <div class="tenant-row-1">
            <div>
              <span class="tenant-name">${escapeHtml(t.displayName || t.tenantId)}</span>
              <span class="tenant-id">${escapeHtml(t.tenantId)}</span>
            </div>
            <span class="phase-badge phase-${phase}">${phase}</span>
          </div>
          <div class="tenant-meta">
            <div>tier <b>${escapeHtml(t.tier || "—")}</b></div>
            <div>rps <b>${t.requestsPerSecond ?? "—"}</b></div>
            <div>namespace <b>${escapeHtml(t.namespace || "—")}</b></div>
            <div>reconciles <b>${t.reconcileCount ?? 0}</b></div>
          </div>
          <div class="tenant-actions">
            <button class="btn btn-danger btn-sm" data-action="chaos" data-id="${escapeAttr(t.tenantId)}" ${phase !== "Ready" ? "disabled" : ""}>Kill proxy</button>
            <button class="btn btn-sm" data-action="delete" data-id="${escapeAttr(t.tenantId)}">Delete</button>
          </div>
        </div>`;
    })
    .join("");
}

async function refreshLogs() {
  const { logs } = await fetchJSON("/api/v1/logs");
  if (logs.length === knownLogCount) return;

  const wasAtBottom =
    LOG_STREAM.scrollHeight - LOG_STREAM.scrollTop - LOG_STREAM.clientHeight < 40;

  const newLines = logs.slice(knownLogCount);
  knownLogCount = logs.length;

  const frag = document.createDocumentFragment();
  for (const line of newLines) {
    const div = document.createElement("div");
    div.className = `log-line ${classifyLog(line)}`;
    div.textContent = line;
    frag.appendChild(div);
  }
  LOG_STREAM.appendChild(frag);

  while (LOG_STREAM.children.length > 400) {
    LOG_STREAM.removeChild(LOG_STREAM.firstChild);
  }

  if (wasAtBottom) {
    LOG_STREAM.scrollTop = LOG_STREAM.scrollHeight;
  }
}

async function poll() {
  try {
    await Promise.all([refreshTenants(), refreshLogs()]);
    setConn(true);
    pollFailures = 0;
  } catch (e) {
    pollFailures += 1;
    if (pollFailures > 2) setConn(false);
  }
}

FORM.addEventListener("submit", async (e) => {
  e.preventDefault();
  const tenantId = document.getElementById("f-tenant-id").value.trim();
  const displayName = document.getElementById("f-display-name").value.trim();
  const tier = document.getElementById("f-tier").value;
  const preset = TIER_PRESETS[tier];

  const submitBtn = FORM.querySelector("button[type=submit]");
  submitBtn.disabled = true;
  try {
    await fetchJSON("/api/v1/tenants", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        tenantId,
        displayName,
        rateLimit: { tier, ...preset },
      }),
    });
    FORM.reset();
    document.getElementById("f-tier").value = "pro";
    await refreshTenants();
  } catch (err) {
    alert(`Couldn't create tenant: ${err.message}`);
  } finally {
    submitBtn.disabled = false;
  }
});

TENANT_LIST.addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-action]");
  if (!btn) return;
  const { action, id } = btn.dataset;
  btn.disabled = true;

  try {
    if (action === "chaos") {
      await fetchJSON(`/api/v1/tenants/${encodeURIComponent(id)}/chaos/kill-proxy`, {
        method: "POST",
      });
    } else if (action === "delete") {
      await fetchJSON(`/api/v1/tenants/${encodeURIComponent(id)}`, { method: "DELETE" });
    }
    await refreshTenants();
  } catch (err) {
    alert(`Action failed: ${err.message}`);
    btn.disabled = false;
  }
});

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}
function escapeAttr(s) {
  return escapeHtml(s);
}

poll();
setInterval(poll, 1000);
