//! Server-side Table response support.
//!
//! `kubectl get <thing>` sends `Accept: application/json;as=Table;v=...` and
//! expects a `meta.k8s.io/v1.Table` back. Without it kubectl falls back to a
//! barebones printer that only shows NAME + AGE for unknown response shapes —
//! which is why `kubectl get nodes` was looking empty against this server.
//!
//! Each resource lists its own columns (NAME / STATUS / AGE / ...) and a
//! row-builder that maps an item to cells. We always embed the original object
//! in `rows[i].object` so kubectl can still do `-o yaml` etc.

use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::bus::{ChangeEvent, WatchFilter};

/// Header set by the watch-detect middleware (see [`super::watch_middleware`])
/// to mark a request as `?watch=true|1`. We propagate the watch flag through a
/// header rather than threading the URI down to every list handler.
pub const WATCH_HEADER: &str = "x-kais-watch";

/// True iff the request's `Accept` header asks for a Table response.
/// kubectl sends `as=Table;v=1.metainternal.k8s.io` — match anything carrying
/// `as=Table`.
pub fn wants_table(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .any(|h| h.contains("as=Table"))
}

/// True iff the request URI carried `?watch=true` (or `=1`). Watches must
/// always emit a stream of `WatchEvent` objects, never a Table — kubectl's
/// watch decoder uses the standard kubernetes scheme which doesn't know how
/// to decode `meta.k8s.io/v1.Table` as a watch payload, and that's exactly
/// the "no kind 'Table' is registered" error users hit otherwise.
pub fn wants_watch(headers: &HeaderMap) -> bool {
    headers.get(WATCH_HEADER).is_some()
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: &'static str,
    pub kind: &'static str, // "string" | "integer" | "date"
    /// 0 = always shown; 1 = only with `-o wide`.
    pub priority: i32,
}

impl Column {
    pub const fn new(name: &'static str, kind: &'static str) -> Self {
        Self { name, kind, priority: 0 }
    }
    pub const fn wide(name: &'static str, kind: &'static str) -> Self {
        Self { name, kind, priority: 1 }
    }
}

/// Build a Table response. If the request didn't ask for one, return the
/// regular list payload `(api_version, kind_list, items)` instead.
///
/// `?watch=true` requests bypass both branches and stream a sequence of
/// `WatchEvent` objects — see [`watch_response`].
pub fn list_response<T, F>(
    headers: &HeaderMap,
    api_version: &'static str,
    list_kind: &'static str,
    items: Vec<T>,
    columns: &[Column],
    row_for: F,
) -> Response
where
    T: Serialize + Send + 'static,
    F: Fn(&T) -> Vec<Value>,
{
    if wants_watch(headers) {
        return watch_response(items);
    }
    if wants_table(headers) {
        let table = build_table(columns, &items, &row_for);
        return Json(table).into_response();
    }

    Json(json!({
        "apiVersion": api_version,
        "kind": list_kind,
        "metadata": {},
        "items": items,
    }))
    .into_response()
}

/// Stream a watch response: one `ADDED` event per current item, then an
/// initial `BOOKMARK` so the client knows it's caught up, then a slow
/// trickle of `BOOKMARK` keep-alives. We don't have a real change feed
/// yet, so the stream eventually ends and kubectl will reconnect — that's
/// fine; the important thing is we never send a `Table` on a watch URI.
pub fn watch_response<T: Serialize + Send + 'static>(items: Vec<T>) -> Response {
    let mut buf = Vec::new();
    for item in &items {
        let event = json!({
            "type": "ADDED",
            "object": item,
        });
        if let Ok(bytes) = serde_json::to_vec(&event) {
            buf.extend_from_slice(&bytes);
            buf.push(b'\n');
        }
    }
    // Initial BOOKMARK marks "you have everything that exists so far".
    let bookmark = json!({
        "type": "BOOKMARK",
        "object": {
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": { "resourceVersion": "0" },
        },
    });
    if let Ok(b) = serde_json::to_vec(&bookmark) {
        buf.extend_from_slice(&b);
        buf.push(b'\n');
    }

    let initial = stream::once(async move {
        Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(buf))
    });

    // Periodic keep-alive bookmarks so the client doesn't reconnect in a
    // tight loop. 30s × 60 = 30 minutes of stream lifetime per request.
    let keepalive = stream::iter(0..60).then(|_| async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let bookmark = json!({
            "type": "BOOKMARK",
            "object": {
                "kind": "Status",
                "apiVersion": "v1",
                "metadata": { "resourceVersion": "0" },
            },
        });
        let mut b = serde_json::to_vec(&bookmark).unwrap_or_default();
        b.push(b'\n');
        Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(b))
    });

    let body = Body::from_stream(initial.chain(keepalive));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::TRANSFER_ENCODING, "chunked")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Like [`watch_response`] but, after the initial snapshot dump and
/// BOOKMARK, forwards live events from `bus_rx` matching `filter` to the
/// client until the connection drops (or the broadcast channel lags
/// past its buffer, at which point we close the stream and let kubectl
/// reconnect / re-list).
pub fn live_watch_response<T: Serialize + Send + 'static>(
    items: Vec<T>,
    filter: WatchFilter,
    mut bus_rx: broadcast::Receiver<ChangeEvent>,
) -> Response {
    // mpsc channel funnels three producers (snapshot dump, live events,
    // keepalive bookmarks) into a single chunked HTTP body.
    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();

    // 1. Snapshot dump + initial BOOKMARK, sent up front (synchronously
    //    queued — the channel is unbounded so this never blocks).
    for item in &items {
        let event = json!({ "type": "ADDED", "object": item });
        if let Some(line) = encode_event_line(&event) {
            let _ = out_tx.send(line);
        }
    }
    if let Some(line) = encode_event_line(&bookmark_event()) {
        let _ = out_tx.send(line);
    }

    // 2. Live forwarder: read from the bus, apply the filter, push
    //    matching events through. Ends on lag/closed (kubectl
    //    reconnects).
    let live_tx = out_tx.clone();
    tokio::spawn(async move {
        loop {
            match bus_rx.recv().await {
                Ok(ev) if filter.matches(&ev) => {
                    let payload = json!({
                        "type": ev.event_type.as_wire(),
                        "object": ev.object,
                    });
                    let Some(line) = encode_event_line(&payload) else { continue };
                    if live_tx.send(line).is_err() {
                        break; // client disconnected
                    }
                }
                Ok(_) => continue, // filtered out
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        "watch lagged by {} events ({}/{:?}); closing stream — client will reconnect",
                        n,
                        filter.kind,
                        filter.namespace
                    );
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 3. Keepalive bookmarks every 30s so kubectl knows the stream is
    //    alive and idle proxies don't time us out.
    let keepalive_tx = out_tx;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        // first tick fires immediately — skip it; we already sent the initial bookmark.
        interval.tick().await;
        loop {
            interval.tick().await;
            let Some(line) = encode_event_line(&bookmark_event()) else { continue };
            if keepalive_tx.send(line).is_err() {
                break;
            }
        }
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(out_rx)
        .map(|b| Ok::<bytes::Bytes, std::io::Error>(b));
    let body = Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::TRANSFER_ENCODING, "chunked")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn bookmark_event() -> Value {
    json!({
        "type": "BOOKMARK",
        "object": {
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": { "resourceVersion": "0" },
        },
    })
}

fn encode_event_line(event: &Value) -> Option<bytes::Bytes> {
    let mut bytes = serde_json::to_vec(event).ok()?;
    bytes.push(b'\n');
    Some(bytes::Bytes::from(bytes))
}

/// Variant of [`list_response`] that subscribes to the change-feed bus
/// when the request is a watch, so the client gets *live* updates after
/// the initial snapshot. Non-watch requests behave identically to
/// `list_response`.
#[allow(clippy::too_many_arguments)]
pub fn list_response_live<T, F>(
    headers: &HeaderMap,
    api_version: &'static str,
    list_kind: &'static str,
    items: Vec<T>,
    columns: &[Column],
    row_for: F,
    filter: WatchFilter,
    bus: &super::bus::Bus,
) -> Response
where
    T: Serialize + Send + 'static,
    F: Fn(&T) -> Vec<Value>,
{
    if wants_watch(headers) {
        return live_watch_response(items, filter, bus.subscribe());
    }
    if wants_table(headers) {
        let table = build_table(columns, &items, &row_for);
        return Json(table).into_response();
    }
    Json(json!({
        "apiVersion": api_version,
        "kind": list_kind,
        "metadata": {},
        "items": items,
    }))
    .into_response()
}

/// Single-object `get` variant: return either a Table with one row, or the
/// raw object. `?watch=true` on a single-object endpoint emits one ADDED
/// event followed by the keep-alive stream — same shape as the list path.
pub fn item_response<T, F>(
    headers: &HeaderMap,
    item: T,
    columns: &[Column],
    row_for: F,
) -> Response
where
    T: Serialize + Send + 'static,
    F: Fn(&T) -> Vec<Value>,
{
    if wants_watch(headers) {
        return watch_response(vec![item]);
    }
    if wants_table(headers) {
        let table = build_table(columns, std::slice::from_ref(&item), &row_for);
        return Json(table).into_response();
    }
    Json(item).into_response()
}

fn build_table<T, F>(columns: &[Column], items: &[T], row_for: &F) -> Value
where
    T: Serialize,
    F: Fn(&T) -> Vec<Value>,
{
    let column_definitions: Vec<Value> = columns
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "type": c.kind,
                "format": "",
                "description": "",
                "priority": c.priority,
            })
        })
        .collect();

    let rows: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "cells": row_for(item),
                "object": item,
            })
        })
        .collect();

    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "columnDefinitions": column_definitions,
        "rows": rows,
    })
}

/// Format a `creationTimestamp` as a kubectl-style "5m" / "2d" age string.
pub fn age_str(ts: Option<DateTime<Utc>>) -> String {
    match ts {
        Some(t) => format_age(Utc::now() - t),
        None => "<unknown>".to_string(),
    }
}

fn format_age(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return format!("{}s", secs);
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m", mins);
    }
    let hrs = mins / 60;
    if hrs < 48 {
        return format!("{}h", hrs);
    }
    let days = hrs / 24;
    format!("{}d", days)
}
