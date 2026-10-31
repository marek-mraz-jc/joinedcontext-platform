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

use crate::blob::S3Blob;
use crate::files::{MAX_FILE_BYTES, PREFIX};
use crate::host::{capped, Failure, Host};
use crate::jobs::{records_json, HostRunner, Scheduler};
use crate::metrics::{Metrics, Outcome};
use crate::placement::{Placed, Placement};
use crate::storage::BlobError;

pub type Body = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// What a shard serves: its runtime, its placement and its counters.
pub struct Shard {
    pub id: String,
    pub host: Arc<Host>,
    pub apps: RwLock<HashMap<String, Placed>>,
    pub metrics: Metrics,
    /// The scheduler of the placed Apps' jobs, whose records `/jobs` answers (AP-154).
    pub jobs: Arc<Scheduler<HostRunner>>,
    /// The Apps' files, which file links on the Apps' hosts reach (T-6195).
    pub files: Option<Arc<S3Blob>>,
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
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok();
    jc_core::http::bearer(value).map(str::to_owned)
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
        if let Some(encoded) = path
            .strip_prefix(&format!("/apps/{}/api/{PREFIX}", app.name))
            .map(str::to_owned)
        {
            return self.file(&app, &encoded, request).await;
        }
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

    /// A file link on the App's own host (T-6195): the link's signature, never the App, decides,
    /// and the bytes move between the caller and the App's prefix alone. A GET streams the
    /// object as an attachment; a PUT stores a body of the signed length, at most
    /// [`MAX_FILE_BYTES`].
    async fn file(
        &self,
        app: &Placed,
        encoded: &str,
        request: http::Request<Incoming>,
    ) -> http::Response<Body> {
        let Some((files, urls)) = self
            .files
            .as_ref()
            .and_then(|files| files.file_urls().map(|urls| (files, urls)))
        else {
            return problem(404, "Not Found", "This shard serves no file links.");
        };
        let method = request.method().clone();
        if method != http::Method::GET && method != http::Method::PUT {
            let mut refused = problem(
                405,
                "Method Not Allowed",
                "A file link is opened with GET, or with PUT for an upload.",
            );
            refused.headers_mut().insert(
                http::header::ALLOW,
                http::HeaderValue::from_static("GET, PUT"),
            );
            return refused;
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let query = request.uri().query().unwrap_or_default();
        let signed = match urls.verify(app, method.as_str(), encoded, query, now) {
            Ok(signed) => signed,
            Err(why) => {
                tracing::warn!(app = %app.id, kind = "file-link", "refused");
                return problem(403, "Forbidden", why);
            }
        };
        let admission = match self.host.admit(app) {
            Ok(admission) => admission,
            Err(failure) => {
                return problem(failure.status(), "Application Error", failure.detail())
            }
        };
        let answer = if method == http::Method::GET {
            download(files, app, &signed.key, admission).await
        } else {
            let answer = upload(files, app, signed, request).await;
            drop(admission);
            answer
        };
        answer.unwrap_or_else(|failed| *failed)
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

/// A file link's refusal, boxed: an answer is large beside the streamed one.
type Refusal = Box<http::Response<Body>>;

fn blob_problem(err: BlobError) -> Refusal {
    Box::new(match err {
        BlobError::NotFound => problem(404, "Not Found", "The application holds no such file."),
        BlobError::Refused(why) | BlobError::Quota(why) => problem(403, "Forbidden", &why),
        BlobError::Unavailable(why) => {
            tracing::warn!(%why, kind = "file-link", "the store failed");
            problem(
                503,
                "Service Unavailable",
                "The file store did not answer; try again in a minute.",
            )
        }
    })
}

/// The object streamed from the store, the admission held until its last byte.
async fn download(
    files: &S3Blob,
    app: &Placed,
    key: &str,
    admission: crate::host::Admission,
) -> Result<http::Response<Body>, Refusal> {
    let stored = files.download(app, key).await.map_err(blob_problem)?;
    match stored.status().as_u16() {
        200 => {}
        404 => return Err(blob_problem(BlobError::NotFound)),
        other => {
            return Err(blob_problem(BlobError::Unavailable(format!(
                "the object store answered {other}"
            ))))
        }
    }
    let content_type = stored
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| http::HeaderValue::from_bytes(v.as_bytes()).ok())
        .unwrap_or(http::HeaderValue::from_static("application/octet-stream"));
    let length = stored.content_length();
    let chunks = futures_util::stream::unfold(
        (stored, Some(admission)),
        |(mut stored, admission)| async move {
            admission.as_ref()?;
            match stored.chunk().await {
                Ok(Some(chunk)) => Some((Ok(hyper::body::Frame::data(chunk)), (stored, admission))),
                Ok(None) => None,
                Err(err) => Some((
                    Err(Box::new(err) as Box<dyn std::error::Error + Send + Sync>),
                    (stored, None),
                )),
            }
        },
    );
    let mut response = http::Response::new(http_body_util::StreamBody::new(chunks).boxed_unsync());
    let headers = response.headers_mut();
    headers.insert(http::header::CONTENT_TYPE, content_type);
    if let Some(length) = length.and_then(|n| http::HeaderValue::from_str(&n.to_string()).ok()) {
        headers.insert(http::header::CONTENT_LENGTH, length);
    }
    // The App's data, never a page of the App's origin.
    for (name, value) in [
        (http::header::CONTENT_DISPOSITION, "attachment"),
        (http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (http::header::CONTENT_SECURITY_POLICY, "sandbox"),
        (http::header::CACHE_CONTROL, "private, no-store"),
    ] {
        headers.insert(name, http::HeaderValue::from_static(value));
    }
    Ok(response)
}

/// A body of exactly the signed length (or, unsized, at most [`MAX_FILE_BYTES`]) into the store.
async fn upload(
    files: &S3Blob,
    app: &Placed,
    signed: crate::files::Signed,
    request: http::Request<Incoming>,
) -> Result<http::Response<Body>, Refusal> {
    let declared = request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if let (Some(signed), Some(declared)) = (signed.length, declared) {
        if signed != declared {
            return Err(Box::new(problem(
                403,
                "Forbidden",
                &format!("This upload link takes exactly {signed} bytes."),
            )));
        }
    }
    let content_type = request
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 255)
        .map(str::to_owned);
    let cap = signed.length.unwrap_or(MAX_FILE_BYTES).min(MAX_FILE_BYTES);
    let too_large = || {
        Box::new(problem(
            413,
            "Payload Too Large",
            &format!("A file link stores at most {cap} bytes."),
        ))
    };
    let cap = usize::try_from(cap).map_err(|_| too_large())?;
    let body = Limited::new(request.into_body(), cap)
        .collect()
        .await
        .map_err(|_| too_large())?
        .to_bytes();
    if signed
        .length
        .is_some_and(|length| length != body.len() as u64)
    {
        return Err(Box::new(problem(
            403,
            "Forbidden",
            &format!(
                "This upload link takes exactly {} bytes.",
                signed.length.unwrap_or_default()
            ),
        )));
    }
    files
        .upload(
            app,
            &signed.key,
            body.to_vec(),
            content_type,
            signed.length.is_some(),
        )
        .await
        .map_err(blob_problem)?;
    Ok(text(200, "text/plain", String::new()))
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
