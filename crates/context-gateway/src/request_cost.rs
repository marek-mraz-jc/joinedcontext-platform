//! What one request cost the broker, per Organization (T-6616, API/00 §6a).
//!
//! The broker measures a request when `ANTARES_REQUEST_COST=on` and says so in two headers:
//! `Server-Timing` (`app`, `db`, `pool`, `total`, in milliseconds) and `JC-Request-Cost`
//! (`cpu-ms`, `db-ms`, `statements`, `rows`, an RFC 8941 dictionary). The broker knows the
//! Tenant; the gateway knows who called, so the gateway totals the figures per Organization.
//!
//! Off by default: both headers are taken off every answer and nothing is counted. `metrics`
//! counts and still takes them off. `header` counts and hands them on, with the gateway's own
//! `gw` share, to a verified caller of the Endpoint's own Organization only: the broker counts
//! the rows it read before a Policy narrowed them, which tells an outsider how many it hid.

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::app::Gateway;
use crate::auth::subject::{authenticate_on, group_projects, Subject};

/// The broker's count of a request's statements and rows (T-6615).
pub const REQUEST_COST: HeaderName = HeaderName::from_static("jc-request-cost");
/// The W3C Server Timing header the broker writes its durations in.
pub const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

/// The label of an anonymous caller.
pub const PUBLIC: &str = "public";
/// The label of a verified caller no Organization of the repository holds: a data space
/// participant, a preview reader, a person whose groups name no project with an Organization.
pub const UNATTRIBUTED: &str = "unattributed";

/// `JC_GATEWAY_REQUEST_COST`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Neither header reaches the caller and nothing is counted (the default).
    #[default]
    Off,
    /// Counted per Organization and Endpoint; neither header reaches the caller.
    Metrics,
    /// Counted, and handed to a verified caller of the Endpoint's own Organization.
    Header,
}

impl Mode {
    /// Unset or empty is `off`; anything but the three words is an error, so a typo never
    /// runs as a setting nobody chose.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("") | Some("off") => Ok(Self::Off),
            Some("metrics") => Ok(Self::Metrics),
            Some("header") => Ok(Self::Header),
            Some(other) => Err(format!("`{other}` is none of `off`, `metrics`, `header`")),
        }
    }
}

/// What the answer's caller has to be told apart by: the path and headers of the request,
/// kept only while the cost is counted.
pub(crate) struct Asked {
    path: String,
    headers: HeaderMap,
}

impl Asked {
    /// The request's path and headers, `None` while the mode is `off`.
    pub(crate) fn of(gateway: &Gateway, request: &Request) -> Option<Self> {
        (gateway.request_cost != Mode::Off).then(|| Self {
            path: request.uri().path().to_owned(),
            headers: request.headers().clone(),
        })
    }
}

/// The broker's figures for one request.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Figures {
    app_ms: f64,
    db_ms: f64,
    pool_ms: f64,
    statements: u64,
    rows: u64,
}

impl Figures {
    /// The figures of the broker's two headers, `None` without a `JC-Request-Cost` the gateway
    /// can read: an answer the broker did not measure adds nothing.
    pub(crate) fn read(timing: &[HeaderValue], cost: &[HeaderValue]) -> Option<Self> {
        let mut figures = Self::default();
        let mut measured = false;
        for (key, value) in members(cost) {
            measured = true;
            match key {
                "statements" => figures.statements = value.parse().ok()?,
                "rows" => figures.rows = value.parse().ok()?,
                _ => {}
            }
        }
        if !measured {
            return None;
        }
        for entry in timing
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
        {
            let mut parts = entry.split(';').map(str::trim);
            let name = parts.next().unwrap_or_default();
            let dur = parts
                .filter_map(|param| param.strip_prefix("dur="))
                .find_map(|ms| ms.parse::<f64>().ok())
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
                .unwrap_or_default();
            match name {
                "app" => figures.app_ms = dur,
                "db" => figures.db_ms = dur,
                "pool" => figures.pool_ms = dur,
                _ => {}
            }
        }
        Some(figures)
    }
}

/// The `key=value` members of an RFC 8941 dictionary spread over `values`.
fn members(values: &[HeaderValue]) -> impl Iterator<Item = (&str, &str)> {
    values
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|member| member.trim().split_once('='))
}

/// Takes the broker's cost headers off `headers`, counts them when the mode asks, and puts them
/// back for a caller `header` mode hands them to. True when it put them back, so the caller's
/// `Server-Timing` also gets the gateway's own `gw` share.
pub(crate) fn settle(
    gateway: &Gateway,
    asked: Option<Asked>,
    headers: &mut HeaderMap,
    endpoint: &str,
) -> bool {
    let timing = take(headers, &SERVER_TIMING);
    let cost = take(headers, &REQUEST_COST);
    let (Some(asked), Some(figures)) = (asked, Figures::read(&timing, &cost)) else {
        return false;
    };
    let (organization, own) = caller_of(gateway, &asked);
    count(&organization, endpoint, &figures);
    if gateway.request_cost != Mode::Header || !own {
        return false;
    }
    for value in timing {
        headers.append(SERVER_TIMING, value);
    }
    for value in cost {
        headers.append(REQUEST_COST, value);
    }
    true
}

fn take(headers: &mut HeaderMap, name: &HeaderName) -> Vec<HeaderValue> {
    match headers.entry(name) {
        axum::http::header::Entry::Occupied(entry) => entry.remove_entry_mult().1.collect(),
        axum::http::header::Entry::Vacant(_) => Vec::new(),
    }
}

/// The Organization the request is counted under, and whether it is the Endpoint's own.
///
/// Only from the caller the gateway verifies on the Endpoint (ADR-N-067): a header or a URN
/// names nothing here (ADR-N-041). A workload counts under its ServiceAccount's project's
/// Organization; a person under the Endpoint's Organization when their verified `jc_org` names
/// it, else under the first Organization of the projects their groups name.
fn caller_of(gateway: &Gateway, asked: &Asked) -> (String, bool) {
    let Some(endpoint) = endpoint_of(gateway, &asked.path) else {
        return (UNATTRIBUTED.to_owned(), false);
    };
    // `federated` only widens the audiences to the broker's exchanged token: the request was
    // admitted already, and the label still comes from a verified token.
    let Ok(subject) = authenticate_on(gateway, &endpoint, &asked.headers, true) else {
        return (UNATTRIBUTED.to_owned(), false);
    };
    if anonymous(&subject) {
        return (PUBLIC.to_owned(), false);
    }
    let home = gateway.organization_of(&endpoint.project);
    let organization = if subject.service_account.is_some() {
        subject.organization.clone()
    } else if subject.user.is_some() && subject.member_of_organization {
        home.clone()
    } else if subject.user.is_some() {
        group_projects(subject.groups.iter().map(String::as_str))
            .iter()
            .filter_map(|project| gateway.organization_of(project))
            .min()
    } else {
        None
    };
    match organization {
        Some(organization) => {
            let own = home.as_deref() == Some(organization.as_str());
            (organization, own)
        }
        None => (UNATTRIBUTED.to_owned(), false),
    }
}

fn anonymous(subject: &Subject) -> bool {
    subject.user.is_none() && subject.service_account.is_none() && subject.did.is_none()
}

/// The Endpoint a path names: `/api/endpoint/{slug}/…` or a space's `/cs/{space}/…`.
fn endpoint_of(gateway: &Gateway, path: &str) -> Option<std::sync::Arc<crate::resolver::Endpoint>> {
    let name = crate::telemetry::named(path)?;
    if path.starts_with("/cs/") {
        gateway
            .resolver
            .resolve_space(name)
            .map(|space| std::sync::Arc::clone(&space.endpoint))
    } else {
        gateway.resolver.resolve(name)
    }
}

/// Adds one request's figures to its Organization's and Endpoint's counters. Both labels are
/// names the repository declares: no person, token, URN or query text (T-5701).
fn count(organization: &str, endpoint: &str, figures: &Figures) {
    let labels = [
        ("organization", organization.to_owned()),
        ("endpoint", endpoint.to_owned()),
    ];
    let micros = |ms: f64| (ms * 1000.0).round() as u64;
    metrics::counter!("jc_gateway_request_cost_requests_total", &labels).increment(1);
    metrics::counter!("jc_gateway_request_cost_app_microseconds_total", &labels)
        .increment(micros(figures.app_ms));
    metrics::counter!("jc_gateway_request_cost_db_microseconds_total", &labels)
        .increment(micros(figures.db_ms));
    metrics::counter!("jc_gateway_request_cost_pool_microseconds_total", &labels)
        .increment(micros(figures.pool_ms));
    metrics::counter!("jc_gateway_request_cost_statements_total", &labels)
        .increment(figures.statements);
    metrics::counter!("jc_gateway_request_cost_rows_total", &labels).increment(figures.rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(texts: &[&str]) -> Vec<HeaderValue> {
        texts
            .iter()
            .map(|t| HeaderValue::from_str(t).expect("a header value"))
            .collect()
    }

    #[test]
    fn the_mode_is_off_unless_named() {
        assert_eq!(Mode::parse(None), Ok(Mode::Off));
        assert_eq!(Mode::parse(Some(" ")), Ok(Mode::Off));
        assert_eq!(Mode::parse(Some("metrics")), Ok(Mode::Metrics));
        assert_eq!(Mode::parse(Some("header")), Ok(Mode::Header));
        assert!(Mode::parse(Some("on")).is_err());
    }

    #[test]
    fn the_broker_figures_are_read_from_both_headers() {
        let figures = Figures::read(
            &values(&["app;dur=2.5, db;dur=1.3, pool;dur=0.4, total;dur=3.2"]),
            &values(&["cpu-ms=2.463, db-ms=1.291, statements=3, rows=5"]),
        );
        assert_eq!(
            figures,
            Some(Figures {
                app_ms: 2.5,
                db_ms: 1.3,
                pool_ms: 0.4,
                statements: 3,
                rows: 5
            })
        );
    }

    #[test]
    fn an_answer_the_broker_did_not_measure_adds_nothing() {
        // The gateway's own or another hop's Server-Timing alone is no broker measure.
        assert_eq!(Figures::read(&values(&["app;dur=2.5"]), &[]), None);
        assert_eq!(Figures::read(&[], &values(&["statements=many"])), None);
        let negative = Figures::read(&values(&["db;dur=-4, app;dur=NaN"]), &values(&["rows=1"]))
            .expect("measured");
        assert_eq!(
            (negative.db_ms, negative.app_ms, negative.rows),
            (0.0, 0.0, 1)
        );
    }
}
