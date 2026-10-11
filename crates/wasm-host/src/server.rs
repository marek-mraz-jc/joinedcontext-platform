//! The shard's HTTP side (ADR-N-044 §2.1), two listeners. The apps port answers
//! `/apps/{name}/api/*` to the App placed under that name and `/healthz`; the edge reaches it, has
//! checked the login already, and the host passes the caller's token to the gateway on the App's
//! behalf and never to the App. The admin port answers `/healthz`, `/metrics` and `/jobs` (the
//! last run of each job, AP-154), every App's records, so only the Portal and the scraper reach it
//! (T-3857): no edge route, and no listener the edge reaches, ever serves them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::host::{capped, Failure, Host};
use crate::jobs::{records_json, HostRunner, Scheduler};
use crate::metrics::{Metrics, Outcome};
use crate::placement::{Placed, Placement};

pub type Body = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// What a shard serves: its runtime, its placement and its counters.
pub struct Shard {
    pub id: String,
    pub host: Arc<Host>,
    pub apps: RwLock<HashMap<String, Placed>>,
    pub metrics: Metrics,
    /// The scheduler of the placed Apps' jobs, whose records `/jobs` answers (AP-154).
    pub jobs: Arc<Scheduler<HostRunner>>,
}

fn text(status: u16, content_type: &str, body: String) -> http::Response<Body> {
    let mut response = http::Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() =
        http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(value) = http::HeaderValue::from_str(content_type) {
        response
            .headers_mut()
            .insert(http::header::CONTENT_TYPE, value);
    }
    response
}

fn problem(status: u16, title: &str, detail: &str) -> http::Response<Body> {
    text(
        status,
        "application/problem+json",
        serde_json::json!({"status": status, "title": title, "detail": detail}).to_string(),
    )
}

/// The caller's bearer token, which only the host carries on to the gateway.
fn bearer(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// Which of the shard's listeners a request came in on (T-3857).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    /// The Apps' APIs, which the edge routes here.
    Apps,
    /// `/metrics` and `/jobs`, for the Portal and the scraper alone.
    Admin,
}

impl Shard {
    /// The App a path names: `/apps/{name}/api` or below.
    fn route(&self, path: &str) -> Option<Placed> {
        let rest = path.strip_prefix("/apps/")?;
        let (name, below) = rest.split_once('/')?;
        if below != "api" && !below.starts_with("api/") {
            return None;
        }
        self.apps.read().ok()?.get(name).cloned()
    }

    /// Serves `port` on `listener` until the process ends.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, port: Port) {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    tracing::warn!(%err, ?port, "accept failed");
                    continue;
                }
            };
            let shard = self.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let shard = shard.clone();
                    async move {
                        Ok::<_, Infallible>(match port {
                            Port::Apps => shard.handle(request).await,
                            Port::Admin => shard.handle_admin(request.uri().path()),
                        })
                    }
                });
                if let Err(err) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    tracing::debug!(%err, "connection ended");
                }
            });
        }
    }

    /// The admin port: the probe, the counters and the job records, nothing of an App's API.
    pub fn handle_admin(&self, path: &str) -> http::Response<Body> {
        match path {
            "/healthz" => text(200, "text/plain", "ok".into()),
            "/metrics" => text(
                200,
                "text/plain; version=0.0.4",
                self.metrics.render(&self.id, self.host.cache_stats()),
            ),
            "/jobs" => text(
                200,
                "application/json",
                records_json(&self.jobs.records()).to_string(),
            ),
            _ => problem(
                404,
                "Not Found",
                "The admin port serves /metrics and /jobs.",
            ),
        }
    }

    /// The apps port: `/healthz` and the placed Apps' APIs.
    pub async fn handle(&self, request: http::Request<Incoming>) -> http::Response<Body> {
        let path = request.uri().path().to_owned();
        if path == "/healthz" {
            return text(200, "text/plain", "ok".into());
        }
        let Some(app) = self.route(&path) else {
            return problem(
                404,
                "Not Found",
                "No application is served at this address.",
            );
        };
        let started = Instant::now();
        // Admitted before the body is read (T-3342).
        let admission = match self.host.admit(&app) {
            Ok(admission) => admission,
            Err(failure) => {
                self.metrics
                    .record(&app.name, Outcome::Busy, started.elapsed());
                return problem(failure.status(), "Application Error", failure.detail());
            }
        };
        let token = bearer(request.headers());
        let (parts, body) = request.into_parts();
        let limit = self.host.limits().request_bytes;
        let body = match Limited::new(body, limit).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => {
                self.metrics
                    .record(&app.name, Outcome::Failed, started.elapsed());
                return problem(413, "Payload Too Large", Failure::TooLarge.detail());
            }
        };
        let outcome = self
            .host
            .serve_admitted(
                &app,
                admission,
                http::Request::from_parts(parts, body),
                token,
            )
            .await;
        match outcome {
            Ok(response) => {
                self.metrics
                    .record(&app.name, Outcome::Answered, started.elapsed());
                let cap = self.host.limits().response_bytes;
                response.map(|body| capped(body, cap))
            }
            Err(failure) => {
                let counted = match failure {
                    Failure::Busy => Outcome::Busy,
                    Failure::Timeout => Outcome::Timeout,
                    _ => Outcome::Failed,
                };
                self.metrics.record(&app.name, counted, started.elapsed());
                if let Failure::Unavailable(why) | Failure::Failed(why) = &failure {
                    tracing::warn!(app = %app.id, %why, status = failure.status(), "request failed");
                }
                problem(failure.status(), "Application Error", failure.detail())
            }
        }
    }

    /// Reads the placement file again whenever it changes; a file that does not parse keeps the
    /// placement in force and says why.
    pub fn watch(self: &Arc<Self>, path: PathBuf) {
        let shard = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut seen = None;
            loop {
                let Some(shard) = shard.upgrade() else { return };
                let modified = tokio::fs::metadata(&path)
                    .await
                    .and_then(|m| m.modified())
                    .ok();
                if modified.is_some() && modified != seen {
                    seen = modified;
                    match Placement::read(&path, &shard.id) {
                        Ok(apps) => {
                            tracing::info!(apps = apps.len(), "placement read");
                            if let Ok(mut current) = shard.apps.write() {
                                *current = apps;
                            }
                        }
                        Err(why) => tracing::error!(%why, "the placement file was not taken"),
                    }
                }
                drop(shard);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::bearer;

    #[test]
    fn only_a_bearer_token_is_carried() {
        let mut headers = http::HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Basic eDp5"),
        );
        assert_eq!(bearer(&headers), None);
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer abc.def"),
        );
        assert_eq!(bearer(&headers).as_deref(), Some("abc.def"));
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer  "),
        );
        assert_eq!(bearer(&headers), None);
    }
}
