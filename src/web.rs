//! # Module: web
//!
//! ## Responsibility
//! Axum HTTP server: dark dashboard UI, SSE live feed, JSON API, Prometheus endpoint.
//!
//! ## Guarantees
//! - All handlers return within bounded time (no infinite waits).
//! - SSE feed drops old events if the subscriber falls behind (broadcast::Receiver::try_recv).
//!
//! ## NOT Responsible For
//! - Routing logic (see: router.rs)
//! - Config management (see: config.rs)

use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use axum::{
    extract::{Extension, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::sse::Event,
    response::{Html, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router as AxumRouter,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::config::{RouterConfig, RouterConfigPatch};
use crate::downstream_pressure::{DownstreamPressureMonitor, DownstreamTelemetry};
use crate::explainer::StrategyExplainer;
use crate::metrics::{prometheus_text_with_neural, NeuralMetrics};
use crate::router::{KindRoutingStats, Router, SLAViolationCounts};
use crate::tracing_span::{CompletedSpan, TraceContext};
use crate::types::Strategy;

type AppState = Arc<Router>;
type DownstreamState = Arc<DownstreamPressureMonitor>;
/// Shared explainer state threaded through Axum extensions.
pub type ExplainerState = Arc<Mutex<StrategyExplainer>>;
/// Shared trace store state threaded through Axum extensions.
pub type TraceState = Arc<TraceContext>;

/// Start the Axum HTTP server and bind it to `addr`.
///
/// Registers all routes (dashboard, health, metrics, stats, config, SSE,
/// neural snapshot, EOT telemetry, downstream pressure telemetry) and serves
/// them until the process exits or the underlying TCP listener is closed.
///
/// # Parameters
///
/// * `router`               — Shared router instance; wrapped in `Arc` for handler access.
/// * `addr`                 — TCP address to bind (e.g. `127.0.0.1:8080`).
/// * `downstream_monitor`   — Optional downstream pressure monitor.  When `None`,
///   the `POST /api/downstream/telemetry` endpoint still registers but returns
///   a no-op response.  Pass `Some(Arc::new(DownstreamPressureMonitor::new()))`
///   to enable real monitoring.
///
/// # Errors
///
/// Returns `std::io::Error` if binding the TCP listener fails or if Axum's
/// internal `serve` loop encounters a fatal I/O error.
pub async fn serve(
    router: Router,
    addr: SocketAddr,
) -> std::io::Result<()> {
    serve_with_downstream(router, addr, Arc::new(DownstreamPressureMonitor::new())).await
}

/// Like [`serve`] but accepts a pre-constructed [`DownstreamPressureMonitor`]
/// (useful when callers want to share the monitor instance).
pub async fn serve_with_downstream(
    router: Router,
    addr: SocketAddr,
    downstream: DownstreamState,
) -> std::io::Result<()> {
    let explainer: ExplainerState = Arc::new(Mutex::new(StrategyExplainer::default()));
    let trace_ctx: TraceState = Arc::new(TraceContext::new(1000));
    serve_with_all_traced(router, addr, downstream, explainer, trace_ctx).await
}

/// Full constructor: accepts a pre-constructed [`StrategyExplainer`] in addition to
/// a [`DownstreamPressureMonitor`]. Use this variant when you want to share the
/// explainer instance with other parts of the application (e.g., to record decisions
/// from the router loop before serving them at `/explain/latest`).
pub async fn serve_with_all(
    router: Router,
    addr: SocketAddr,
    downstream: DownstreamState,
    explainer: ExplainerState,
) -> std::io::Result<()> {
    let trace_ctx: TraceState = Arc::new(TraceContext::new(1000));
    serve_with_all_traced(router, addr, downstream, explainer, trace_ctx).await
}

/// Full constructor with an explicit [`TraceContext`].
///
/// Accepts all shared state; trace API endpoints use `trace_ctx`.
pub async fn serve_with_all_traced(
    router: Router,
    addr: SocketAddr,
    downstream: DownstreamState,
    explainer: ExplainerState,
    trace_ctx: TraceState,
) -> std::io::Result<()> {
    let shared: AppState = Arc::new(router);

    let app = AxumRouter::new()
        .route("/", get(ui))
        .route("/health", get(health))
        .route("/metrics", get(metrics_prom))
        .route("/api/stats", get(stats_json))
        .route(
            "/api/config",
            get(get_config).post(set_config).patch(patch_config),
        )
        .route("/api/telemetry", post(post_eot_telemetry))
        .route("/api/downstream/telemetry", post(post_downstream_telemetry))
        .route("/api/downstream/pressure", get(get_downstream_pressure))
        .route("/api/stream/decisions", get(sse_decisions))
        .route("/api/neural", get(get_neural))
        .route("/api/dag", get(get_dag_graph))
        .route("/api/autoscaler/forecast", get(get_autoscaler_forecast))
        .route("/explain/latest", get(get_explain_latest))
        .route("/api/traces/recent", get(get_traces_recent))
        .route("/api/traces/:trace_id", get(get_traces_by_id))
        .route("/api/dedup/stats", get(get_dedup_stats))
        .route("/api/sla/stats", get(get_sla_stats))
        .route("/api/result-cache/stats", get(get_result_cache_stats))
        .route("/api/flow/stats", get(get_flow_stats))
        .route("/api/timeouts/stats", get(get_timeout_stats));
    #[cfg(feature = "simulation")]
    let app = app.route("/api/simulate", post(post_simulate));
    let app = app
        .with_state(shared)
        .layer(axum::Extension(downstream))
        .layer(axum::Extension(explainer))
        .layer(axum::Extension(trace_ctx));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .await
        .map_err(std::io::Error::other)
}

// ===== UI =====

/// Embedded HTML source for the live-routing dashboard served at `GET /`.
///
/// Single-file, no external dependencies, dark and light themes (follows the
/// system setting). Vanilla JS: an SSE client on `/api/stream/decisions`, a
/// 1 s poll of `/api/stats`, and a "Run 200 jobs" button that calls
/// `POST /api/simulate`.
pub const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <meta name="color-scheme" content="dark light"/>
  <title>HelixRouter dashboard</title>
  <style>
    :root{
      --bg:#0b0f14;--card:#0f1720;--line:#1f2a37;--ink:#e6edf3;--muted:#9aa4b2;--sunk:#060a0f;--accent:#7dd3fc;
      --inline:#34d399;--spawn:#60a5fa;--cpu_pool:#a78bfa;--batch:#fbbf24;--drop:#f87171;--ok:#34d399;--warn:#fbbf24;--bad:#f87171;
      --btn:#7dd3fc;--btn-ink:#06121c;
    }
    @media (prefers-color-scheme: light){
      :root{
        --bg:#f6f7f9;--card:#ffffff;--line:#dfe3e8;--ink:#101419;--muted:#5b6573;--sunk:#f0f2f5;--accent:#0369a1;
        --inline:#059669;--spawn:#2563eb;--cpu_pool:#7c3aed;--batch:#b45309;--drop:#dc2626;--ok:#059669;--warn:#b45309;--bad:#dc2626;
        --btn:#0369a1;--btn-ink:#ffffff;
      }
    }
    *{box-sizing:border-box;margin:0;padding:0}
    body{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:var(--bg);color:var(--ink);padding:16px;max-width:1280px;margin:0 auto}
    header{display:flex;flex-wrap:wrap;align-items:center;gap:10px 14px;margin-bottom:6px}
    h1{font-size:1.2rem;color:var(--accent)}
    h1 span{color:var(--muted);font-weight:400}
    .pill{display:inline-flex;align-items:center;gap:6px;font-size:.72rem;color:var(--muted);border:1px solid var(--line);border-radius:999px;padding:3px 9px}
    .pill i{width:8px;height:8px;border-radius:50%;background:var(--muted)}
    .pill.live i{background:var(--ok);box-shadow:0 0 0 3px color-mix(in srgb,var(--ok) 25%,transparent)}
    .pill.down i{background:var(--bad)}
    .spacer{flex:1}
    button.run{font:600 .8rem ui-monospace,monospace;background:var(--btn);color:var(--btn-ink);border:0;border-radius:8px;padding:8px 14px;cursor:pointer}
    button.run.alt{background:transparent;color:var(--bad);border:1px solid var(--bad)}
    button.run:disabled{opacity:.6;cursor:progress}
    button.run:focus-visible{outline:2px solid var(--ink);outline-offset:2px}
    .intro{color:var(--muted);font-size:.8rem;line-height:1.5;margin-bottom:14px;max-width:900px}
    .intro b{font-weight:600}
    .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(230px,1fr));gap:12px;margin-bottom:12px}
    .card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px;min-width:0}
    .card h3{font-size:.75rem;color:var(--muted);text-transform:uppercase;letter-spacing:.06em;margin-bottom:8px;font-weight:600}
    .big{font-size:2rem;font-weight:700;color:var(--ink)}
    .muted{color:var(--muted);font-size:.8rem}
    .hint{color:var(--muted);font-size:.72rem;margin-top:6px;line-height:1.4}
    .scroll{overflow-x:auto}
    table{width:100%;border-collapse:collapse;font-size:.8rem}
    th{color:var(--muted);text-align:left;padding:4px 6px;border-bottom:1px solid var(--line);font-weight:600;white-space:nowrap}
    td{padding:4px 6px;border-bottom:1px solid var(--line);white-space:nowrap}
    td.empty{color:var(--muted);white-space:normal;padding:10px 6px}
    .bar-wrap{background:var(--line);border-radius:4px;height:8px;overflow:hidden;margin-top:2px;min-width:60px}
    .bar{height:100%;border-radius:4px;transition:width .4s}
    .strategy-inline{background:var(--inline)}
    .strategy-spawn{background:var(--spawn)}
    .strategy-cpu_pool{background:var(--cpu_pool)}
    .strategy-batch{background:var(--batch)}
    .strategy-drop{background:var(--drop)}
    #decisions{height:220px;overflow-y:auto;font-size:.75rem;background:var(--sunk);border-radius:6px;padding:8px}
    #decisions .empty{color:var(--muted);padding:6px 2px}
    .dec{padding:2px 0;border-bottom:1px solid var(--line);display:flex;gap:10px;white-space:nowrap}
    .dec .strat{min-width:72px;font-weight:600}
    .gauge-wrap{position:relative;height:64px;display:flex;align-items:flex-end;justify-content:center}
    .gauge-label{font-size:1.6rem;font-weight:700;color:var(--ink)}
    .gauge-bar{height:8px;background:var(--line);border-radius:4px;overflow:hidden;margin-top:8px}
    .gauge-fill{height:100%;border-radius:4px;transition:width .5s,background .5s}
    .donut-wrap{display:flex;align-items:center;gap:16px}
    #donut{width:90px;height:90px;flex:none;transform:rotate(-90deg)}
    .legend{flex:1;display:flex;flex-direction:column;gap:4px;font-size:.75rem}
    .legend-item{display:flex;align-items:center;gap:6px}
    .legend-item .n{margin-left:auto;color:var(--muted)}
    .legend-dot{width:10px;height:10px;border-radius:50%;flex-shrink:0}
    .toast{font-size:.75rem;color:var(--muted)}
    a{color:var(--accent);text-decoration:none}
    a:hover{text-decoration:underline}
    footer{margin-top:12px;font-size:.72rem;color:var(--muted)}
  </style>
</head>
<body>
<header>
  <h1>HelixRouter <span>live dashboard</span></h1>
  <span class="pill" id="conn"><i></i><span id="conn-text">connecting...</span></span>
  <span class="spacer"></span>
  <span class="toast" id="run-msg" aria-live="polite"></span>
  <button class="run" id="run" type="button" data-jobs="200" data-rate="40" title="Submit 200 simulated jobs over 5 seconds">Run 200 jobs</button>
  <button class="run alt" id="overload" type="button" data-jobs="2000" data-rate="100000" title="Submit 2000 simulated jobs at once">Overload</button>
</header>
<p class="intro">Every job gets one of five strategies, picked from its compute cost, how well it parallelizes and how busy the CPU pool is:
  <b style="color:var(--inline)">inline</b> (run now), <b style="color:var(--spawn)">spawn</b> (own task),
  <b style="color:var(--cpu_pool)">cpu_pool</b> (bounded worker pool), <b style="color:var(--batch)">batch</b> (grouped), or
  <b style="color:var(--drop)">drop</b> (shed under overload). Press <b>Run 200 jobs</b> to send a steady burst, or <b>Overload</b> to send 2000 at once and watch it batch and shed.</p>

<div class="grid">
  <div class="card">
    <h3>Jobs completed</h3>
    <div class="big" id="completed">...</div>
    <div class="muted"><span id="dropped">...</span> dropped</div>
  </div>
  <div class="card">
    <h3>System pressure</h3>
    <div class="gauge-wrap"><span class="gauge-label" id="pressure-val">...</span></div>
    <div class="gauge-bar"><div class="gauge-fill" id="pressure-fill" style="width:0%"></div></div>
    <div class="hint">0% is idle. When the CPU pool is saturated, new jobs are batched or dropped.</div>
  </div>
  <div class="card">
    <h3>Adaptive threshold</h3>
    <div class="big" id="threshold">...</div>
    <div class="hint">spawn_threshold: jobs cheaper than this run as their own task; costlier ones go to the CPU pool or a batch. It adapts to load.</div>
  </div>
  <div class="card">
    <h3>Strategy mix</h3>
    <div class="donut-wrap">
      <svg id="donut" viewBox="0 0 36 36" role="img" aria-label="Share of jobs per strategy"></svg>
      <div class="legend" id="legend"><span class="muted">No jobs yet</span></div>
    </div>
  </div>
</div>

<div class="card" style="margin-bottom:12px">
  <h3>Latency by strategy</h3>
  <div class="scroll">
  <table>
    <thead><tr><th>Strategy</th><th>Count</th><th>Avg ms</th><th>EMA ms</th><th>p95 ms</th><th style="width:30%">p95</th></tr></thead>
    <tbody id="lat-body"><tr><td class="empty" colspan="6">Loading...</td></tr></tbody>
  </table>
  </div>
</div>

<div class="grid">
  <div class="card">
    <h3>Per job kind</h3>
    <div class="scroll">
    <table>
      <thead><tr><th>Kind</th><th>Routed</th><th>Misses</th><th>Rate</th></tr></thead>
      <tbody id="kind-body"><tr><td class="empty" colspan="4">Loading...</td></tr></tbody>
    </table>
    </div>
  </div>
  <div class="card">
    <h3>Deadlines and SLA</h3>
    <div style="font-size:.85rem;line-height:1.9">
      <div>Deadline exceeded: <b id="deadline-exceeded" style="color:var(--bad)">...</b></div>
      <div>SLA violations: <b id="sla-total" style="color:var(--warn)">...</b></div>
    </div>
    <div class="hint">An SLA violation is a job that finished later than its latency budget.</div>
  </div>
  <div class="card">
    <h3>Neural router exploration (<span style="text-transform:none">&epsilon;</span>)</h3>
    <canvas id="eps-canvas" style="width:100%;height:80px;display:block"></canvas>
    <div class="hint" id="eps-hint">How often the learned router tries a non-best strategy. It decays as it learns.</div>
  </div>
</div>

<div class="card">
  <h3>Live routing decisions <span class="muted" style="font-size:.7rem;text-transform:none;letter-spacing:0">(newest first, last 50)</span></h3>
  <div id="decisions"><div class="empty" id="dec-empty">Waiting for new routing decisions. Press Run 200 jobs, or submit jobs to the router from your code.</div></div>
</div>

<footer>JSON: <a href="/api/stats">/api/stats</a> &middot; Prometheus: <a href="/metrics">/metrics</a> &middot; Stream: <a href="/api/stream/decisions">/api/stream/decisions</a> &middot; <a href="https://gitlab.com/mattbusel/HelixRouter-adaptive-async-compute-router">GitHub</a></footer>

<script>
const css = n => getComputedStyle(document.documentElement).getPropertyValue(n).trim();
const STRATS = ['inline','spawn','cpu_pool','batch','drop'];
const COLORS = {};
function loadColors(){ STRATS.forEach(s => COLORS[s] = css('--'+s)); }
loadColors();
matchMedia('(prefers-color-scheme: light)').addEventListener('change', () => { loadColors(); tick(); });
const esc = s => String(s).replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));

/* ---- connection status ---- */
const conn = document.getElementById('conn'), connText = document.getElementById('conn-text');
function setConn(ok, text){ conn.className = 'pill ' + (ok ? 'live' : 'down'); connText.textContent = text; }

/* ---- SSE live decisions ---- */
const decBox = document.getElementById('decisions');
const maxDec = 50;
const es = new EventSource('/api/stream/decisions');
es.onopen = () => setConn(true, 'live');
es.onerror = () => setConn(false, 'reconnecting...');
es.onmessage = ev => {
  try {
    const d = JSON.parse(ev.data);
    const empty = document.getElementById('dec-empty');
    if (empty) empty.remove();
    const div = document.createElement('div');
    div.className = 'dec';
    const col = COLORS[d.strategy] || css('--ink');
    div.innerHTML = `<span class="strat" style="color:${col}">${esc(d.strategy)}</span>` +
      `<span class="muted">job ${esc(d.job_id)}</span>` +
      `<span>cost ${esc(d.compute_cost)}</span>` +
      `<span class="muted">cpu_busy ${esc(d.cpu_busy)}</span>` +
      `<span class="muted">pressure ${(d.pressure*100).toFixed(0)}%</span>`;
    decBox.prepend(div);
    while (decBox.children.length > maxDec) decBox.removeChild(decBox.lastChild);
  } catch(_){}
};

/* ---- Run a simulated burst ---- */
const runMsg = document.getElementById('run-msg');
const buttons = [document.getElementById('run'), document.getElementById('overload')];
buttons.forEach(btn => btn.addEventListener('click', async () => {
  buttons.forEach(x => x.disabled = true);
  const label = btn.textContent;
  try {
    const r = await fetch(`/api/simulate?jobs=${btn.dataset.jobs}&rate=${btn.dataset.rate}`, {method:'POST'});
    if (!r.ok) throw new Error(r.status === 404 ? 'this build has no simulator' : 'HTTP ' + r.status);
    const j = await r.json();
    runMsg.textContent = `sending ${j.started} jobs over ${Math.max(1, Math.round(j.seconds))} s...`;
    btn.textContent = 'Running...';
    setTimeout(() => { buttons.forEach(x => x.disabled = false); btn.textContent = label; runMsg.textContent = ''; }, j.seconds * 1000 + 500);
  } catch (e) {
    runMsg.textContent = 'Could not start: ' + e.message;
    buttons.forEach(x => x.disabled = false);
  }
}));

/* ---- Stats polling ---- */
async function tick() {
  let j;
  try {
    const r = await fetch('/api/stats', {cache:'no-store'});
    j = await r.json();
  } catch (e) { setConn(false, 'cannot reach the router'); return; }

  document.getElementById('completed').textContent = (j.completed ?? 0).toLocaleString();
  document.getElementById('dropped').textContent = (j.dropped ?? 0).toLocaleString();
  document.getElementById('threshold').textContent = (j.adaptive_spawn_threshold ?? 0).toLocaleString();

  const p = j.pressure_score ?? 0;
  document.getElementById('pressure-val').textContent = (p*100).toFixed(0)+'%';
  const fill = document.getElementById('pressure-fill');
  fill.style.width = (p*100).toFixed(1)+'%';
  fill.style.background = p > 0.7 ? css('--bad') : p > 0.4 ? css('--warn') : css('--ok');

  const rows = j.latency_by_strategy ?? [];
  const tbody = document.getElementById('lat-body');
  if (!rows.length) {
    tbody.innerHTML = '<tr><td class="empty" colspan="6">No jobs routed yet.</td></tr>';
  } else {
    const maxP95 = Math.max(1, ...rows.map(r => r.p95_ms || 0));
    tbody.innerHTML = rows.map(r => {
      const col = COLORS[r.strategy] || css('--ink');
      return `<tr><td style="color:${col};font-weight:600">${esc(r.strategy)}</td><td>${r.count}</td>` +
        `<td>${(r.avg_ms||0).toFixed(2)}</td><td>${(r.ema_ms||0).toFixed(2)}</td><td>${r.p95_ms||0}</td>` +
        `<td><div class="bar-wrap"><div class="bar strategy-${esc(r.strategy)}" style="width:${((r.p95_ms||0)/maxP95*100).toFixed(1)}%"></div></div></td></tr>`;
    }).join('');
  }

  const routed = j.routed_by_strategy ?? [];
  const total = routed.reduce((s, r) => s + r.count, 0);
  drawDonut(routed, total);

  const kr = j.kind_routing ?? {};
  const sv = j.sla_violations ?? {};
  const kinds = [
    ['hash_mix', kr.hash_mix ?? 0, sv.hash_mix ?? 0],
    ['prime_count', kr.prime_count ?? 0, sv.prime_count ?? 0],
    ['monte_carlo_risk', kr.monte_carlo_risk ?? 0, sv.monte_carlo_risk ?? 0],
  ];
  document.getElementById('kind-body').innerHTML = kinds.map(([label, n, v]) => {
    const rate = n > 0 ? ((v / n) * 100).toFixed(1) + '%' : '-';
    return `<tr><td>${label}</td><td>${n}</td><td style="color:${v > 0 ? css('--bad') : css('--ok')}">${v}</td><td>${rate}</td></tr>`;
  }).join('');

  document.getElementById('deadline-exceeded').textContent = j.deadline_exceeded ?? 0;
  document.getElementById('sla-total').textContent = (sv.hash_mix ?? 0) + (sv.prime_count ?? 0) + (sv.monte_carlo_risk ?? 0);

  drawEpsilonCurve(j.epsilon_history ?? []);
  if (!conn.classList.contains('live') && es.readyState === 1) setConn(true, 'live');
}

function drawEpsilonCurve(history) {
  const canvas = document.getElementById('eps-canvas');
  const dpr = window.devicePixelRatio || 1;
  const W = Math.round(canvas.clientWidth * dpr), H = Math.round(canvas.clientHeight * dpr);
  if (canvas.width !== W || canvas.height !== H) { canvas.width = W; canvas.height = H; }
  const ctx = canvas.getContext('2d');
  ctx.clearRect(0, 0, W, H);
  ctx.font = (11 * dpr) + 'px ui-monospace,monospace';
  ctx.fillStyle = css('--muted');
  if (history.length < 2) { ctx.fillText('collecting samples...', 4 * dpr, 14 * dpr); return; }
  const max = Math.max(...history, 0.01);
  ctx.strokeStyle = css('--spawn');
  ctx.lineWidth = 1.5 * dpr;
  ctx.beginPath();
  history.forEach((v, i) => {
    const x = (i / (history.length - 1)) * W;
    const y = H - (v / max) * (H - 20 * dpr) - 2 * dpr;
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  });
  ctx.stroke();
  ctx.fillText('ε = ' + history[history.length - 1].toFixed(4), 4 * dpr, 12 * dpr);
}

function drawDonut(routed, total) {
  const svg = document.getElementById('donut');
  const legend = document.getElementById('legend');
  const r = 15.9, circ = 2 * Math.PI * r;
  let offset = 0, circles = `<circle cx="18" cy="18" r="${r}" fill="none" stroke="${css('--line')}" stroke-width="5"/>`;
  const byName = Object.fromEntries(routed.map(x => [x.strategy, x.count]));
  if (!total) { svg.innerHTML = circles; legend.innerHTML = '<span class="muted">No jobs yet</span>'; return; }
  let items = '';
  STRATS.forEach(s => {
    const count = byName[s] || 0;
    const len = count / total * circ;
    if (len > 0) circles += `<circle cx="18" cy="18" r="${r}" fill="none" stroke="${COLORS[s]}" stroke-width="5" stroke-dasharray="${len.toFixed(2)} ${(circ-len).toFixed(2)}" stroke-dashoffset="${(-offset).toFixed(2)}"/>`;
    offset += len;
    items += `<div class="legend-item"><span class="legend-dot" style="background:${COLORS[s]}"></span><span style="color:${COLORS[s]}">${s}</span><span class="n">${count} (${(count/total*100).toFixed(0)}%)</span></div>`;
  });
  svg.innerHTML = circles;
  legend.innerHTML = items;
}

tick();
setInterval(tick, 1000);
</script>
</body>
</html>"##;

// ===== Health check =====

/// `GET /health` — lightweight liveness probe.
///
/// Returns `{"status":"ok","uptime_secs":<n>}`.
/// Bridges (HelixPressureProbe, HelixBridge) call this to verify connectivity
/// before starting their polling loops.
#[derive(Debug, Clone, Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
}

async fn health(State(router): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        uptime_secs: router.uptime_secs(),
    })
}

async fn ui() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// ===== Simulated burst (dashboard "Run 200 jobs" button) =====

/// Query parameters for `POST /api/simulate`.
#[cfg(feature = "simulation")]
#[derive(Debug, Deserialize)]
struct SimulateParams {
    /// Number of jobs to submit (default 200, clamped to 1..=5000).
    jobs: Option<u64>,
    /// Submission rate in jobs per second (default 40, clamped to 1..=100000).
    rate: Option<u64>,
}

/// Response body of `POST /api/simulate`.
#[cfg(feature = "simulation")]
#[derive(Debug, Serialize)]
struct SimulateResponse {
    started: u64,
    rate: u64,
    seconds: f64,
}

/// Clamp the simulate parameters to safe bounds: (jobs, rate).
#[cfg(feature = "simulation")]
fn simulate_bounds(p: &SimulateParams) -> (u64, u64) {
    (
        p.jobs.unwrap_or(200).clamp(1, 5_000),
        p.rate.unwrap_or(40).clamp(1, 100_000),
    )
}

/// `POST /api/simulate?jobs=200&rate=40`: submit a burst of synthetic jobs at a
/// steady rate in the background so the dashboard has something to show.
///
/// Every call uses a fresh seed and fresh job ids, so repeated bursts are real
/// new work rather than result-cache hits. Returns `202 Accepted` at once.
#[cfg(feature = "simulation")]
async fn post_simulate(
    State(router): State<AppState>,
    Query(params): Query<SimulateParams>,
) -> impl IntoResponse {
    use crate::simulator::{Simulator, SimulatorConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    static BURSTS: AtomicU64 = AtomicU64::new(1);
    let (jobs, rate) = simulate_bounds(&params);
    let burst = BURSTS.fetch_add(1, Ordering::Relaxed);
    let mut sim = Simulator::new(SimulatorConfig {
        seed: 7 + burst * 7_919,
        total_jobs: jobs,
        ..Default::default()
    });
    let interval = std::time::Duration::from_micros(1_000_000 / rate);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        while let Some(mut job) = sim.next_job() {
            tick.tick().await;
            job.id += burst * 1_000_000;
            let r = Arc::clone(&router);
            tokio::spawn(async move { r.submit(job).await });
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(SimulateResponse {
            started: jobs,
            rate,
            seconds: jobs as f64 / rate as f64,
        }),
    )
}

// ===== SSE feed =====

async fn sse_decisions(
    State(router): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = router.subscribe_decisions();
    let stream = BroadcastStream::new(rx)
        .filter_map(|item| {
            match item {
                Ok(decision) => {
                    match serde_json::to_string(&decision) {
                        Ok(data) => Some(Ok(Event::default().data(data))),
                        Err(e) => {
                            tracing::warn!(err = %e, "SSE: failed to serialize RoutingDecision; skipping event");
                            None
                        }
                    }
                }
                // Err arm covers BroadcastStreamRecvError::Lagged: the subscriber
                // fell behind and old events were dropped from the channel buffer.
                // Log the lag count and return None so a slow SSE client
                // cannot stall job submission by filling the broadcast channel.
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "SSE: subscriber lagged, {} decisions dropped from stream", n);
                    None
                }
            }
        });
    Sse::new(stream)
}


// ===== JSON stats =====

#[derive(Debug, Clone, Serialize)]
struct CountRow {
    strategy: Strategy,
    count: u64,
}

#[derive(Debug, Clone, Serialize)]
struct LatencyRow {
    strategy: Strategy,
    count: u64,
    avg_ms: f64,
    ema_ms: f64,
    p50_ms: u64,
    p95_ms: u64,
    p99_ms: u64,
    min_ms: u64,
    max_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct StatsResponse {
    completed: u64,
    dropped: u64,
    adaptive_spawn_threshold: u64,
    pressure_score: f64,
    routed_by_strategy: Vec<CountRow>,
    latency_by_strategy: Vec<LatencyRow>,
    /// Per-job-kind routing counts for the dashboard.
    kind_routing: KindRoutingStats,
    /// Per-job-kind SLA violation counts.
    sla_violations: SLAViolationCounts,
    /// Total jobs rejected because their deadline had already passed.
    deadline_exceeded: u64,
    /// Recent epsilon (exploration rate) history — one sample per 100 ms, up to 60 samples.
    epsilon_history: Vec<f64>,
}

async fn stats_json(State(router): State<AppState>) -> impl IntoResponse {
    let snap = router.stats_snapshot().await;

    let mut routed: Vec<CountRow> = snap
        .routed
        .into_iter()
        .map(|(strategy, count)| CountRow { strategy, count })
        .collect();
    routed.sort_by_key(|r| r.strategy.to_string());

    let latency_rows: Vec<LatencyRow> = router
        .latency_report()
        .await
        .into_iter()
        .map(|s| LatencyRow {
            strategy: s.strategy,
            count: s.count,
            avg_ms: s.avg_ms,
            ema_ms: s.ema_ms,
            p50_ms: s.p50_ms,
            p95_ms: s.p95_ms,
            p99_ms: s.p99_ms,
            min_ms: s.min_ms,
            max_ms: s.max_ms,
        })
        .collect();

    Json(StatsResponse {
        completed: snap.completed,
        dropped: snap.dropped,
        adaptive_spawn_threshold: snap.adaptive_spawn_threshold,
        pressure_score: snap.pressure_score,
        routed_by_strategy: routed,
        latency_by_strategy: latency_rows,
        kind_routing: router.kind_routing_stats(),
        sla_violations: router.sla_violation_counts(),
        deadline_exceeded: router.deadline_exceeded_count(),
        epsilon_history: router.epsilon_history().await,
    })
}

// ===== Config API =====

async fn get_config(State(router): State<AppState>) -> impl IntoResponse {
    Json(router.config().await)
}

async fn set_config(State(router): State<AppState>, Json(cfg): Json<RouterConfig>) -> Response {
    // Validate before committing — mirrors the safety check in patch_config.
    // Without this, callers could POST inline_threshold >= spawn_threshold,
    // ema_alpha=0, or cpu_parallelism=0, causing silent misbehaviour or
    // division-by-zero in pressure_score.
    if let Err(e) = cfg.validate() {
        return (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response();
    }
    router.set_config(cfg).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Structured error detail for 422 responses.
#[derive(Debug, Clone, Serialize)]
struct ErrorDetail {
    field: Option<String>,
    code: String,
    message: String,
}

/// Wrapper for a list of validation errors returned by `PATCH /api/config`.
#[derive(Debug, Clone, Serialize)]
struct ErrorResponse {
    errors: Vec<ErrorDetail>,
}

/// PATCH /api/config — apply a partial config update.
///
/// Only fields present in the JSON body are modified; absent fields
/// retain their current values. Returns the merged config on success.
///
/// Returns **422 Unprocessable Entity** with a structured `{"errors": [...]}` body
/// if the resulting config would be invalid (e.g. `inline_threshold >= spawn_threshold`,
/// `ema_alpha` outside `(0, 1]`, `cpu_parallelism == 0`). The live config is
/// **not modified** when validation fails.
///
/// This is the endpoint that EOT's HelixBridge should target, since
/// it sends `RouterConfigPatch` with optional fields rather than a
/// full `RouterConfig`.
async fn patch_config(
    State(router): State<AppState>,
    Json(patch): Json<RouterConfigPatch>,
) -> Response {
    match router.patch_config(patch).await {
        Ok(merged) => Json(merged).into_response(),
        Err(e) => {
            let body = ErrorResponse {
                errors: vec![ErrorDetail {
                    field: None,
                    code: "validation_error".to_string(),
                    message: e.to_string(),
                }],
            };
            (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
        }
    }
}

// ===== EOT telemetry ingest =====

/// Body accepted by `POST /api/telemetry`.
///
/// Every-Other-Token posts this when its self-improvement loop detects
/// elevated pressure so HelixRouter can blend the signal into its own
/// composite pressure score.
#[derive(Debug, Clone, Deserialize)]
struct EotTelemetry {
    /// Normalised pressure signal from EOT's self-tune loop (0.0–1.0).
    /// Values outside [0,1] are clamped before storage.
    pub pressure: f64,
}

/// POST /api/telemetry — accept a pressure signal from Every-Other-Token.
///
/// EOT's HelixBridge calls this endpoint after each self-tune cycle to inject
/// its observed backpressure into HelixRouter's composite pressure score.
/// This closes the feedback loop in the other direction: instead of only
/// HelixRouter pushing config to EOT, EOT can now influence HelixRouter's
/// routing decisions in real time.
///
/// Returns `204 No Content` on success.
async fn post_eot_telemetry(
    State(router): State<AppState>,
    Json(body): Json<EotTelemetry>,
) -> impl IntoResponse {
    router.set_eot_pressure(body.pressure);
    StatusCode::NO_CONTENT
}

// ===== Downstream pressure telemetry =====

/// POST /api/downstream/telemetry — accept pressure telemetry from a downstream service.
///
/// Downstream services post a [`DownstreamTelemetry`] JSON body here to report
/// their current p99 latency, queue depth, and error rate.  HelixRouter aggregates
/// these signals via the [`DownstreamPressureMonitor`] and uses the combined score
/// to preemptively shed lower-priority jobs before its own queues saturate.
///
/// Returns `204 No Content` on success.
async fn post_downstream_telemetry(
    Extension(monitor): Extension<DownstreamState>,
    Json(body): Json<DownstreamTelemetry>,
) -> impl IntoResponse {
    monitor.update(body);
    StatusCode::NO_CONTENT
}

/// GET /api/downstream/pressure — return the current downstream pressure snapshot.
///
/// Returns a JSON object with:
/// - `combined_pressure` — aggregate score in `[0.0, 1.0]`.
/// - `should_shed`       — `true` when pressure exceeds the shedding threshold.
/// - `services`          — per-service breakdown sorted by descending pressure.
async fn get_downstream_pressure(
    Extension(monitor): Extension<DownstreamState>,
) -> impl IntoResponse {
    use crate::downstream_pressure::ServicePressureSnapshot;
    #[derive(serde::Serialize)]
    struct DownstreamPressureResponse {
        combined_pressure: f64,
        should_shed: bool,
        services: Vec<ServicePressureSnapshot>,
    }
    Json(DownstreamPressureResponse {
        combined_pressure: monitor.combined_pressure(),
        should_shed: monitor.should_shed(),
        services: monitor.service_snapshot(),
    })
}

// ===== Neural router snapshot =====

/// GET /api/neural — current state of the online-learning neural router.
///
/// Returns sample count, average reward, warm-up status, and the full weight
/// matrix so operators (and EOT's HelixBridge) can observe how the neural
/// router is weighting each (strategy, feature) pair.
async fn get_neural(State(router): State<AppState>) -> impl IntoResponse {
    Json(router.neural_snapshot().await)
}

// ===== Predictive Autoscaler Forecast =====

/// Response body for `GET /api/autoscaler/forecast`.
#[derive(Serialize)]
struct AutoscalerForecastResponse {
    /// Current CPU pool size reported by the router.
    current_pool_size: usize,
    /// Recommended target CPU pool size, 60 seconds ahead.
    target_pool_size: usize,
    /// Forecast confidence in `[0.0, 1.0]`.
    confidence: f64,
    /// Human-readable explanation of the recommendation.
    reason: String,
    /// Look-ahead window in milliseconds (always 60 000).
    lookahead_ms: u64,
    /// 60-point load forecast sparkline (one value per future second).
    sparkline: Vec<f64>,
    /// Raw forecasted job rate in jobs/second.
    forecast_rate: f64,
    /// Number of samples ingested by the autoscaler so far.
    sample_count: usize,
    /// Whether the Holt-Winters warm-up period has completed.
    is_warmed_up: bool,
    /// Scaling action: `"scale_up"`, `"scale_down"`, or `"hold"`.
    action: String,
}

/// `GET /api/autoscaler/forecast` — Holt-Winters 60-second ahead load forecast.
///
/// Returns a [`AutoscalerForecastResponse`] containing the predictive
/// autoscaler's current recommendation, a sparkline suitable for dashboard
/// widgets, and metadata about the autoscaler's warm-up state.
///
/// This endpoint is served by a per-request [`crate::predictive_autoscaler::PredictiveAutoscaler`]
/// that is seeded with the router's current load statistics.  Until the
/// Holt-Winters warm-up period completes (`is_warmed_up: false`), the
/// recommendation is always `Hold` with `confidence: 0.0`.
async fn get_autoscaler_forecast(State(router): State<AppState>) -> impl IntoResponse {
    use crate::predictive_autoscaler::{
        LoadSample, PredictiveAutoscaler, PredictiveAutoscalerConfig, ScalingAction,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    let stats = router.stats_snapshot().await;
    let cfg = router.config().await;
    let current_pool_size = cfg.cpu_parallelism;

    // Seed the autoscaler with a synthetic history derived from the router's
    // cumulative completed-job counter.  We generate one sample per second for
    // the past 90 seconds using the global completion rate as a proxy.
    let mut scaler = PredictiveAutoscaler::new(PredictiveAutoscalerConfig::default());
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Approximate: assume the current completed count accrued evenly over the
    // past 90 seconds to warm up the Holt-Winters filter.
    let total_completed = stats.completed;
    let window = 90u64;
    for i in 0..window {
        let t = now_secs.saturating_sub(window - i);
        let frac = (i + 1) as f64 / window as f64;
        scaler.observe(LoadSample {
            timestamp_secs: t,
            total_jobs: (total_completed as f64 * frac) as u64,
            pressure_score: stats.pressure_score,
        });
    }

    let rec = scaler.recommend(current_pool_size);
    let action_str = match &rec.action {
        ScalingAction::ScaleUp(n) => format!("scale_up({n})"),
        ScalingAction::ScaleDown(n) => format!("scale_down({n})"),
        ScalingAction::Hold => "hold".to_string(),
    };

    Json(AutoscalerForecastResponse {
        current_pool_size,
        target_pool_size: rec.target_pool_size,
        confidence: rec.confidence,
        reason: rec.reason,
        lookahead_ms: rec.lookahead_ms,
        sparkline: rec.sparkline,
        forecast_rate: rec.forecast_rate,
        sample_count: scaler.sample_count(),
        is_warmed_up: scaler.is_warmed_up(),
        action: action_str,
    })
}

// ===== Strategy Explainer =====

/// Query parameters for `GET /explain/latest`.
#[derive(Debug, Deserialize)]
struct ExplainQuery {
    /// Maximum number of reasons to return. Defaults to 20. Capped at 200.
    n: Option<usize>,
}

/// `GET /explain/latest[?n=<count>]` — fetch the last N routing decision reasons.
///
/// Returns a JSON array of [`crate::explainer::DecisionReason`] objects, most recent last.
/// Each object contains the chosen strategy, pressure metrics at decision time, and a
/// human-readable `explanation_text` sentence describing *why* that strategy was chosen.
///
/// This endpoint powers the "Decision Reasoning" panel in the dashboard.
///
/// ## Query Parameters
///
/// | Name | Type | Default | Description |
/// |------|------|---------|-------------|
/// | `n`  | usize | 20 | Number of most-recent reasons to return (capped at 200). |
///
/// ## Example
///
/// ```http
/// GET /explain/latest?n=5
/// ```
///
/// ```json
/// [
///   {
///     "job_id": 1042,
///     "chosen_strategy": "batch",
///     "pressure_score": 0.72,
///     "latency_ema_ms": 12.5,
///     "queue_depth": 80,
///     "compute_cost": 8000,
///     "scaling_potential": 0.85,
///     "timestamp_ms": 1711234567890,
///     "explanation_text": "Chose Batch because scaling_potential=0.85 is high, ..."
///   }
/// ]
/// ```
async fn get_explain_latest(
    Extension(explainer): Extension<ExplainerState>,
    Query(params): Query<ExplainQuery>,
) -> impl IntoResponse {
    let n = params.n.unwrap_or(20).min(200);
    let guard = explainer.lock().await;
    Json(guard.latest(n))
}

// ===== DAG Visualization =====

/// GET /api/dag — Return the current job DAG as a JSON graph (nodes + edges).
///
/// The response is a [`crate::dag::DagGraphPayload`] containing all nodes and
/// directed edges in the most recently constructed DAG. This endpoint is
/// designed to be consumed by a browser-side D3.js force-directed visualization:
///
/// ```js
/// const resp = await fetch("/api/dag");
/// const { nodes, edges } = await resp.json();
/// const simulation = d3.forceSimulation(nodes)
///     .force("link", d3.forceLink(edges).id(d => d.id))
///     .force("charge", d3.forceManyBody())
///     .force("center", d3.forceCenter(width / 2, height / 2));
/// ```
///
/// When no DAG has been constructed, returns an empty graph:
/// `{ "nodes": [], "edges": [], "node_count": 0, "edge_count": 0 }`.
///
/// # Response shape
///
/// ```json
/// {
///   "nodes": [
///     { "id": 0, "job_id": 1, "kind": "hash_mix", "compute_cost": 500,
///       "dep_count": 0, "is_leaf": false, "status": "pending" },
///     { "id": 1, "job_id": 2, "kind": "prime_count", "compute_cost": 5000,
///       "dep_count": 1, "is_leaf": true,  "status": "pending" }
///   ],
///   "edges": [
///     { "source": 0, "target": 1 }
///   ],
///   "node_count": 2,
///   "edge_count": 1
/// }
/// ```
async fn get_dag_graph(State(_router): State<AppState>) -> impl IntoResponse {
    // Return an illustrative empty DAG payload.
    // In a full integration a shared Arc<Mutex<Option<JobDag>>> would be
    // maintained by the router and queried here. The payload format is fully
    // defined by DagGraphPayload and ready for real data.
    use crate::dag::DagGraphPayload;
    let payload = DagGraphPayload {
        nodes: vec![],
        edges: vec![],
        node_count: 0,
        edge_count: 0,
    };
    Json(payload)
}

// ===== Prometheus =====

/// GET /metrics — Prometheus text-format exposition endpoint.
///
/// Emits counters and gauges for completed/dropped jobs, per-strategy routing
/// counts, per-strategy latency percentiles (p50/p95/p99/ema/min/max), and
/// neural-router learning quality metrics (sample_count, avg_reward, epsilon).
///
/// Content-Type is `text/plain; version=0.0.4` as required by the Prometheus
/// scrape protocol.
async fn metrics_prom(State(router): State<AppState>) -> Response {
    let snap = router.stats_snapshot().await;
    let summaries = router.latency_report().await;
    let neural_snap = router.neural_snapshot().await;
    let neural_metrics = NeuralMetrics {
        sample_count: neural_snap.sample_count,
        avg_reward: neural_snap.avg_reward,
        epsilon: neural_snap.epsilon,
        is_warmed_up: neural_snap.is_warmed_up,
    };
    // Pre-allocate a large enough buffer to avoid repeated reallocations.
    // This avoids creating many small heap allocations via format!() in the hot path.
    let base = prometheus_text_with_neural(
        snap.completed,
        snap.dropped,
        &snap.routed,
        &summaries,
        Some(&neural_metrics),
    );
    let mut text = String::with_capacity(base.len() + 4096);
    text.push_str(&base);
    // Batch observability counters.
    text.push_str("# TYPE helix_batch_enqueued counter\n");
    let mut sorted_enqueued: Vec<_> = snap.batch_enqueued.iter().collect();
    sorted_enqueued.sort_by_key(|(k, _)| *k);
    for (kind, count) in sorted_enqueued {
        use std::fmt::Write as _;
        let _ = write!(text, "helix_batch_enqueued{{kind=\"{kind}\"}} {count}\n");
    }
    text.push_str("# TYPE helix_batch_flushed counter\n");
    let mut sorted_flushed: Vec<_> = snap.batch_flushed.iter().collect();
    sorted_flushed.sort_by_key(|(k, _)| *k);
    for (kind, count) in sorted_flushed {
        use std::fmt::Write as _;
        let _ = write!(text, "helix_batch_flushed{{kind=\"{kind}\"}} {count}\n");
    }
    // Blocking panic counter.
    text.push_str("# HELP helix_blocking_panics_total Number of blocking CPU tasks that panicked\n");
    text.push_str("# TYPE helix_blocking_panics_total counter\n");
    {
        use std::fmt::Write as _;
        let _ = write!(text, "helix_blocking_panics_total {}\n", snap.blocking_panics);
    }
    // CPU queue depth gauge.
    text.push_str("# HELP helix_cpu_queue_depth Current number of jobs queued for CPU pool\n");
    text.push_str("# TYPE helix_cpu_queue_depth gauge\n");
    {
        use std::fmt::Write as _;
        let _ = write!(text, "helix_cpu_queue_depth {}\n", snap.cpu_queue_depth);
    }
    // Adaptive decay counter.
    text.push_str("# HELP helix_adaptive_decay_total Total adaptive spawn_threshold decay events\n");
    text.push_str("# TYPE helix_adaptive_decay_total counter\n");
    {
        use std::fmt::Write as _;
        let _ = write!(text, "helix_adaptive_decay_total {}\n", snap.adaptive_decay_count);
    }
    // Batch miss counter.
    text.push_str("# HELP helix_batch_miss_total Total batch buffer misses (unknown job kind)\n");
    text.push_str("# TYPE helix_batch_miss_total counter\n");
    {
        use std::fmt::Write as _;
        let _ = write!(text, "helix_batch_miss_total {}\n", snap.batch_miss_count);
    }
    // SLA violation counters per job kind.
    {
        use std::fmt::Write as _;
        text.push_str("# HELP helix_sla_violations_total Total SLA violations per job kind\n");
        text.push_str("# TYPE helix_sla_violations_total counter\n");
        let sla = router.sla_violation_counts();
        let _ = write!(text, "helix_sla_violations_total{{kind=\"hash_mix\"}} {}\n", sla.hash_mix);
        let _ = write!(text, "helix_sla_violations_total{{kind=\"prime_count\"}} {}\n", sla.prime_count);
        let _ = write!(text, "helix_sla_violations_total{{kind=\"monte_carlo_risk\"}} {}\n", sla.monte_carlo_risk);
        text.push_str("# HELP helix_deadline_exceeded_total Total jobs rejected due to expired deadline\n");
        text.push_str("# TYPE helix_deadline_exceeded_total counter\n");
        let _ = write!(text, "helix_deadline_exceeded_total {}\n", router.deadline_exceeded_count());
        text.push_str("# HELP helix_kind_routed_total Total jobs routed per job kind\n");
        text.push_str("# TYPE helix_kind_routed_total counter\n");
        let kr = router.kind_routing_stats();
        let _ = write!(text, "helix_kind_routed_total{{kind=\"hash_mix\"}} {}\n", kr.hash_mix);
        let _ = write!(text, "helix_kind_routed_total{{kind=\"prime_count\"}} {}\n", kr.prime_count);
        let _ = write!(text, "helix_kind_routed_total{{kind=\"monte_carlo_risk\"}} {}\n", kr.monte_carlo_risk);
    }

    let mut resp = (StatusCode::OK, text).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    resp
}

// ===== Trace API =====

/// JSON-serialisable representation of a completed span returned by the trace API.
#[derive(Debug, Serialize)]
struct SpanResponse {
    trace_id: u64,
    span_id: u64,
    parent_span_id: Option<u64>,
    operation: String,
    duration_ms: f64,
    tags: std::collections::HashMap<String, String>,
}

impl From<&CompletedSpan> for SpanResponse {
    fn from(s: &CompletedSpan) -> Self {
        Self {
            trace_id: s.trace_id,
            span_id: s.span_id,
            parent_span_id: s.parent_span_id,
            operation: s.operation.clone(),
            duration_ms: s.duration_ms,
            tags: s.tags.clone(),
        }
    }
}

/// `GET /api/traces/recent` — return the 100 most-recently completed spans.
///
/// Returns a JSON array of span objects ordered oldest-first.
async fn get_traces_recent(
    Extension(ctx): Extension<TraceState>,
) -> impl IntoResponse {
    let store = ctx.store();
    let guard = store.lock().await;
    let spans: Vec<SpanResponse> = guard.recent(100).iter().map(|s| SpanResponse::from(*s)).collect();
    Json(spans)
}

/// `GET /api/traces/:trace_id` — return all spans for a given trace ID.
///
/// `trace_id` must be a `u64` integer. Returns `[]` if no spans are found.
async fn get_traces_by_id(
    axum::extract::Path(trace_id): axum::extract::Path<u64>,
    Extension(ctx): Extension<TraceState>,
) -> impl IntoResponse {
    let store = ctx.store();
    let guard = store.lock().await;
    let spans: Vec<SpanResponse> = guard
        .query_by_trace(trace_id)
        .iter()
        .map(|s| SpanResponse::from(*s))
        .collect();
    Json(spans)
}

// ===== Deduplication stats =====

/// `GET /api/dedup/stats` — return current job deduplication statistics.
///
/// Returns a JSON object with:
/// - `total_submitted`  — total `submit()` calls since router start.
/// - `deduped_count`    — number of those calls that were classified as duplicates.
/// - `active_entries`   — in-flight entries currently tracked.
/// - `dedup_rate`       — fraction of submissions that were deduplicated.
async fn get_dedup_stats(State(router): State<AppState>) -> impl IntoResponse {
    #[derive(serde::Serialize)]
    struct DedupStatsResponse {
        total_submitted: u64,
        deduped_count: u64,
        active_entries: usize,
        dedup_rate: f64,
    }
    let stats = router.dedup_stats().await;
    Json(DedupStatsResponse {
        total_submitted: stats.total_submitted,
        deduped_count: stats.deduped_count,
        active_entries: stats.active_entries,
        dedup_rate: stats.dedup_rate,
    })
}

// ===== SLA queue stats =====

/// `GET /api/sla/stats` — return current SLA priority queue statistics.
///
/// Returns a JSON object with:
/// - `enqueued`  — total jobs ever pushed onto the SLA queue.
/// - `dequeued`  — total jobs popped from the SLA queue.
/// - `expired`   — total jobs removed as SLA-expired.
/// - `by_class`  — per-class breakdown (critical/high/normal/batch).
async fn get_sla_stats(State(router): State<AppState>) -> impl IntoResponse {
    #[derive(serde::Serialize)]
    struct ClassStatsJson {
        enqueued: u64,
        dequeued: u64,
        expired: u64,
    }
    #[derive(serde::Serialize)]
    struct SlaStatsResponse {
        enqueued: u64,
        dequeued: u64,
        expired: u64,
        by_class: std::collections::HashMap<String, ClassStatsJson>,
    }
    let stats = router.sla_stats().await;
    let by_class = stats
        .by_class
        .into_iter()
        .map(|(cls, cs)| {
            (
                cls.name().to_string(),
                ClassStatsJson {
                    enqueued: cs.enqueued,
                    dequeued: cs.dequeued,
                    expired: cs.expired,
                },
            )
        })
        .collect();
    Json(SlaStatsResponse {
        enqueued: stats.enqueued,
        dequeued: stats.dequeued,
        expired: stats.expired,
        by_class,
    })
}

/// `GET /api/result-cache/stats` — return result cache statistics as JSON.
async fn get_result_cache_stats(State(router): State<AppState>) -> impl IntoResponse {
    #[derive(serde::Serialize)]
    struct ResultCacheStatsResponse {
        entries: usize,
        hits: u64,
        misses: u64,
        evictions: u64,
        hit_rate: f64,
    }
    let stats = router.result_cache_stats().await;
    Json(ResultCacheStatsResponse {
        entries: stats.entries,
        hits: stats.hits,
        misses: stats.misses,
        evictions: stats.evictions,
        hit_rate: stats.hit_rate,
    })
}

/// `GET /api/flow/stats` — return flow controller statistics as JSON.
async fn get_flow_stats(State(router): State<AppState>) -> impl IntoResponse {
    #[derive(serde::Serialize)]
    struct FlowStatsResponse {
        admitted: u64,
        throttled: u64,
        rejected: u64,
        current_tokens: f64,
    }
    let stats = router.flow_stats();
    Json(FlowStatsResponse {
        admitted: stats.admitted,
        throttled: stats.throttled,
        rejected: stats.rejected,
        current_tokens: stats.current_tokens,
    })
}

/// `GET /api/timeouts/stats` — return adaptive timeout manager statistics as JSON.
///
/// Returns a map from job-kind to per-kind statistics:
/// - `p50_ms` — median observed latency (ms).
/// - `p95_ms` — 95th-percentile observed latency (ms); used as the default timeout.
/// - `p99_ms` — 99th-percentile observed latency (ms).
/// - `timeout_count` — number of times `mark_timeout` was called for this kind.
/// - `sample_count` — number of latency samples in the current sliding window.
///
/// Returns an empty object `{}` until at least one job completes and feeds its
/// latency into the timeout manager.
async fn get_timeout_stats(State(router): State<AppState>) -> impl IntoResponse {
    Json(router.timeout_stats().await)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── Simulate endpoint tests ───────────────────────────────────────────

    #[cfg(feature = "simulation")]
    #[test]
    fn test_simulate_bounds_defaults_and_clamps() {
        let d = SimulateParams { jobs: None, rate: None };
        assert_eq!(simulate_bounds(&d), (200, 40));
        let big = SimulateParams { jobs: Some(1_000_000), rate: Some(0) };
        assert_eq!(simulate_bounds(&big), (5_000, 1));
    }

    #[cfg(feature = "simulation")]
    #[tokio::test]
    async fn test_post_simulate_runs_new_jobs_each_burst() {
        let router: AppState = Arc::new(Router::new(RouterConfig::default()));
        for _ in 0..2 {
            let resp = post_simulate(
                State(Arc::clone(&router)),
                Query(SimulateParams { jobs: Some(20), rate: Some(2_000) }),
            )
            .await
            .into_response();
            assert_eq!(resp.status(), StatusCode::ACCEPTED);
        }
        // Both bursts use fresh seeds, so all 40 jobs are routed (none are
        // answered from the result cache of the first burst).
        let mut routed = 0;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let s = router.stats_snapshot().await;
            routed = s.routed.values().sum::<u64>();
            if routed >= 40 {
                break;
            }
        }
        assert_eq!(routed, 40);
    }

    #[test]
    fn test_index_html_has_run_button_and_light_theme() {
        assert!(INDEX_HTML.contains("/api/simulate"));
        assert!(INDEX_HTML.contains("prefers-color-scheme: light"));
        assert!(!INDEX_HTML.contains('\u{2014}'), "no em dashes in the dashboard");
    }

    // ── Health endpoint tests ─────────────────────────────────────────────

    #[test]
    fn test_health_response_status_is_ok() {
        let resp = HealthResponse {
            status: "ok",
            uptime_secs: 0,
        };
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn test_health_response_serializes_to_json() {
        let resp = HealthResponse {
            status: "ok",
            uptime_secs: 42,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(json.contains("\"status\":\"ok\""), "json: {json}");
        assert!(json.contains("\"uptime_secs\":42"), "json: {json}");
    }

    #[test]
    fn test_health_response_uptime_is_non_negative() {
        let resp = HealthResponse {
            status: "ok",
            uptime_secs: u64::MAX,
        };
        assert!(resp.uptime_secs <= u64::MAX);
    }

    #[test]
    fn test_index_html_is_not_empty() {
        assert!(!INDEX_HTML.is_empty());
    }

    #[test]
    fn test_index_html_has_decisions_feed_div() {
        assert!(INDEX_HTML.contains("id=\"decisions\""));
    }

    #[test]
    fn test_index_html_has_sse_eventsource() {
        assert!(INDEX_HTML.contains("EventSource"));
        assert!(INDEX_HTML.contains("/api/stream/decisions"));
    }

    #[test]
    fn test_index_html_has_donut_svg() {
        assert!(INDEX_HTML.contains("id=\"donut\""));
    }

    #[test]
    fn test_index_html_has_pressure_gauge() {
        assert!(INDEX_HTML.contains("pressure-fill"));
        assert!(INDEX_HTML.contains("pressure-val"));
    }

    #[test]
    fn test_index_html_has_latency_table() {
        assert!(INDEX_HTML.contains("lat-body"));
        assert!(INDEX_HTML.contains("EMA ms"));
    }

    #[test]
    fn test_index_html_has_adaptive_threshold_display() {
        assert!(INDEX_HTML.contains("adaptive_spawn_threshold"));
        assert!(INDEX_HTML.contains("id=\"threshold\""));
    }

    #[test]
    fn test_index_html_has_strategy_color_map() {
        assert!(INDEX_HTML.contains("COLORS"));
        assert!(INDEX_HTML.contains("inline"));
        assert!(INDEX_HTML.contains("cpu_pool"));
        assert!(INDEX_HTML.contains("batch"));
    }

    #[test]
    fn test_index_html_has_metrics_link() {
        assert!(INDEX_HTML.contains("/metrics"));
    }

    #[test]
    fn test_index_html_has_api_stats_link() {
        assert!(INDEX_HTML.contains("/api/stats"));
    }

    #[test]
    fn test_index_html_has_dark_background() {
        assert!(INDEX_HTML.contains("#0b0f14"));
    }

    #[test]
    fn test_index_html_has_stats_polling() {
        assert!(INDEX_HTML.contains("setInterval"));
        assert!(INDEX_HTML.contains("tick"));
    }

    #[test]
    fn test_index_html_has_draw_donut_function() {
        assert!(INDEX_HTML.contains("drawDonut"));
    }

    #[test]
    fn test_index_html_has_completed_counter() {
        assert!(INDEX_HTML.contains("id=\"completed\""));
    }

    #[test]
    fn test_index_html_has_dropped_counter() {
        assert!(INDEX_HTML.contains("id=\"dropped\""));
    }

    #[test]
    fn test_index_html_has_strategy_colors_for_all_strategies() {
        for s in ["inline", "spawn", "cpu_pool", "batch", "drop"] {
            assert!(INDEX_HTML.contains(s), "missing strategy color for {s}");
        }
    }

    #[test]
    fn test_index_html_has_legend_div() {
        assert!(INDEX_HTML.contains("id=\"legend\""));
    }

    #[test]
    fn test_index_html_no_inline_polling_via_setinterval_only_sse() {
        // SSE used for decisions, setInterval only for stats polling
        let sse_count = INDEX_HTML.matches("EventSource").count();
        assert_eq!(sse_count, 1);
    }

    #[test]
    fn test_index_html_has_bar_chart_classes() {
        assert!(INDEX_HTML.contains("strategy-inline"));
        assert!(INDEX_HTML.contains("strategy-spawn"));
    }

    #[test]
    fn test_index_html_valid_html_structure() {
        assert!(INDEX_HTML.contains("<!doctype html>"));
        assert!(INDEX_HTML.contains("</html>"));
        assert!(INDEX_HTML.contains("<head>"));
        assert!(INDEX_HTML.contains("</head>"));
        assert!(INDEX_HTML.contains("<body>"));
        assert!(INDEX_HTML.contains("</body>"));
    }

    #[test]
    fn test_index_html_is_valid_utf8() {
        // Ensure the HTML is valid UTF-8 (it uses em-dashes which are multi-byte but valid)
        assert!(std::str::from_utf8(INDEX_HTML.as_bytes()).is_ok());
    }

    // ── Cross-repo schema compatibility tests ────────────────────────────────
    //
    // EOT's HelixBridge deserializes /api/stats and /api/neural responses.
    // These tests pin the exact JSON field names and types so any rename or
    // structural change here will break the build rather than silently
    // producing wrong telemetry at runtime.

    /// Verify that `StatsResponse` serialises every field EOT expects.
    ///
    /// EOT's `RouterStats` mirrors: completed, dropped,
    /// adaptive_spawn_threshold, pressure_score, routed_by_strategy,
    /// latency_by_strategy.
    #[test]
    fn test_stats_response_schema_has_all_eot_fields() {
        let resp = StatsResponse {
            completed: 100,
            dropped: 5,
            adaptive_spawn_threshold: 250,
            pressure_score: 0.42,
            routed_by_strategy: vec![
                CountRow {
                    strategy: Strategy::Inline,
                    count: 80,
                },
                CountRow {
                    strategy: Strategy::Drop,
                    count: 5,
                },
            ],
            latency_by_strategy: vec![LatencyRow {
                strategy: Strategy::Inline,
                count: 80,
                avg_ms: 1.5,
                ema_ms: 1.4,
                p50_ms: 1,
                p95_ms: 4,
                p99_ms: 4,
                min_ms: 1,
                max_ms: 5,
            }],
            kind_routing: KindRoutingStats { hash_mix: 0, prime_count: 0, monte_carlo_risk: 0 },
            sla_violations: SLAViolationCounts { hash_mix: 0, prime_count: 0, monte_carlo_risk: 0 },
            deadline_exceeded: 0,
            epsilon_history: vec![],
        };

        let json = serde_json::to_string(&resp).expect("serialize StatsResponse");

        // Top-level scalar fields
        assert!(json.contains("\"completed\":100"), "missing completed");
        assert!(json.contains("\"dropped\":5"), "missing dropped");
        assert!(
            json.contains("\"adaptive_spawn_threshold\":250"),
            "missing adaptive_spawn_threshold"
        );
        assert!(
            json.contains("\"pressure_score\":0.42"),
            "missing pressure_score"
        );

        // Array fields
        assert!(
            json.contains("\"routed_by_strategy\""),
            "missing routed_by_strategy"
        );
        assert!(
            json.contains("\"latency_by_strategy\""),
            "missing latency_by_strategy"
        );

        // routed row fields
        assert!(
            json.contains("\"strategy\""),
            "missing strategy in routed row"
        );
        assert!(json.contains("\"count\""), "missing count in routed row");

        // latency row fields
        assert!(json.contains("\"avg_ms\""), "missing avg_ms");
        assert!(json.contains("\"ema_ms\""), "missing ema_ms");
        assert!(json.contains("\"p50_ms\""), "missing p50_ms");
        assert!(json.contains("\"p95_ms\""), "missing p95_ms");
        assert!(json.contains("\"p99_ms\""), "missing p99_ms");
        assert!(json.contains("\"min_ms\""), "missing min_ms");
        assert!(json.contains("\"max_ms\""), "missing max_ms");
    }

    /// Verify strategy names serialise as snake_case strings (EOT uses
    /// `#[serde(rename_all = "snake_case")]` when deserialising).
    #[test]
    fn test_stats_response_strategy_names_are_snake_case() {
        let resp = StatsResponse {
            completed: 0,
            dropped: 0,
            adaptive_spawn_threshold: 0,
            pressure_score: 0.0,
            routed_by_strategy: vec![
                CountRow {
                    strategy: Strategy::Inline,
                    count: 0,
                },
                CountRow {
                    strategy: Strategy::Spawn,
                    count: 0,
                },
                CountRow {
                    strategy: Strategy::CpuPool,
                    count: 0,
                },
                CountRow {
                    strategy: Strategy::Batch,
                    count: 0,
                },
                CountRow {
                    strategy: Strategy::Drop,
                    count: 0,
                },
            ],
            latency_by_strategy: vec![],
            kind_routing: KindRoutingStats { hash_mix: 0, prime_count: 0, monte_carlo_risk: 0 },
            sla_violations: SLAViolationCounts { hash_mix: 0, prime_count: 0, monte_carlo_risk: 0 },
            deadline_exceeded: 0,
            epsilon_history: vec![],
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(
            json.contains("\"inline\""),
            "Strategy::Inline should serialise as \"inline\""
        );
        assert!(
            json.contains("\"spawn\""),
            "Strategy::Spawn should serialise as \"spawn\""
        );
        assert!(
            json.contains("\"cpu_pool\""),
            "Strategy::CpuPool should serialise as \"cpu_pool\""
        );
        assert!(
            json.contains("\"batch\""),
            "Strategy::Batch should serialise as \"batch\""
        );
        assert!(
            json.contains("\"drop\""),
            "Strategy::Drop should serialise as \"drop\""
        );
    }

    /// Verify that `NeuralSnapshot` serialises the fields EOT expects.
    ///
    /// EOT's `NeuralRouterState` mirrors: sample_count, avg_reward,
    /// is_warmed_up, weights (Vec<Vec<f64>> that deserialises from [[f64;7];5]).
    #[test]
    fn test_neural_snapshot_schema_has_all_eot_fields() {
        use crate::router::NeuralSnapshot;

        let snap = NeuralSnapshot {
            sample_count: 150,
            avg_reward: 0.82,
            is_warmed_up: true,
            weights: [[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7]; 5],
            epsilon: 0.10,
            reward_variance: 0.0,
            per_strategy_counts: [0u64; 5],
        };

        let json = serde_json::to_string(&snap).expect("serialize NeuralSnapshot");

        assert!(
            json.contains("\"sample_count\":150"),
            "missing sample_count"
        );
        assert!(json.contains("\"avg_reward\":0.82"), "missing avg_reward");
        assert!(
            json.contains("\"is_warmed_up\":true"),
            "missing is_warmed_up"
        );
        assert!(json.contains("\"weights\""), "missing weights");
    }

    /// Verify that the weights matrix is a nested array parseable as Vec<Vec<f64>>.
    ///
    /// EOT deserialises `[[f64;7];5]` into `Vec<Vec<f64>>`.  serde renders
    /// fixed arrays identically to vecs, so this confirms the wire format
    /// remains compatible even if the type annotation changes.
    #[test]
    fn test_neural_snapshot_weights_deserialise_as_nested_vec() {
        use crate::router::NeuralSnapshot;
        use serde_json::Value;

        let snap = NeuralSnapshot {
            sample_count: 10,
            avg_reward: 0.5,
            is_warmed_up: false,
            weights: [[0.0; 7]; 5],
            epsilon: 0.10,
            reward_variance: 0.0,
            per_strategy_counts: [0u64; 5],
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let val: Value = serde_json::from_str(&json).expect("parse");

        let weights = val["weights"].as_array().expect("weights should be array");
        assert_eq!(weights.len(), 5, "outer dim should be 5 (N_STRATEGIES)");
        for (i, row) in weights.iter().enumerate() {
            let row_arr = row.as_array().expect("each row should be array");
            assert_eq!(
                row_arr.len(),
                7,
                "inner dim of row {i} should be 7 (N_FEATURES)"
            );
        }
    }

    /// Verify zero-state StatsResponse (all counters zero) serialises cleanly.
    #[test]
    fn test_stats_response_zero_state_serialises() {
        let resp = StatsResponse {
            completed: 0,
            dropped: 0,
            adaptive_spawn_threshold: 0,
            pressure_score: 0.0,
            routed_by_strategy: vec![],
            latency_by_strategy: vec![],
            kind_routing: KindRoutingStats { hash_mix: 0, prime_count: 0, monte_carlo_risk: 0 },
            sla_violations: SLAViolationCounts { hash_mix: 0, prime_count: 0, monte_carlo_risk: 0 },
            deadline_exceeded: 0,
            epsilon_history: vec![],
        };
        let json = serde_json::to_string(&resp).expect("serialize zero StatsResponse");
        assert!(json.contains("\"routed_by_strategy\":[]"));
        assert!(json.contains("\"latency_by_strategy\":[]"));
    }

    // ── EotTelemetry deserialization tests ────────────────────────────────

    #[test]
    fn test_eot_telemetry_deserializes_pressure() {
        let json = r#"{"pressure":0.75}"#;
        let body: EotTelemetry = serde_json::from_str(json).expect("deserialize");
        assert!((body.pressure - 0.75).abs() < 1e-9);
    }

    #[test]
    fn test_eot_telemetry_deserializes_zero() {
        let json = r#"{"pressure":0.0}"#;
        let body: EotTelemetry = serde_json::from_str(json).expect("deserialize");
        assert_eq!(body.pressure, 0.0);
    }

    #[test]
    fn test_eot_telemetry_deserializes_one() {
        let json = r#"{"pressure":1.0}"#;
        let body: EotTelemetry = serde_json::from_str(json).expect("deserialize");
        assert!((body.pressure - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_eot_telemetry_ignores_extra_fields() {
        let json = r#"{"pressure":0.5,"drop_rate":0.1,"extra":"ignored"}"#;
        let result = serde_json::from_str::<EotTelemetry>(json);
        assert!(result.is_ok(), "extra fields should be ignored: {result:?}");
    }

    #[test]
    fn test_eot_telemetry_fails_on_missing_pressure() {
        let json = r#"{"drop_rate":0.1}"#;
        let result = serde_json::from_str::<EotTelemetry>(json);
        assert!(
            result.is_err(),
            "missing pressure should fail deserialization"
        );
    }
}
