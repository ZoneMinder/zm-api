//! Every JSON response must match the schema the OpenAPI document gives for
//! its route, method and status (#237).
//!
//! `scripts/check-openapi-compat.py` catches a change to the document; this
//! catches a handler that disagrees with it, which is how `GET /me` once broke
//! the dashboard (GH #62). [`checked`] wraps a router in a layer that buffers
//! each `application/json` response, validates it, and passes it on
//! unchanged. Other responses (streams, images, playlists) pass straight
//! through.
//!
//! `ZM_API_SCHEMA_CHECK` picks what happens on a finding: `enforce` (the
//! default) panics, failing the test; `report` appends a JSON line per finding
//! to the file named by `ZM_API_SCHEMA_REPORT`; `off` skips the check.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::{self, Body};
use axum::extract::Request;
use axum::http::header;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use serde_json::{json, Value};

/// Wrap `router` so its JSON responses are checked against the spec.
pub fn checked(router: Router) -> Router {
    router.layer(middleware::from_fn(check))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Enforce,
    Report,
    Off,
}

fn mode() -> Mode {
    match std::env::var("ZM_API_SCHEMA_CHECK").as_deref() {
        Ok("off") => Mode::Off,
        Ok("report") => Mode::Report,
        _ => Mode::Enforce,
    }
}

async fn check(req: Request, next: Next) -> Response {
    let mode = mode();
    if mode == Mode::Off {
        return next.run(req).await;
    }
    let method = req.method().as_str().to_ascii_lowercase();
    let path = req.uri().path().to_string();
    let response = next.run(req).await;

    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !is_json || !path.starts_with("/api/") {
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = body::to_bytes(body, usize::MAX)
        .await
        .expect("buffer JSON response");
    if let Some(finding) = finding(&method, &path, parts.status.as_u16(), &bytes) {
        match mode {
            Mode::Enforce => panic!(
                "{} {} answered {} with JSON its OpenAPI document doesn't describe \
                 ({}, route {}):\n{}\nbody: {}",
                method.to_uppercase(),
                path,
                parts.status,
                finding.kind,
                finding.template,
                finding.detail,
                String::from_utf8_lossy(&bytes)
            ),
            Mode::Report => report(&method, &path, parts.status.as_u16(), &finding),
            _ => {}
        }
    }
    Response::from_parts(parts, Body::from(bytes))
}

struct Finding {
    /// `mismatch`, `undocumented-status`, `no-json-schema` or `unknown-route`
    kind: &'static str,
    template: String,
    detail: String,
}

/// Routes the router serves on purpose without documenting them.
const UNDOCUMENTED: &[&str] = &[
    // Deprecated alias of /server/control/{action} (routes/server.rs).
    "/api/v3/states/change/",
];

fn finding(method: &str, path: &str, status: u16, bytes: &[u8]) -> Option<Finding> {
    let spec = spec();
    let Some(route) = spec.route(method, path) else {
        // A 404 for a path no route matches is the fallback, not an
        // undocumented route.
        if status == 404 || UNDOCUMENTED.iter().any(|p| path.starts_with(p)) {
            return None;
        }
        return Some(Finding {
            kind: "unknown-route",
            template: String::new(),
            detail: String::new(),
        });
    };
    let template = route.template.clone();
    let responses = &route.op["responses"];
    let Some(documented) = responses
        .get(status.to_string())
        .or_else(|| responses.get("default"))
    else {
        return Some(Finding {
            kind: "undocumented-status",
            template,
            detail: format!("documented: {}", keys(responses)),
        });
    };
    let documented = spec.resolve(documented);
    let Some(schema) = documented
        .pointer("/content/application~1json/schema")
        .cloned()
    else {
        // `Json(())` sends `null`: no content, as documented.
        if bytes == b"null" {
            return None;
        }
        return Some(Finding {
            kind: "no-json-schema",
            template,
            detail: format!("content: {}", keys(&documented["content"])),
        });
    };
    let instance: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            return Some(Finding {
                kind: "mismatch",
                template,
                detail: format!("not JSON: {e}"),
            })
        }
    };
    let validator = spec.validator(&route.template, method, status, schema);
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .take(10)
        .map(|e| format!("  at {}: {}", e.instance_path(), e))
        .collect();
    (!errors.is_empty()).then(|| Finding {
        kind: "mismatch",
        template,
        detail: errors.join("\n"),
    })
}

fn keys(v: &Value) -> String {
    v.as_object()
        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

fn report(method: &str, path: &str, status: u16, f: &Finding) {
    let Ok(file) = std::env::var("ZM_API_SCHEMA_REPORT") else {
        return;
    };
    let line = json!({
        "kind": f.kind,
        "method": method.to_uppercase(),
        "route": f.template,
        "path": path,
        "status": status,
        "detail": f.detail,
    });
    if let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        // One write per line: tests run in parallel and O_APPEND keeps a
        // single small write whole.
        let _ = out.write_all(format!("{line}\n").as_bytes());
    }
}

struct Route {
    method: String,
    template: String,
    pattern: regex::Regex,
    params: usize,
    op: Value,
}

struct Spec {
    routes: Vec<Route>,
    doc: Value,
    validators: Mutex<HashMap<(String, String, u16), Arc<jsonschema::Validator>>>,
}

fn spec() -> &'static Spec {
    static SPEC: OnceLock<Spec> = OnceLock::new();
    SPEC.get_or_init(|| {
        let doc = serde_json::to_value(zm_api::routes::served_openapi()).expect("spec as JSON");
        let mut routes = Vec::new();
        let param = regex::Regex::new(r"\\\{[^}]+\\\}").unwrap();
        for (template, item) in doc["paths"].as_object().expect("paths") {
            let pattern = format!("^{}$", param.replace_all(&regex::escape(template), "[^/]+"));
            for (method, op) in item.as_object().expect("path item") {
                routes.push(Route {
                    method: method.clone(),
                    template: template.clone(),
                    pattern: regex::Regex::new(&pattern).expect("route pattern"),
                    params: template.matches('{').count(),
                    op: op.clone(),
                });
            }
        }
        Spec {
            routes,
            doc,
            validators: Mutex::new(HashMap::new()),
        }
    })
}

impl Spec {
    /// The operation for `method` on `path`. A literal segment beats a
    /// parameter, so `/events/counts` isn't taken for `/events/{id}`.
    fn route(&self, method: &str, path: &str) -> Option<&Route> {
        self.routes
            .iter()
            .filter(|r| r.method == method && r.pattern.is_match(path))
            .min_by_key(|r| r.params)
    }

    /// Follow a `$ref` to a shared response component.
    fn resolve(&self, v: &Value) -> Value {
        match v.get("$ref").and_then(Value::as_str) {
            Some(r) => self
                .doc
                .pointer(r.trim_start_matches('#'))
                .cloned()
                .unwrap_or(Value::Null),
            None => v.clone(),
        }
    }

    fn validator(
        &self,
        template: &str,
        method: &str,
        status: u16,
        schema: Value,
    ) -> Arc<jsonschema::Validator> {
        let key = (template.to_string(), method.to_string(), status);
        let mut cache = self.validators.lock().unwrap();
        cache
            .entry(key)
            .or_insert_with(|| {
                // `#/components/schemas/...` references resolve against this
                // root, so carry the spec's components beside the schema.
                let mut root = schema;
                if let Value::Object(map) = &mut root {
                    map.insert("components".into(), self.doc["components"].clone());
                }
                Arc::new(
                    jsonschema::draft202012::new(&root)
                        .unwrap_or_else(|e| panic!("schema for {method} {template}: {e}")),
                )
            })
            .clone()
    }
}
