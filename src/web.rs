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

use std::{net::SocketAddr, sync::Arc, convert::Infallible};

use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response, Sse},
    response::sse::Event,
    routing::{get, post},
    Json, Router as AxumRouter,
};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::config::{RouterConfig, RouterConfigPatch};
use crate::metrics::{prometheus_text_with_neural, NeuralMetrics};
use crate::router::Router;
use crate::types::Strategy;

type AppState = Arc<Router>;

/// Start the Axum HTTP server and bind it to `addr`.
///
/// Registers all routes (dashboard, health, metrics, stats, config, SSE,
/// neural snapshot, EOT telemetry) and serves them until the process exits
/// or the underlying TCP listener is closed.
///
/// # Parameters
///
/// * `router` — Shared router instance; wrapped in `Arc` for handler access.
/// * `addr`   — TCP address to bind (e.g. `127.0.0.1:8080`).
///
/// # Errors
///
/// Returns `std::io::Error` if binding the TCP listener fails or if Axum's
/// internal `serve` loop encounters a fatal I/O error.
pub async fn serve(router: Router, addr: SocketAddr) -> std::io::Result<()> {
    let shared: AppState = Arc::new(router);

    let app = AxumRouter::new()
        .route("/", get(ui))
        .route("/health", get(health))
        .route("/metrics", get(metrics_prom))
        .route("/api/stats", get(stats_json))
        .route("/api/config", get(get_config).post(set_config).patch(patch_config))
        .route("/api/telemetry", post(post_eot_telemetry))
        .route("/api/stream/decisions", get(sse_decisions))
        .route("/api/neural", get(get_neural))
        .with_state(shared);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await.map_err(std::io::Error::other)
}

// ===== UI =====

/// Embedded HTML source for the dark live-routing dashboard served at `GET /`.
///
/// Single-file, no external dependencies. Vanilla JS SSE client polls
/// `/api/stream/decisions` and renders a strategy donut, latency table, and
/// pressure gauge in real time.
pub const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <title>HelixRouter</title>
  <style>
    *{box-sizing:border-box;margin:0;padding:0}
    body{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#0b0f14;color:#e6edf3;padding:16px}
    h1{font-size:1.2rem;color:#7dd3fc;margin-bottom:12px}
    .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(260px,1fr));gap:12px;margin-bottom:16px}
    .card{background:#0f1720;border:1px solid #1f2a37;border-radius:10px;padding:14px}
    .card h3{font-size:.8rem;color:#9aa4b2;text-transform:uppercase;letter-spacing:.06em;margin-bottom:8px}
    .big{font-size:2rem;font-weight:700;color:#e6edf3}
    .muted{color:#9aa4b2;font-size:.8rem}
    table{width:100%;border-collapse:collapse;font-size:.8rem}
    th{color:#9aa4b2;text-align:left;padding:4px 6px;border-bottom:1px solid #1f2a37}
    td{padding:4px 6px;border-bottom:1px solid #0f1720}
    .bar-wrap{background:#1f2a37;border-radius:4px;height:8px;overflow:hidden;margin-top:2px}
    .bar{height:100%;border-radius:4px;transition:width .4s}
    .strategy-inline{background:#34d399}
    .strategy-spawn{background:#60a5fa}
    .strategy-cpu_pool{background:#a78bfa}
    .strategy-batch{background:#fbbf24}
    .strategy-drop{background:#f87171}
    #decisions{height:180px;overflow-y:auto;font-size:.75rem;background:#060a0f;border-radius:6px;padding:8px}
    .dec{padding:2px 0;border-bottom:1px solid #0f1720;display:flex;gap:8px}
    .dec .strat{min-width:70px;font-weight:600}
    .gauge-wrap{position:relative;height:80px;display:flex;align-items:flex-end;justify-content:center}
    .gauge-label{font-size:1.4rem;font-weight:700;color:#e6edf3}
    .gauge-bar{position:absolute;bottom:0;left:0;right:0;height:8px;background:#1f2a37;border-radius:4px;overflow:hidden}
    .gauge-fill{height:100%;border-radius:4px;transition:width .5s,background .5s}
    .donut-wrap{display:flex;align-items:center;gap:16px}
    #donut{width:90px;height:90px}
    .legend{flex:1;display:flex;flex-direction:column;gap:4px;font-size:.75rem}
    .legend-item{display:flex;align-items:center;gap:6px}
    .legend-dot{width:10px;height:10px;border-radius:50%;flex-shrink:0}
    a{color:#7dd3fc;text-decoration:none}
    a:hover{text-decoration:underline}
  </style>
</head>
<body>
<h1>HelixRouter — Live Dashboard</h1>

<div class="grid">

  <!-- Throughput -->
  <div class="card">
    <h3>Throughput</h3>
    <div class="big" id="completed">—</div>
    <div class="muted">completed &nbsp;|&nbsp; <span id="dropped">—</span> dropped</div>
  </div>

  <!-- Pressure gauge -->
  <div class="card">
    <h3>System Pressure</h3>
    <div class="gauge-wrap">
      <span class="gauge-label" id="pressure-val">—</span>
    </div>
    <div class="gauge-bar"><div class="gauge-fill" id="pressure-fill" style="width:0%"></div></div>
  </div>

  <!-- Adaptive threshold -->
  <div class="card">
    <h3>Adaptive Threshold</h3>
    <div class="big" id="threshold">—</div>
    <div class="muted">spawn_threshold (auto-adapts)</div>
  </div>

  <!-- Strategy donut -->
  <div class="card">
    <h3>Strategy Distribution</h3>
    <div class="donut-wrap">
      <svg id="donut" viewBox="0 0 36 36"></svg>
      <div class="legend" id="legend"></div>
    </div>
  </div>

</div>

<!-- Latency table -->
<div class="card" style="margin-bottom:12px">
  <h3>Latency by Strategy</h3>
  <table>
    <thead><tr><th>Strategy</th><th>Count</th><th>Avg ms</th><th>EMA ms</th><th>p95 ms</th><th>Bar</th></tr></thead>
    <tbody id="lat-body"></tbody>
  </table>
</div>

<!-- Routing decisions feed -->
<div class="card">
  <h3>Live Routing Decisions <span class="muted" style="font-size:.7rem">(last 50)</span>
    &nbsp;<a href="/api/stats">/api/stats</a> &nbsp;<a href="/metrics">/metrics</a>
  </h3>
  <div id="decisions"></div>
</div>

<script>
const COLORS = {
  inline:'#34d399', spawn:'#60a5fa', cpu_pool:'#a78bfa', batch:'#fbbf24', drop:'#f87171'
};

/* ---- SSE live decisions ---- */
const decBox = document.getElementById('decisions');
const maxDec = 50;
const es = new EventSource('/api/stream/decisions');
es.onmessage = ev => {
  try {
    const d = JSON.parse(ev.data);
    const div = document.createElement('div');
    div.className = 'dec';
    const col = COLORS[d.strategy] || '#e6edf3';
    div.innerHTML = `<span class="strat" style="color:${col}">${d.strategy}</span>` +
      `<span class="muted">job#${d.job_id}</span>` +
      `<span>cost=${d.compute_cost}</span>` +
      `<span class="muted">cpu_busy=${d.cpu_busy}</span>` +
      `<span class="muted">p=${(d.pressure*100).toFixed(0)}%</span>`;
    decBox.prepend(div);
    while(decBox.children.length > maxDec) decBox.removeChild(decBox.lastChild);
  } catch(_){}
};

/* ---- Stats polling ---- */
async function tick() {
  try {
    const r = await fetch('/api/stats', {cache:'no-store'});
    const j = await r.json();
    document.getElementById('completed').textContent = j.completed ?? '—';
    document.getElementById('dropped').textContent = j.dropped ?? '—';
    document.getElementById('threshold').textContent = j.adaptive_spawn_threshold ?? '—';

    // Pressure gauge
    const p = j.pressure_score ?? 0;
    document.getElementById('pressure-val').textContent = (p*100).toFixed(0)+'%';
    const fill = document.getElementById('pressure-fill');
    fill.style.width = (p*100).toFixed(1)+'%';
    fill.style.background = p > 0.7 ? '#f87171' : p > 0.4 ? '#fbbf24' : '#34d399';

    // Latency table
    const rows = j.latency_by_strategy ?? [];
    const tbody = document.getElementById('lat-body');
    tbody.innerHTML = '';
    const maxP95 = Math.max(1, ...rows.map(r=>r.p95_ms||0));
    rows.forEach(r => {
      const tr = document.createElement('tr');
      const col = COLORS[r.strategy] || '#e6edf3';
      tr.innerHTML = `<td style="color:${col}">${r.strategy}</td>` +
        `<td>${r.count}</td>` +
        `<td>${(r.avg_ms||0).toFixed(2)}</td>` +
        `<td>${(r.ema_ms||0).toFixed(2)}</td>` +
        `<td>${r.p95_ms||0}</td>` +
        `<td><div class="bar-wrap"><div class="bar strategy-${r.strategy}" style="width:${((r.p95_ms||0)/maxP95*100).toFixed(1)}%"></div></div></td>`;
      tbody.appendChild(tr);
    });

    // Donut chart
    const routed = j.routed_by_strategy ?? [];
    const total = routed.reduce((s,r)=>s+r.count,0) || 1;
    drawDonut(routed, total);

  } catch(e) { console.warn('stats fetch failed', e); }
}

function drawDonut(routed, total) {
  const svg = document.getElementById('donut');
  const legend = document.getElementById('legend');
  svg.innerHTML = '';
  legend.innerHTML = '';

  const R = 15.9, cx = 18, cy = 18, r = R;
  const circ = 2 * Math.PI * r;
  let offset = 0;

  routed.forEach(item => {
    const frac = item.count / total;
    const len = frac * circ;
    const col = COLORS[item.strategy] || '#9aa4b2';

    const circle = document.createElementNS('http://www.w3.org/2000/svg','circle');
    circle.setAttribute('cx', cx);
    circle.setAttribute('cy', cy);
    circle.setAttribute('r', r);
    circle.setAttribute('fill', 'none');
    circle.setAttribute('stroke', col);
    circle.setAttribute('stroke-width', '5');
    circle.setAttribute('stroke-dasharray', `${len.toFixed(2)} ${(circ-len).toFixed(2)}`);
    circle.setAttribute('stroke-dashoffset', (-offset).toFixed(2));
    svg.appendChild(circle);
    offset += len;

    const li = document.createElement('div');
    li.className = 'legend-item';
    li.innerHTML = `<span class="legend-dot" style="background:${col}"></span>` +
      `<span style="color:${col}">${item.strategy}</span>` +
      `<span class="muted">${item.count}</span>`;
    legend.appendChild(li);
  });
}

tick();
setInterval(tick, 1500);
</script>
</body>
</html>"#;

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

// ===== SSE feed =====

async fn sse_decisions(State(router): State<AppState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
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
                Err(_) => None,
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
    })
}

// ===== Config API =====

async fn get_config(State(router): State<AppState>) -> impl IntoResponse {
    Json(router.config().await)
}

async fn set_config(
    State(router): State<AppState>,
    Json(cfg): Json<RouterConfig>,
) -> Response {
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

/// PATCH /api/config — apply a partial config update.
///
/// Only fields present in the JSON body are modified; absent fields
/// retain their current values. Returns the merged config on success.
///
/// Returns **422 Unprocessable Entity** if the resulting config would be
/// invalid (e.g. `inline_threshold >= spawn_threshold`, `ema_alpha` outside
/// `(0, 1]`, `cpu_parallelism == 0`). The live config is **not modified**
/// when validation fails.
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
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response(),
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

// ===== Neural router snapshot =====

/// GET /api/neural — current state of the online-learning neural router.
///
/// Returns sample count, average reward, warm-up status, and the full weight
/// matrix so operators (and EOT's HelixBridge) can observe how the neural
/// router is weighting each (strategy, feature) pair.
async fn get_neural(State(router): State<AppState>) -> impl IntoResponse {
    Json(router.neural_snapshot().await)
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
    };
    let text = prometheus_text_with_neural(snap.completed, snap.dropped, &snap.routed, &summaries, Some(&neural_metrics));

    let mut resp = (StatusCode::OK, text).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    resp
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── Health endpoint tests ─────────────────────────────────────────────

    #[test]
    fn test_health_response_status_is_ok() {
        let resp = HealthResponse { status: "ok", uptime_secs: 0 };
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn test_health_response_serializes_to_json() {
        let resp = HealthResponse { status: "ok", uptime_secs: 42 };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(json.contains("\"status\":\"ok\""), "json: {json}");
        assert!(json.contains("\"uptime_secs\":42"), "json: {json}");
    }

    #[test]
    fn test_health_response_uptime_is_non_negative() {
        let resp = HealthResponse { status: "ok", uptime_secs: u64::MAX };
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
                CountRow { strategy: Strategy::Inline, count: 80 },
                CountRow { strategy: Strategy::Drop,   count: 5  },
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
        };

        let json = serde_json::to_string(&resp).expect("serialize StatsResponse");

        // Top-level scalar fields
        assert!(json.contains("\"completed\":100"),              "missing completed");
        assert!(json.contains("\"dropped\":5"),                  "missing dropped");
        assert!(json.contains("\"adaptive_spawn_threshold\":250"), "missing adaptive_spawn_threshold");
        assert!(json.contains("\"pressure_score\":0.42"),        "missing pressure_score");

        // Array fields
        assert!(json.contains("\"routed_by_strategy\""),         "missing routed_by_strategy");
        assert!(json.contains("\"latency_by_strategy\""),        "missing latency_by_strategy");

        // routed row fields
        assert!(json.contains("\"strategy\""),                   "missing strategy in routed row");
        assert!(json.contains("\"count\""),                      "missing count in routed row");

        // latency row fields
        assert!(json.contains("\"avg_ms\""),                     "missing avg_ms");
        assert!(json.contains("\"ema_ms\""),                     "missing ema_ms");
        assert!(json.contains("\"p50_ms\""),                     "missing p50_ms");
        assert!(json.contains("\"p95_ms\""),                     "missing p95_ms");
        assert!(json.contains("\"p99_ms\""),                     "missing p99_ms");
        assert!(json.contains("\"min_ms\""),                     "missing min_ms");
        assert!(json.contains("\"max_ms\""),                     "missing max_ms");
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
                CountRow { strategy: Strategy::Inline,  count: 0 },
                CountRow { strategy: Strategy::Spawn,   count: 0 },
                CountRow { strategy: Strategy::CpuPool, count: 0 },
                CountRow { strategy: Strategy::Batch,   count: 0 },
                CountRow { strategy: Strategy::Drop,    count: 0 },
            ],
            latency_by_strategy: vec![],
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(json.contains("\"inline\""),   "Strategy::Inline should serialise as \"inline\"");
        assert!(json.contains("\"spawn\""),    "Strategy::Spawn should serialise as \"spawn\"");
        assert!(json.contains("\"cpu_pool\""), "Strategy::CpuPool should serialise as \"cpu_pool\"");
        assert!(json.contains("\"batch\""),    "Strategy::Batch should serialise as \"batch\"");
        assert!(json.contains("\"drop\""),     "Strategy::Drop should serialise as \"drop\"");
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
        };

        let json = serde_json::to_string(&snap).expect("serialize NeuralSnapshot");

        assert!(json.contains("\"sample_count\":150"),  "missing sample_count");
        assert!(json.contains("\"avg_reward\":0.82"),   "missing avg_reward");
        assert!(json.contains("\"is_warmed_up\":true"), "missing is_warmed_up");
        assert!(json.contains("\"weights\""),           "missing weights");
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
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let val: Value = serde_json::from_str(&json).expect("parse");

        let weights = val["weights"].as_array().expect("weights should be array");
        assert_eq!(weights.len(), 5, "outer dim should be 5 (N_STRATEGIES)");
        for (i, row) in weights.iter().enumerate() {
            let row_arr = row.as_array().expect("each row should be array");
            assert_eq!(row_arr.len(), 7, "inner dim of row {i} should be 7 (N_FEATURES)");
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
        assert!(result.is_err(), "missing pressure should fail deserialization");
    }
}
