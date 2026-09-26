//! Edge cases of `routes::forge::handler` (T-1867).
//!
//! The forge door is where a run writes code. It holds a token that may write the whole
//! configuration repository, so the door itself is the control: one operation of a named few, a
//! file path inside the run's own application directory, a branch that is the run's own, and a
//! commit that carries the person who proposed it.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases, RUN_ID, TICKET};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The application directory of the run in `common::sample_run`.
const INSIDE: &str = "projects/helsinki/apps/bikes";

fn proxy(forge: &str) -> axum::Router {
    app(
        sample_run(true),
        Bases {
            forge: forge.to_owned(),
            ..Bases::default()
        },
    )
}

/// The same proxy, asking a Portal that holds no run and answers `404` to every lookup.
///
/// `Bases::default()` points the Portal at a name that resolves nowhere, which since T-2418 is a
/// failure of the platform's (`503`) rather than a bad credential (`401`). A case about a run id
/// nobody holds has to ask a Portal that is there.
fn proxy_asking(forge: &str, portal: &MockServer) -> axum::Router {
    app(
        sample_run(true),
        Bases {
            forge: forge.to_owned(),
            portal: portal.uri(),
            ..Bases::default()
        },
    )
}

async fn forge_answering(status: u16, body: &str) -> MockServer {
    let forge = MockServer::start().await;
    for verb in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&forge)
            .await;
    }
    forge
}

/// What the forge was handed, as JSON.
async fn last_sent(forge: &MockServer) -> serde_json::Value {
    let seen = forge.received_requests().await.expect("the mock recorded");
    serde_json::from_slice(&seen.last().expect("a request reached the forge").body)
        .expect("the forge is handed JSON")
}

/// AG-22: a caller with no run credentials writes nothing to the repository.
#[tokio::test]
async fn a_write_without_credentials_is_refused_before_the_forge_is_asked() {
    let forge = MockServer::start().await;
    let response = proxy(&forge.uri())
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(format!("/v1/forge/contents/{INSIDE}/src/main.rs"))
                .body(Body::from(r#"{"content":"eA==","message":"add"}"#))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(forge
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// AG-22: a ticket of another run writes nothing.
#[tokio::test]
async fn a_ticket_of_another_run_writes_nothing() {
    let portal = MockServer::start().await;
    for (run, ticket) in [(RUN_ID, "another-runs-ticket"), ("nobody", TICKET)] {
        let response = proxy_asking("http://gitea.invalid:3000", &portal)
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(format!("/v1/forge/contents/{INSIDE}/src/main.rs"))
                    .header("x-jc-run", run)
                    .header("x-jc-ticket", ticket)
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "run '{run}'");
    }
}

/// AG-64: the door knows a named few operations. Anything else is refused before a request
/// leaves the proxy, so the forge token cannot be spent on the rest of the forge's API.
#[tokio::test]
async fn an_operation_the_door_does_not_know_is_refused() {
    let forge = forge_answering(200, "{}").await;
    for (verb, rest) in [
        ("GET", "contents"),
        ("POST", "issues"),
        ("DELETE", "branches"),
        ("GET", "branches"),
        ("PATCH", "pulls"),
        ("POST", "pulls/1/merge"),
        ("POST", "releases"),
        ("GET", "collaborators"),
        ("POST", "commits/abc"),
        ("PUT", "admin/users"),
        ("GET", ""),
    ] {
        let response = proxy(&forge.uri())
            .oneshot(
                authed(verb, &format!("/v1/forge/{rest}"))
                    .body(Body::from("{}"))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert!(
            !response.status().is_success(),
            "{verb} '{rest}' answered {}",
            response.status()
        );
    }
    assert!(
        forge
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing reached the forge"
    );
}

/// AG-64, T-0817: a file path outside the run's own application directory is refused — a
/// neighbour that shares the prefix's letters, a traversal in any spelling, an absolute path.
#[tokio::test]
async fn a_file_outside_the_runs_application_directory_is_refused() {
    let forge = forge_answering(200, "{}").await;
    for file in [
        "projects/helsinki/apps/bikes-other/src/main.rs",
        "projects/helsinki/apps/bikesX",
        "projects/oulu/apps/bikes/src/main.rs",
        "secrets/values.yaml",
        &format!("{INSIDE}/../../../../secrets/values.yaml"),
        &format!("{INSIDE}/%2e%2e/x"),
        &format!("{INSIDE}//x"),
        &format!("/{INSIDE}/x"),
        "",
    ] {
        let response = proxy(&forge.uri())
            .oneshot(
                authed("PUT", &format!("/v1/forge/contents/{file}"))
                    .body(Body::from(r#"{"content":"eA==","message":"add"}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert!(
            !response.status().is_success(),
            "'{file}' answered {}",
            response.status()
        );
    }
    assert!(
        forge
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing reached the forge"
    );
}

/// AP-100: a run reads and writes no workflow, wherever it would sit: the build is the Portal's,
/// so a run cannot rewrite what the forge runs, and nothing of such a request reaches the forge.
#[tokio::test]
async fn a_run_never_touches_a_workflow() {
    let forge = forge_answering(200, "{}").await;
    for (verb, file) in [
        ("PUT", format!("{INSIDE}/.gitea/workflows/build.yml")),
        ("POST", format!("{INSIDE}/.gitea/actions/x.yml")),
        ("DELETE", format!("{INSIDE}/src/.gitea/workflows/build.yml")),
        ("PUT", ".gitea/workflows/build.yml".to_owned()),
    ] {
        let response = proxy(&forge.uri())
            .oneshot(
                authed(verb, &format!("/v1/forge/contents/{file}"))
                    .body(Body::from(r#"{"content":"eA==","message":"build"}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{verb} {file}");
    }
    let nested = proxy(&forge.uri())
        .oneshot(
            authed(
                "PUT",
                &format!("/v1/forge/contents/{INSIDE}/.gitea/workflows/build.yml"),
            )
            .body(Body::from(r#"{"content":"eA==","message":"build"}"#))
            .expect("a request"),
        )
        .await
        .expect("an answer");
    assert!(body_of(nested).await.contains("AP-100"));
    assert!(
        forge
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "nothing reached the forge"
    );
}

/// AG-64: a write inside the run's directory carries the run's branch, the agent as author and
/// committer, and the person who proposed it — none of which the workspace can set for itself.
#[tokio::test]
async fn a_write_carries_the_runs_branch_and_the_person_who_proposed_it() {
    let forge = forge_answering(201, "{}").await;
    let response = proxy(&forge.uri())
        .oneshot(
            authed("PUT", &format!("/v1/forge/contents/{INSIDE}/src/main.rs"))
                .body(Body::from(
                    serde_json::json!({
                        "content": "eA==",
                        "message": "add main",
                        "branch": "main",
                        "author": { "name": "somebody else", "email": "nobody@evil.test" },
                        "committer": { "name": "somebody else", "email": "nobody@evil.test" }
                    })
                    .to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::CREATED);

    let sent = last_sent(&forge).await;
    assert_eq!(sent["branch"], format!("agent/app-bikes/{RUN_ID}"));
    assert_eq!(sent["author"]["name"], "agent:app-builder@helsinki");
    assert_eq!(sent["committer"]["name"], "agent:app-builder@helsinki");
    assert_eq!(
        sent["message"],
        "add main\n\nCo-Proposed-By: demo.steward@hel.fi"
    );
}

/// AG-64: the trailer is added once. A message that already carries it is left as it is, so a
/// retry does not stack the line.
#[tokio::test]
async fn the_proposer_trailer_is_added_once() {
    let forge = forge_answering(200, "{}").await;
    let message = "add main\n\nCo-Proposed-By: demo.steward@hel.fi";
    let response = proxy(&forge.uri())
        .oneshot(
            authed("PUT", &format!("/v1/forge/contents/{INSIDE}/src/main.rs"))
                .body(Body::from(
                    serde_json::json!({ "content": "eA==", "message": message }).to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(last_sent(&forge).await["message"], message);
}

/// AG-64: a delete inside the directory is a write too, so it carries the same branch and the
/// same author.
#[tokio::test]
async fn a_delete_carries_the_runs_branch_as_a_write_does() {
    let forge = forge_answering(200, "{}").await;
    let response = proxy(&forge.uri())
        .oneshot(
            authed("DELETE", &format!("/v1/forge/contents/{INSIDE}/src/old.rs"))
                .body(Body::from(
                    serde_json::json!({ "sha": "abc", "message": "drop", "branch": "main" })
                        .to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        last_sent(&forge).await["branch"],
        format!("agent/app-bikes/{RUN_ID}")
    );
}

/// AG-64: a run creates its own branch and no other, whatever it asks for.
#[tokio::test]
async fn a_run_creates_its_own_branch_and_no_other() {
    let forge = forge_answering(201, "{}").await;
    for (name, expected) in [
        ("main", StatusCode::FORBIDDEN),
        ("agent/app-bikes/another-run", StatusCode::FORBIDDEN),
        ("", StatusCode::FORBIDDEN),
        (&format!("agent/app-bikes/{RUN_ID}"), StatusCode::CREATED),
    ] {
        let response = proxy(&forge.uri())
            .oneshot(
                authed("POST", "/v1/forge/branches")
                    .body(Body::from(
                        serde_json::json!({ "new_branch_name": name }).to_string(),
                    ))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), expected, "branch '{name}'");
    }
}

/// AG-64: a pull request is opened from the run's own branch, whatever head the workspace names.
#[tokio::test]
async fn a_pull_request_is_opened_from_the_runs_own_branch() {
    let forge = forge_answering(201, "{}").await;
    let response = proxy(&forge.uri())
        .oneshot(
            authed("POST", "/v1/forge/pulls")
                .body(Body::from(
                    serde_json::json!({
                        "title": "add bikes",
                        "base": "main",
                        "head": "somebody-elses-branch"
                    })
                    .to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        last_sent(&forge).await["head"],
        format!("agent/app-bikes/{RUN_ID}")
    );
}

/// AG-64: reading a pull request or a commit is allowed, so a run can see what became of what it
/// proposed.
#[tokio::test]
async fn reading_a_pull_request_or_a_commit_is_allowed() {
    let forge = forge_answering(200, "{}").await;
    for rest in ["pulls/1", "pulls/1/files", "commits/abc123"] {
        let response = proxy(&forge.uri())
            .oneshot(
                authed("GET", &format!("/v1/forge/{rest}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "GET {rest}");
    }
}

/// AG-86, T-3021: a read of a pull request or a commit stays inside the run's repository. The
/// target is parsed with its dot segments resolved, so without a check `pulls/../../other/repo`
/// would reach another repository, or any forge GET, with the platform's token.
#[tokio::test]
async fn a_read_that_would_leave_the_runs_repository_is_refused() {
    let forge = forge_answering(200, "{}").await;
    for rest in [
        "pulls/../../other/repo/contents/secrets.yaml",
        "pulls/1/../../../../../admin/users",
        "commits/../../configuration/raw/values.yaml",
        "commits/%2e%2e/%2e%2e/other/repo",
        "pulls/%2F..%2F..%2Fother",
        "pulls/1/..",
        "pulls//1",
        "commits/a\\..\\b",
    ] {
        let response = proxy(&forge.uri())
            .oneshot(
                authed("GET", &format!("/v1/forge/{rest}"))
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "GET {rest}");
    }
    let reached: Vec<String> = forge
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect();
    assert!(reached.is_empty(), "the forge was asked for {reached:?}");
}

/// AG-35: the forge is told the platform's token and the workspace is told none of it, whatever
/// the forge answers.
#[tokio::test]
async fn the_forge_token_is_presented_upstream_and_never_relayed_back() {
    for status in [200u16, 401, 403, 404, 409, 422, 500] {
        let forge = forge_answering(status, r#"{"message":"answered"}"#).await;
        let host = forge.address().to_string();

        let response = proxy(&forge.uri())
            .oneshot(
                authed("GET", "/v1/forge/pulls/1")
                    .header("authorization", "Bearer the-workspaces-own-token")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status().as_u16(), status);

        let seen = forge.received_requests().await.expect("the mock recorded");
        assert_eq!(
            seen.first()
                .expect("a request")
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("token mock-forge-token"),
            "the forge is told the platform's token"
        );

        let body = body_of(response).await;
        assert!(
            !body.contains("mock-forge-token"),
            "the token leaked: {body}"
        );
        assert!(!body.contains(&host), "the forge's address leaked: {body}");
    }
}

/// AG-41, T-0811: a file past the proxy's ceiling is refused, and the refusal costs the forge
/// nothing.
#[tokio::test]
async fn a_body_past_the_ceiling_is_refused() {
    let forge = forge_answering(200, "{}").await;
    let response = proxy(&forge.uri())
        .oneshot(
            authed("PUT", &format!("/v1/forge/contents/{INSIDE}/big.bin"))
                .body(Body::from(vec![b'x'; 4 * 1024 * 1024 + 1]))
                .expect("a request"),
        )
        .await
        .expect("an answer");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(forge
        .received_requests()
        .await
        .unwrap_or_default()
        .is_empty());
}

/// A run of `helsinki` in layout 2: its folder at the root of the project's own repository.
fn project_repository_proxy(forge: &str, repository: &str) -> axum::Router {
    app(
        agent_proxy::runs::RunContext {
            repository: Some(repository.to_owned()),
            path_prefix: "apps/bikes/".to_owned(),
            ..sample_run(true)
        },
        Bases {
            forge: forge.to_owned(),
            ..Bases::default()
        },
    )
}

/// Every path the forge was asked for.
async fn paths_asked(forge: &MockServer) -> Vec<String> {
    forge
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect()
}

/// AG-86, CC-87: in layout 2 a run's write and its reads reach its project's repository, in the
/// configuration repository's owner, and nothing of the configuration repository.
#[tokio::test]
async fn a_run_of_layout_two_reaches_its_project_repository_alone() {
    let forge = forge_answering(200, "{}").await;
    let proxy = project_repository_proxy(&forge.uri(), "helsinki");
    for (verb, uri, body) in [
        (
            "PUT",
            "/v1/forge/contents/apps/bikes/src/main.rs",
            r#"{"content":"eA==","message":"add"}"#,
        ),
        ("GET", "/v1/forge/pulls/7", ""),
    ] {
        let response = proxy
            .clone()
            .oneshot(authed(verb, uri).body(Body::from(body)).expect("a request"))
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK, "{verb} {uri}");
    }
    let asked = paths_asked(&forge).await;
    assert_eq!(
        asked,
        [
            "/api/v1/repos/joinedcontext/helsinki/contents/apps/bikes/src/main.rs",
            "/api/v1/repos/joinedcontext/helsinki/pulls/7",
        ],
        "{asked:?}"
    );
}

/// AG-86: the folder is the one of the project's repository, so the configuration repository's
/// spelling of it, or another project's, is outside and refused before the forge is asked.
#[tokio::test]
async fn a_run_of_layout_two_writes_nothing_outside_its_folder() {
    let forge = forge_answering(200, "{}").await;
    let proxy = project_repository_proxy(&forge.uri(), "helsinki");
    for path in [
        "projects/helsinki/apps/bikes/src/main.rs",
        "projects/espoo/apps/bikes/src/main.rs",
        "project.yaml",
    ] {
        let response = proxy
            .clone()
            .oneshot(
                authed("PUT", &format!("/v1/forge/contents/{path}"))
                    .body(Body::from(r#"{"content":"eA==","message":"add"}"#))
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert!(paths_asked(&forge).await.is_empty());
}

/// AG-86: a repository name that would climb out of the owner, or name a path, is no repository
/// this door reaches.
#[tokio::test]
async fn a_repository_name_that_is_a_path_is_refused() {
    let forge = forge_answering(200, "{}").await;
    for name in ["../configuration", "other/helsinki", "", "helsinki%2F.."] {
        let response = project_repository_proxy(&forge.uri(), name)
            .oneshot(
                authed("GET", "/v1/forge/pulls/7")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{name:?}");
    }
    assert!(paths_asked(&forge).await.is_empty());
}
