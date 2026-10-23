//! Embedded Control REST API and Dashboard server (`axum`), RFC 7807 Problem Details,
//! and localhost security guards (Host header check + `X-Wrapper-Request: 1` anti-CSRF check).

use crate::constants::headers;
use crate::manager::{ApplicationManager, ManagerError};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Shared state passed to Control API route handlers and security middleware.
#[derive(Clone)]
pub struct ApiState {
    pub manager: ApplicationManager,
    pub configured_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct OutputQuery {
    pub lines: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputResponse {
    pub name: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadResponse {
    pub summary: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub unchanged: Vec<String>,
    pub requires_service_restart: Vec<String>,
}

/// Build the `axum::Router` with all Phase 2 §7.1 endpoints and §7.3 security guards.
pub fn build_router(manager: ApplicationManager, configured_port: u16) -> Router {
    let state = Arc::new(ApiState {
        manager,
        configured_port,
    });

    Router::new()
        .route("/", get(get_dashboard))
        .route("/api/applications", get(list_applications))
        .route("/api/applications/:name", get(get_application))
        .route("/api/applications/:name/start", post(start_application))
        .route("/api/applications/:name/stop", post(stop_application))
        .route("/api/applications/:name/restart", post(restart_application))
        .route("/api/applications/:name/output", get(get_application_output))
        .route("/api/configuration/reload", post(reload_configuration))
        .route("/api/health", get(get_health))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_guard_middleware,
        ))
        // Enforce a 16 KiB body size limit to keep memory footprint tiny and prevent payload DoS
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}

/// Phase 2 §7.3 Security Middleware:
/// 1. Validates `Host` header to allow only `127.0.0.1`, `localhost`, or `[::1]` (blocks DNS rebinding).
/// 2. Requires `X-Wrapper-Request: 1` on all `POST` requests (blocks browser cross-site CSRF).
/// 3. Attaches defensive HTTP headers (`X-Content-Type-Options`, `X-Frame-Options`, `Cache-Control`, `CSP`).
async fn security_guard_middleware(
    State(state): State<Arc<ApiState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // 1. Host header check (DNS-rebinding mitigation)
    if let Some(host_val) = req.headers().get(header::HOST) {
        let host_str = host_val.to_str().unwrap_or("");
        if !is_allowed_loopback_host(host_str, state.configured_port) {
            return problem_response(
                StatusCode::FORBIDDEN,
                "Forbidden Host Header",
                &format!(
                    "Host header '{host_str}' is not allowed. Only loopback hosts (127.0.0.1, localhost, [::1]) are permitted."
                ),
                None,
            );
        }
    } else {
        return problem_response(
            StatusCode::FORBIDDEN,
            "Missing Host Header",
            "Requests to the Control API must include a valid loopback Host header.",
            None,
        );
    }

    // 2. Anti-CSRF header check on all POST requests
    if req.method() == Method::POST {
        let csrf_ok = req
            .headers()
            .get(headers::WRAPPER_REQUEST)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim() == headers::WRAPPER_REQUEST_VALUE)
            .unwrap_or(false);

        if !csrf_ok {
            return problem_response(
                StatusCode::FORBIDDEN,
                "Missing Anti-CSRF Header",
                "POST requests must carry header 'X-Wrapper-Request: 1'.",
                None,
            );
        }
    }

    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
        ),
    );
    response
}

fn is_allowed_loopback_host(host_header: &str, configured_port: u16) -> bool {
    let trimmed = host_header.trim().to_ascii_lowercase();
    let port_suffix = format!(":{configured_port}");

    let host_only = trimmed
        .strip_suffix(&port_suffix)
        .unwrap_or(&trimmed);

    matches!(host_only, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

async fn get_dashboard() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

async fn list_applications(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    Json(state.manager.list_snapshots())
}

async fn get_application(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
) -> Response {
    match state.manager.get_snapshot(&name) {
        Ok(snap) => (StatusCode::OK, Json(snap)).into_response(),
        Err(err) => map_manager_error(err),
    }
}

async fn start_application(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let (source, user) = extract_caller_metadata(&headers);
    match state
        .manager
        .start_application(&name, &source, user.as_deref())
        .await
    {
        Ok(snap) => (StatusCode::ACCEPTED, Json(snap)).into_response(),
        Err(err) => map_manager_error(err),
    }
}

async fn stop_application(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let (source, user) = extract_caller_metadata(&headers);
    match state
        .manager
        .stop_application(&name, &source, user.as_deref())
        .await
    {
        Ok(snap) => (StatusCode::ACCEPTED, Json(snap)).into_response(),
        Err(err) => map_manager_error(err),
    }
}

async fn restart_application(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let (source, user) = extract_caller_metadata(&headers);
    match state
        .manager
        .restart_application(&name, &source, user.as_deref())
        .await
    {
        Ok(snap) => (StatusCode::ACCEPTED, Json(snap)).into_response(),
        Err(err) => map_manager_error(err),
    }
}

async fn get_application_output(
    State(state): State<Arc<ApiState>>,
    Path(name): Path<String>,
    Query(query): Query<OutputQuery>,
) -> Response {
    let line_count = query.lines.unwrap_or(200).clamp(1, 5000);
    match state.manager.get_output(&name, line_count) {
        Ok(lines) => (
            StatusCode::OK,
            Json(OutputResponse {
                name,
                lines,
            }),
        )
            .into_response(),
        Err(err) => map_manager_error(err),
    }
}

async fn reload_configuration(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Response {
    let (source, user) = extract_caller_metadata(&headers);
    match state
        .manager
        .reload_configuration(&source, user.as_deref())
        .await
    {
        Ok(diff) => {
            let body = ReloadResponse {
                summary: diff.summary_string(),
                added: diff.added,
                removed: diff.removed,
                changed: diff.changed,
                unchanged: diff.unchanged,
                requires_service_restart: diff.requires_service_restart,
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(err) => map_manager_error(err),
    }
}

async fn get_health(State(state): State<Arc<ApiState>>) -> Response {
    let report = state.manager.evaluate_service_health();
    let status = if report.healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(report)).into_response()
}

fn extract_caller_metadata(headers: &HeaderMap) -> (String, Option<String>) {
    let source = headers
        .get(headers::WRAPPER_SOURCE)
        .and_then(|v| v.to_str().ok())
        .filter(|s| matches!(*s, "dashboard" | "cli" | "api"))
        .unwrap_or("api")
        .to_string();

    let user = headers
        .get(headers::WRAPPER_USER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    (source, user)
}

fn map_manager_error(err: ManagerError) -> Response {
    match err {
        ManagerError::UnknownApplication(name) => problem_response(
            StatusCode::NOT_FOUND,
            "Unknown Application",
            &format!("Application '{name}' does not exist."),
            None,
        ),
        ManagerError::ValidationFailed(errors) => problem_response(
            StatusCode::BAD_REQUEST,
            "Invalid Configuration",
            &format!("Configuration validation failed: {}", errors.join("; ")),
            Some(json!({ "errors": errors })),
        ),
        ManagerError::InternalError(msg) => problem_response(
            StatusCode::CONFLICT,
            "Operation Conflict",
            &msg,
            None,
        ),
    }
}

/// Format an RFC 7807 Problem Details response (`application/problem+json`).
fn problem_response(
    status: StatusCode,
    title: &str,
    detail: &str,
    extensions: Option<Value>,
) -> Response {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "type".to_string(),
        Value::String(format!("https://httpstatuses.io/{}", status.as_u16())),
    );
    obj.insert("title".to_string(), Value::String(title.to_string()));
    obj.insert("status".to_string(), json!(status.as_u16()));
    obj.insert("detail".to_string(), Value::String(detail.to_string()));

    if let Some(Value::Object(ext_map)) = extensions {
        for (k, v) in ext_map {
            obj.insert(k, v);
        }
    }

    let body = serde_json::to_string(&Value::Object(obj)).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        )
        .body(Body::from(body))
        .unwrap()
}
