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
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

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
pub fn list_response<T, F>(
    headers: &HeaderMap,
    api_version: &'static str,
    list_kind: &'static str,
    items: Vec<T>,
    columns: &[Column],
    row_for: F,
) -> Response
where
    T: Serialize,
    F: Fn(&T) -> Vec<Value>,
{
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
/// raw object.
pub fn item_response<T, F>(
    headers: &HeaderMap,
    item: T,
    columns: &[Column],
    row_for: F,
) -> Response
where
    T: Serialize,
    F: Fn(&T) -> Vec<Value>,
{
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
