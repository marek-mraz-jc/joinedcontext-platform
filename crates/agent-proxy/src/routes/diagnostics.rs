//! The diagnostics door (AG-57): what a run may read about a resource of its own project when
//! a step failed, fetched by the Portal and redacted before the workspace sees it (AG-56).

use crate::audit::{log_request, AuditEntry};
use crate::auth::authenticate;
use crate::routes::portal_bearer;
use crate::ProxyState;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::LazyLock;
use std::time::Instant;

/// The components the door knows; anything else is refused before a request leaves the proxy.
const COMPONENTS: [&str; 2] = ["pipeline", "change"];

/// A resource name (DNS-1123) or a change id (`chg-` and eight hex digits).
fn is_name(id: &str) -> bool {
    let dns = id.len() <= 63
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-');
    let change =
        id.len() == 12 && id.starts_with("chg-") && id[4..].bytes().all(|b| b.is_ascii_hexdigit());
    !id.is_empty() && (dns || change)
}

/// What a body must not carry to the workspace (AG-40, AG-56): a bearer, a token-shaped
/// header value, a named secret's value, the credentials of a connection URI, a JWT, and a
/// credential no field name labelled but whose issuer's prefix names it anyway (T-0957).
static SECRETS: LazyLock<Vec<(regex::Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (
            regex::Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("a literal pattern"),
            "Bearer [REDACTED]",
        ),
        (
            regex::Regex::new(r#"(?i)\b(authorization|cookie|set-cookie)(\\?["']?\s*[:=]\s*\\?["']?)[^"'\r\n\\]+"#)
                .expect("a literal pattern"),
            "$1$2[REDACTED]",
        ),
        (
            regex::Regex::new(
                r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|client[_-]?secret)(\\?["']?\s*[:=]\s*\\?["']?)[^"'\s,;&\\]+"#,
            )
            .expect("a literal pattern"),
            "$1$2[REDACTED]",
        ),
        (
            regex::Regex::new(r#"([a-z][a-z0-9+.-]*://[^\s/:@"']+:)[^\s@"']+@"#).expect("a literal pattern"),
            "$1[REDACTED]@",
        ),
        (
            regex::Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}")
                .expect("a literal pattern"),
            "[REDACTED]",
        ),
        // A credential nothing labelled, recognised by the prefix its issuer gives it
        // (T-0957, AG-40). Only prefixes that mean one thing: a rule for "anything long and
        // alphanumeric" would take the commit shas, branch names and entity URNs a change's
        // own diagnostics are made of, and hand the workspace back a document with the answer
        // removed. What is not prefixed is still covered where it is named.
        (
            // AWS: the four-letter type prefix and sixteen upper-case characters.
            regex::Regex::new(r"\b(?:AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b").expect("a literal pattern"),
            "[REDACTED]",
        ),
        (
            // GitHub, both shapes: the classic `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_` token and
            // the fine-grained `github_pat_`.
            regex::Regex::new(r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})")
                .expect("a literal pattern"),
            "[REDACTED]",
        ),
        (
            // `sk-` keys, the shape OpenAI, OpenRouter and Anthropic-compatible clients use.
            regex::Regex::new(r"\bsk-(?:or-|ant-|proj-|live-)?[A-Za-z0-9_-]{20,}")
                .expect("a literal pattern"),
            "[REDACTED]",
        ),
    ]
});

/// The text with every secret-shaped run replaced; the shape of the document stays.
pub fn redact(text: &str) -> String {
    SECRETS
        .iter()
        .fold(text.to_owned(), |acc, (pattern, replacement)| {
            pattern.replace_all(&acc, *replacement).into_owned()
        })
}

pub async fn handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Path((component, id)): Path<(String, String)>,
) -> Response {
    let start = Instant::now();
    let run = match authenticate(&headers, &state.runs, &state.config).await {
        Ok(r) => r,
        Err(p) => return (*p).into_response(),
    };

    if !COMPONENTS.contains(&component.as_str()) {
        return jc_core::ProblemDetails::new(
            400,
            "bad-request",
            format!("the diagnostics door knows no component '{component}'"),
        )
        .into_response();
    }
    if !is_name(&id) {
        return jc_core::ProblemDetails::new(
            400,
            "bad-request",
            "a diagnostics id is a resource name or a change id",
        )
        .into_response();
    }

    if let Err(msg) = state
        .limits
        .check_rpm(&run.id, run.requests_per_minute)
        .await
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            jc_core::ProblemDetails::new(429, "too-many-requests", msg),
        )
            .into_response();
    }

    let path = format!(
        "internal/agent-runs/{}/diagnostics/{component}/{id}",
        run.id
    );
    let mut url = state.config.portal_base.clone();
    url.set_path(&path);

    let bearer = match portal_bearer(&state.credentials).await {
        Ok(token) => token,
        Err(response) => return *response,
    };
    let resp = state.http.get(url).bearer_auth(&bearer).send().await;

    let (status, body) = match resp {
        Ok(r) => {
            let s = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let text = r.text().await.unwrap_or_default();
            // The profile's response cap holds here as on every other door (AG-41).
            let cut = text
                .char_indices()
                .nth(usize::try_from(run.max_response_bytes).unwrap_or(usize::MAX))
                .map_or(text.len(), |(i, _)| i);
            (s, redact(&text[..cut]))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, redact(&e.to_string())),
    };

    log_request(&AuditEntry {
        run_id: &run.id,
        user: &run.created_by,
        upstream: "portal",
        method: "GET",
        path: &path,
        status: status.as_u16(),
        bytes: body.len(),
        duration_ms: start.elapsed().as_millis(),
    });

    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::{is_name, redact};

    #[test]
    fn names_and_change_ids_pass_and_paths_do_not() {
        for ok in ["hsl-bikes", "a", "chg-0000000a", "x9"] {
            assert!(is_name(ok), "{ok}");
        }
        for bad in [
            "",
            "../x",
            "a/b",
            "Hsl",
            "-a",
            "a-",
            "chg-XYZ",
            "chg-0000000A0",
            "a b",
        ] {
            assert!(!is_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_bearer_a_named_secret_a_uri_credential_and_a_jwt_are_redacted() {
        // The secret-shaped values are assembled here, so the source holds no string a secret
        // scanner would take for a real one.
        let key = ["sk", "live", "0123456789"].join("-");
        let jwt = [
            "eyJhbGciOiJIUzI1NiJ9",
            "eyJzdWIiOiIxIn0",
            "abcdefghijklmnop",
        ]
        .join(".");
        let body = format!(
            r#"{{"authorization":"Bearer abcdefghijklmnop.qrstuvwxyz","error":"connect postgresql://jc:s3cr3t-pw@db:5432/jc failed","apiKey":"{key}","password": "hunter2","token":"{jwt}", "received": 42}}"#
        );
        let out = redact(&body);
        assert!(!out.contains("abcdefghijklmnop.qrstuvwxyz"), "{out}");
        assert!(!out.contains("s3cr3t-pw"), "{out}");
        assert!(!out.contains(&key), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(!out.contains(&jwt), "{out}");
        assert!(
            out.contains("postgresql://jc:[REDACTED]@db:5432/jc"),
            "{out}"
        );
        assert!(
            out.contains(r#""received": 42"#),
            "the shape of the document stays: {out}"
        );
    }

    /// T-0957, AG-40: a secret does not stop being one because nothing labelled it. A vendor
    /// prefix is the label — `AKIA`, `ghp_`, `sk-` mean one thing and never occur in prose —
    /// so a key pasted into a stack trace is redacted as a labelled one is.
    #[test]
    fn a_secret_with_no_field_name_is_redacted_by_its_own_prefix() {
        // Assembled, so this source holds no string a secret scanner would take for a real one.
        let aws = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let github = format!("ghp_{}", "abc123def456ghi789jkl012");
        let fine_grained = format!(
            "github_pat_{}",
            "11ABCDEFG0aBcDeFgHiJkL_mNoPqRsTuVwXyZ0123456789"
        );
        let openai = format!("sk-{}", "proj0123456789abcdefghij");
        let body = format!(
            "traceback: the deploy step used {aws} and {github}; the mirror used \
             {fine_grained}; the model call used {openai} and was refused"
        );

        let out = redact(&body);
        for secret in [&aws, &github, &fine_grained, &openai] {
            assert!(!out.contains(secret.as_str()), "{secret} survived: {out}");
        }
        assert!(out.contains("the deploy step used"), "{out}");
        assert!(out.contains("and was refused"), "{out}");
    }

    /// The other half: the change component's own diagnostics are full of commit shas and
    /// branch names, and a rule that redacted "anything long and alphanumeric" would hand back
    /// a document with the answer removed.
    #[test]
    fn what_a_change_is_made_of_is_not_a_secret() {
        let body = "merge of 9f1c2b7d4e6a8c0b2d4f6a8c0e2b4d6f8a0c2e4b into main failed: \
                    /commits/abc123def4567890abcdef1234567890abcdef12 is not an ancestor, \
                    branch agent/app-bikes/e3b0c442-98fc-1c14-9afb-4c7b2756a120 is behind";
        assert_eq!(redact(body), body);
    }

    #[test]
    fn plain_counters_pass_untouched() {
        let body = r#"{"pipeline":"hsl-bikes","received":1200,"sent":1190,"errors":10}"#;
        assert_eq!(redact(body), body);
    }
}
