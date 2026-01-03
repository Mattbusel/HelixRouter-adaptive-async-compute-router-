use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router as AxumRouter,
};
use serde::Serialize;

use crate::router::{LatencySummary, Router};
use crate::types::Strategy;

pub async fn serve(router: Router, addr: SocketAddr) {
    // If your Router is already internally Arc'd you can remove this.
    let shared = Arc::new(router);

    let app = AxumRouter::new()
        .route("/", get(ui))
        // Correct mapping:
        .route("/metrics", get(metrics_prom))
        .route("/api/stats", get(stats_json))
        .with_state(shared);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind http addr");

    axum::serve(listener, app).await.expect("serve");
}

// ---------- UI ----------

async fn ui() -> Html<&'static str> {
    // Keep it simple: UI fetches /api/stats (JSON).
    // Your existing UI is fine; this is just a fallback minimal UI.
    Html(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>HelixRouter</title>
  <style>
    body { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; background:#0b0f14; color:#e6edf3; }
    .card { background:#0f1720; border:1px solid #1f2a37; border-radius:12px; padding:16px; max-width: 900px; margin: 24px auto; }
    table { width:100%; border-collapse: collapse; }
    th, td { padding: 6px 8px; border-bottom: 1px solid #1f2a37; }
    .muted { color:#9aa4b2; }
    a { color:#7dd3fc; text-decoration: none; }
  </style>
</head>
<body>
  <div class="card">
    <h2>HelixRouter</h2>
    <div class="muted">Live routing counters + latency summaries.</div>
    <p class="muted">API: <a href="/api/stats">/api/stats</a> · Metrics: <a href="/metrics">/metrics</a></p>

    <pre id="out" class="muted">loading…</pre>
  </div>

<script>
async function tick() {
  try {
    const r = await fetch("/api/stats", { cache: "no-store" });
    const j = await r.json();
    document.getElementById("out").textContent = JSON.stringify(j, null, 2);
  } catch (e) {
    document.getElementById("out").textContent = "fetch failed: " + e;
  }
}
tick();
setInterval(tick, 1000);
</script>
</body>
</html>"#,
    )
}

// ---------- JSON stats ----------

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
    p95_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct StatsResponse {
    completed: u64,
    dropped: u64,
    routed_by_strategy: Vec<CountRow>,
    latency_by_strategy: Vec<LatencyRow>,
}

async fn stats_json(State(router): State<Arc<Router>>) -> impl IntoResponse {
    // IMPORTANT: this must be the ASYNC snapshot (no blocking_lock inside runtime).
    let snap = router.stats_snapshot().await;

    // Make routed stable for UI
    let mut routed: Vec<CountRow> = snap
        .routed
        .into_iter()
        .map(|(strategy, count)| CountRow { strategy, count })
        .collect();
    routed.sort_by_key(|r| r.strategy.to_string());

    let mut latency_rows: Vec<LatencyRow> = Vec::new();
    for s in router.latency_report().await {
        latency_rows.push(LatencyRow {
            strategy: s.strategy,
            count: s.count,
            avg_ms: s.avg_ms,
            p95_ms: s.p95_ms,
        });
    }
    latency_rows.sort_by_key(|r| r.strategy.to_string());

    Json(StatsResponse {
        completed: snap.completed,
        dropped: snap.dropped,
        routed_by_strategy: routed,
        latency_by_strategy: latency_rows,
    })
}

// ---------- Prometheus text ----------

async fn metrics_prom(State(router): State<Arc<Router>>) -> Response {
    // Prometheus endpoint should be text/plain and MUST NOT be JSON.
    // (This is what you're currently seeing at /api/stats.)
    let snap = router.stats_snapshot().await;

    let mut out = String::new();
    out.push_str("# TYPE helix_completed counter\n");
    out.push_str(&format!("helix_completed {}\n", snap.completed));
    out.push_str("# TYPE helix_dropped counter\n");
    out.push_str(&format!("helix_dropped {}\n", snap.dropped));
    out.push_str("# TYPE helix_routed counter\n");

    let mut routed: Vec<(Strategy, u64)> = snap.routed.into_iter().collect();
    routed.sort_by_key(|(s, _)| s.to_string());

    for (k, v) in routed {
        out.push_str(&format!("helix_routed{{strategy=\"{}\"}} {}\n", k, v));
    }

    let mut resp = (StatusCode::OK, out).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    resp
}
