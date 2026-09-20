mod common;

use common::*;
use jcctl::apisix::{render, Settings, EDGE_CLIENT_SECRET, END_MARKER, OIDC_SESSION_SECRET};
use serde_json::Value;

fn settings() -> Settings {
    Settings::new("city.example.com", "prod", "prod")
}

fn rendered(dir: &std::path::Path) -> String {
    render(&load(dir), &settings())
}

fn parsed(yaml: &str) -> Value {
    serde_norway::from_str(yaml).expect("the rendered configuration is valid YAML")
}

fn document(dir: &std::path::Path) -> Value {
    parsed(rendered(dir).trim_end_matches("#END\n"))
}

fn find<'a>(document: &'a Value, section: &str, id: &str) -> &'a Value {
    document[section]
        .as_array()
        .unwrap_or_else(|| panic!("{section} is an array"))
        .iter()
        .find(|item| item["id"] == id)
        .unwrap_or_else(|| panic!("{section} has no {id}"))
}

fn has(document: &Value, section: &str, id: &str) -> bool {
    document[section]
        .as_array()
        .unwrap_or_else(|| panic!("{section} is an array"))
        .iter()
        .any(|item| item["id"] == id)
}

/// The plugin table of a route, inline or through its shared plugin config.
fn plugins_of<'a>(document: &'a Value, route_id: &str) -> &'a Value {
    let route = find(document, "routes", route_id);
    match route["plugin_config_id"].as_str() {
        Some(config) => &find(document, "plugin_configs", config)["plugins"],
        None => &route["plugins"],
    }
}

const FORGEABLE_HEADERS: [&str; 6] = [
    "NGSILD-Tenant",
    "X-Userinfo",
    "X-Access-Token",
    "X-Allowed-Scope-Ids",
    "X-Endpoint-Slug",
    "X-Consumer-Identity",
];

const SERVICE_APP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: air-quality-today
  namespace: ovzdusie
spec:
  kind: service
  source:
    path: apps/air-quality-today
  build: {}
  visibility: internal
"#;

const STATIC_APP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: air-quality-map
  namespace: ovzdusie
spec:
  kind: static
  source:
    path: apps/air-quality-map
  build: {}
  visibility: project
"#;

const PUBLIC_FULLSTACK_APP: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: hsl-transport
  namespace: doprava
spec:
  kind: fullstack
  source:
    path: apps/hsl-transport
  build: {}
  visibility: public
"#;

fn repo_with_three_apps(test_name: &str) -> std::path::PathBuf {
    let dir = demo_repo(test_name);
    write(
        &dir,
        "projects/ovzdusie/apps/air-quality-today/app.yaml",
        SERVICE_APP,
    );
    write(
        &dir,
        "projects/ovzdusie/apps/air-quality-map/app.yaml",
        STATIC_APP,
    );
    write(
        &dir,
        "projects/doprava/apps/hsl-transport/app.yaml",
        PUBLIC_FULLSTACK_APP,
    );
    dir
}

/// Without the terminal marker APISIX commits nothing and keeps serving the previous
/// routing table, silently (stack verdict S7).
#[test]
fn the_last_line_is_the_literal_end_marker() {
    let dir = repo_with_three_apps("apisix-end");
    let config = rendered(&dir);

    assert!(config.ends_with("\n#END\n"), "{config}");
    assert_eq!(
        config.lines().last(),
        Some(END_MARKER),
        "the marker must be the final line"
    );
    assert_eq!(
        config.matches("\n#END").count(),
        1,
        "exactly one marker, at the end"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// APISIX refuses the whole file if it does not parse, so the renderer's output has to be
/// YAML even with the Lua header-stripping function inside it.
#[test]
fn the_rendered_configuration_is_valid_yaml_with_the_documented_sections() {
    let dir = repo_with_three_apps("apisix-yaml");
    let document = document(&dir);

    for section in ["routes", "upstreams", "plugin_configs"] {
        assert!(
            document[section].is_array(),
            "{section} is missing from the rendered configuration"
        );
    }

    let gateway = find(&document, "upstreams", "upstream-context-gateway");
    assert!(gateway["nodes"]["context-gateway.prod.svc.cluster.local:8080"] == 1);
    assert_eq!(gateway["timeout"]["read"], 300);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The Portal lives on `portal.{host}`; the shared surfaces stay on the apex (ADR-N-019).
#[test]
fn the_portal_routes_carry_the_portal_host_and_the_surfaces_the_apex() {
    let dir = demo_repo("apisix-hosts");
    let document = document(&dir);

    for id in ["portal-ui", "portal-api"] {
        assert_eq!(
            find(&document, "routes", id)["host"],
            "portal.city.example.com",
            "{id}"
        );
    }
    for id in [
        "gitea-forge",
        "well-known",
        "context-space",
        "context-endpoint",
        "apps-surface",
        "apex-redirect",
    ] {
        assert_eq!(
            find(&document, "routes", id)["host"],
            "city.example.com",
            "{id}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The apex root sends people to the Portal: a redirect route under every other route on
/// the apex, so it answers only what no surface claimed (ADR-N-019).
#[test]
fn the_apex_redirect_is_the_lowest_priority_route_to_the_portal() {
    let dir = repo_with_three_apps("apisix-apex");
    let document = document(&dir);

    let redirect = find(&document, "routes", "apex-redirect");
    assert_eq!(redirect["uri"], "/*");
    assert_eq!(
        redirect["plugins"]["redirect"]["uri"],
        "https://portal.city.example.com/"
    );
    assert_eq!(redirect["plugins"]["redirect"]["ret_code"], 302);
    assert!(
        redirect.get("upstream_id").is_none(),
        "a redirect reaches no upstream"
    );

    let lowest = redirect["priority"].as_u64().expect("a numeric priority");
    for route in document["routes"].as_array().expect("routes") {
        if route["id"] != "apex-redirect" {
            assert!(
                route["priority"].as_u64().expect("a numeric priority") > lowest,
                "{} must sit above the apex redirect",
                route["id"]
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The endpoint surface must sit above the context space surface, and both above the
/// catch-all that serves the Portal UI (Deployment/10 section 3).
#[test]
fn route_priorities_put_the_specific_surfaces_above_the_catch_all() {
    let dir = demo_repo("apisix-priority");
    let document = document(&dir);

    let priority = |id: &str| {
        find(&document, "routes", id)["priority"]
            .as_u64()
            .expect("a numeric priority")
    };

    assert_eq!(priority("portal-ui"), 1);
    assert!(priority("context-space") > priority("portal-api"));
    assert!(priority("context-endpoint") > priority("context-space"));
    assert!(priority("apps-surface") > priority("context-endpoint"));

    let endpoint = find(&document, "routes", "context-endpoint");
    assert_eq!(endpoint["uri"], "/api/endpoint/*");
    assert_eq!(endpoint["upstream_id"], "upstream-context-gateway");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every app, whatever its kind, gets its own route behind the edge login, above the
/// shared `/apps/*` surface (AP-26).
#[test]
fn every_app_kind_gets_a_route_with_the_edge_login() {
    let dir = repo_with_three_apps("apisix-apps");
    let document = document(&dir);

    let surface = find(&document, "routes", "apps-surface")["priority"]
        .as_u64()
        .expect("priority");

    for name in ["air-quality-today", "air-quality-map", "hsl-transport"] {
        let route = find(&document, "routes", &format!("app-{name}"));
        assert_eq!(route["uri"], format!("/apps/{name}/*"));
        assert_eq!(route["host"], "city.example.com");
        assert!(route["priority"].as_u64().expect("priority") > surface);
        assert!(
            route.get("plugin_config_id").is_none(),
            "an app route carries its plugins inline"
        );

        let oidc = &route["plugins"]["openid-connect"];
        assert_eq!(oidc["client_id"], "edge", "{name}");
        assert_eq!(oidc["bearer_only"], false, "{name}");
        assert_eq!(oidc["use_jwks"], false, "{name}");
        assert_eq!(oidc["use_pkce"], true, "{name}");
        assert_eq!(oidc["ssl_verify"], true, "{name}");
        assert_eq!(oidc["set_userinfo_header"], true, "{name}");
        assert_eq!(oidc["set_access_token_header"], true, "{name}");
        assert_eq!(oidc["set_id_token_header"], false, "{name}");
        assert_eq!(
            oidc["discovery"],
            "https://idm.city.example.com/realms/prod/.well-known/openid-configuration"
        );
        assert_eq!(route["plugins"]["request-id"]["include_in_response"], true);
        assert_eq!(
            route["plugins"]["response-rewrite"]["headers"]["set"]["X-Content-Type-Options"],
            "nosniff"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A `public` app lets an anonymous request through to the app; every other visibility
/// sends the browser to Keycloak (AP-28).
#[test]
fn a_public_app_passes_anonymous_requests_and_the_others_require_login() {
    let dir = repo_with_three_apps("apisix-visibility");
    let document = document(&dir);

    let unauth = |name: &str| {
        find(&document, "routes", &format!("app-{name}"))["plugins"]["openid-connect"]
            ["unauth_action"]
            .clone()
    };
    assert_eq!(unauth("hsl-transport"), "pass");
    assert_eq!(unauth("air-quality-today"), "auth");
    assert_eq!(unauth("air-quality-map"), "auth");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The session cookie, the callback and the logout of an app route follow the app's own
/// name and path, so one app's session is not another's; the `session` object takes flat
/// keys, a nested `cookie` block is ignored by APISIX (AP-29).
#[test]
fn the_cookie_name_path_and_the_logout_path_follow_the_app_name() {
    let dir = repo_with_three_apps("apisix-cookie");
    let document = document(&dir);

    for name in ["air-quality-today", "air-quality-map", "hsl-transport"] {
        let oidc = &find(&document, "routes", &format!("app-{name}"))["plugins"]["openid-connect"];
        let session = &oidc["session"];
        assert_eq!(session["cookie_name"], format!("jc_edge_app_{name}"));
        assert_eq!(session["cookie_path"], format!("/apps/{name}/"));
        assert_eq!(session["cookie_secure"], true);
        assert_eq!(session["cookie_http_only"], true);
        assert_eq!(session["cookie_same_site"], "Lax");
        assert_eq!(session["idling_timeout"], 3600);
        assert_eq!(session["rolling_timeout"], 3600);
        assert_eq!(session["absolute_timeout"], 36000);
        assert!(session.get("cookie").is_none(), "no nested cookie block");
        assert_eq!(oidc["logout_path"], format!("/apps/{name}/logout"));
        assert_eq!(
            oidc["redirect_uri"],
            format!("https://city.example.com/apps/{name}/callback")
        );
        assert_eq!(
            oidc["post_logout_redirect_uri"],
            format!("https://city.example.com/apps/{name}/")
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A `service` or `fullstack` app upstreams to its own pod on 8080; a `static` app is
/// served by the Portal and gets no upstream of its own (AP-26).
#[test]
fn app_upstreams_point_at_the_pod_or_the_portal_by_kind() {
    let dir = repo_with_three_apps("apisix-upstreams");
    let document = document(&dir);

    for name in ["air-quality-today", "hsl-transport"] {
        let route = find(&document, "routes", &format!("app-{name}"));
        assert_eq!(route["upstream_id"], format!("upstream-app-{name}"));
        let pod = find(&document, "upstreams", &format!("upstream-app-{name}"));
        assert!(pod["nodes"][format!("app-{name}.prod.svc.cluster.local:8080")] == 1);
    }

    let static_route = find(&document, "routes", "app-air-quality-map");
    assert_eq!(static_route["upstream_id"], "upstream-portal");
    assert!(
        !has(&document, "upstreams", "upstream-app-air-quality-map"),
        "a static app has no upstream of its own"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The oauth2-proxy sidecar is gone: nothing in the routing table points at its port.
#[test]
fn no_route_or_upstream_mentions_the_old_sidecar_port() {
    let dir = repo_with_three_apps("apisix-no-sidecar");
    let config = rendered(&dir);

    assert!(!config.contains("4180"), "{config}");
    assert!(!config.contains("oauth2-proxy"), "{config}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The Portal routes and the shared surface run the same login front as the apps, with
/// the cookie scoped to their own host or path (ADR-N-019).
#[test]
fn the_portal_and_the_shared_surface_log_in_at_the_edge() {
    let dir = demo_repo("apisix-portal-login");
    let document = document(&dir);

    let ui = &plugins_of(&document, "portal-ui")["openid-connect"];
    assert_eq!(ui["client_id"], "edge");
    assert_eq!(ui["bearer_only"], false);
    assert_eq!(ui["use_jwks"], false);
    assert_eq!(ui["use_pkce"], true);
    assert_eq!(ui["unauth_action"], "auth");
    assert_eq!(
        ui["redirect_uri"],
        "https://portal.city.example.com/callback"
    );
    assert_eq!(ui["logout_path"], "/logout");
    assert_eq!(
        ui["post_logout_redirect_uri"],
        "https://portal.city.example.com/"
    );
    assert_eq!(ui["session"]["cookie_name"], "jc_edge");
    assert_eq!(ui["session"]["cookie_path"], "/");

    // A bearer caller passes through and the Portal verifies the token itself.
    let api = &plugins_of(&document, "portal-api")["openid-connect"];
    assert_eq!(api["client_id"], "edge");
    assert_eq!(api["bearer_only"], false);
    assert_eq!(
        api["use_jwks"], false,
        "a bearer token the plugin cannot verify must reach unauth_action, not a 401"
    );
    assert_eq!(api["unauth_action"], "pass");
    assert_eq!(api["session"]["cookie_name"], "jc_edge");
    assert_eq!(api["set_access_token_header"], true);

    let apps = &plugins_of(&document, "apps-surface")["openid-connect"];
    assert_eq!(apps["unauth_action"], "auth");
    assert_eq!(apps["session"]["cookie_name"], "jc_edge_apps");
    assert_eq!(apps["session"]["cookie_path"], "/apps/");
    assert_eq!(
        apps["redirect_uri"],
        "https://city.example.com/apps/callback"
    );
    assert_eq!(apps["logout_path"], "/apps/logout");
    assert_eq!(
        find(&document, "routes", "apps-surface")["upstream_id"],
        "upstream-portal"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every header a caller could use to forge a tenant, an identity or an authorization
/// claim is cleared before the edge sets its own, on every route that reaches an upstream
/// with identity headers (Deployment/10 section 4, AP-28).
#[test]
fn every_login_and_context_route_strips_every_forgeable_header() {
    let dir = repo_with_three_apps("apisix-headers");
    let document = document(&dir);

    for id in [
        "portal-ui",
        "portal-api",
        "apps-surface",
        "context-space",
        "context-endpoint",
        "app-air-quality-today",
        "app-air-quality-map",
        "app-hsl-transport",
    ] {
        let plugins = plugins_of(&document, id);
        let lua = plugins["serverless-pre-function"]["functions"][0]
            .as_str()
            .unwrap_or_else(|| panic!("{id} has the stripping function"));
        for header in FORGEABLE_HEADERS {
            assert!(lua.contains(header), "{id} does not clear {header}");
        }
        assert_eq!(plugins["serverless-pre-function"]["phase"], "rewrite");
    }

    // The context surfaces never run a code flow: the tenant surface takes bearer tokens
    // only, the public endpoint surface lets an anonymous caller through to the gateway's
    // own PEP.
    let firewall = &plugins_of(&document, "context-space")["openid-connect"];
    assert_eq!(firewall["client_id"], "edge");
    assert_eq!(firewall["bearer_only"], true);
    assert_eq!(firewall["use_jwks"], true);
    assert!(
        firewall.get("client_secret").is_none(),
        "bearer_only needs no client secret"
    );
    let surface = &plugins_of(&document, "context-endpoint")["openid-connect"];
    assert_eq!(surface["client_id"], "edge");
    assert_eq!(surface["bearer_only"], false);
    assert_eq!(surface["unauth_action"], "pass");
    assert_eq!(surface["use_jwks"], true);
    assert_eq!(surface["ssl_verify"], true);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The secrets are placeholders in Git; the deployment's ConfigMap template turns them
/// into APISIX environment references (AP-27).
#[test]
fn the_secret_placeholders_appear_verbatim_and_no_secret_value_does() {
    let dir = repo_with_three_apps("apisix-placeholders");
    let config = rendered(&dir);
    let document = document(&dir);

    assert_eq!(EDGE_CLIENT_SECRET, "${EDGE_CLIENT_SECRET}");
    assert_eq!(OIDC_SESSION_SECRET, "${OIDC_SESSION_SECRET}");
    assert!(config.contains("${EDGE_CLIENT_SECRET}"), "{config}");
    assert!(config.contains("${OIDC_SESSION_SECRET}"), "{config}");

    for id in [
        "portal-ui",
        "portal-api",
        "apps-surface",
        "app-air-quality-today",
    ] {
        let oidc = &plugins_of(&document, id)["openid-connect"];
        assert_eq!(oidc["client_secret"], "${EDGE_CLIENT_SECRET}", "{id}");
        assert_eq!(oidc["session"]["secret"], "${OIDC_SESSION_SECRET}", "{id}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_same_repository_renders_byte_identical_output() {
    let dir = repo_with_three_apps("apisix-deterministic");

    assert_eq!(rendered(&dir), rendered(&dir));
    assert!(rendered(&dir).ends_with("\n#END\n"));

    let _ = std::fs::remove_dir_all(&dir);
}
