//! The declarative APISIX standalone configuration (T-0138, T-0508, OPS-31,
//! Deployment/10 section 3).
//!
//! APISIX runs in file mode with no etcd and no Admin API, so the whole routing table is
//! one rendered file. The Portal is served on `portal.{host}`; the apex `{host}` keeps the
//! shared surfaces (`/apps/*`, `/git/*`, `/cs/*`, `/api/endpoint/*`, `/.well-known/*`) and
//! redirects `/` to the Portal (ADR-N-019).
//!
//! Login happens at the edge: the `openid-connect` plugin in session mode sits on the
//! Portal routes, on the shared `/apps/*` surface and on the route every `App` gets,
//! `static` apps included, so no app pod carries a login sidecar and no app contains login
//! code (AP-26). The one confidential client `edge` of the realm serves all of them
//! (AP-27). The plugin sets `X-Userinfo` and `X-Access-Token` for the upstream after the
//! route stripped them from the client request, so only the edge can set them; a
//! `visibility: public` app lets an anonymous request pass (AP-28). The session cookie of
//! an app route is scoped to `/apps/{name}/` and `/apps/{name}/logout` ends the edge and
//! the Keycloak session (AP-29). Endpoints add no routes: `/api/endpoint/*` is one route
//! and the gateway resolves the slug behind it.
//!
//! Two secrets appear in the output as the literal strings [`EDGE_CLIENT_SECRET`] and
//! [`OIDC_SESSION_SECRET`]. The deployment's ConfigMap template turns them into APISIX
//! environment-variable references, so the values reach APISIX from Kubernetes Secrets
//! and never sit in Git (AP-27).
//!
//! The file must end with the literal `#END`. Without it APISIX commits nothing and keeps
//! serving the previous configuration without an error (stack verdict S7).

use std::collections::BTreeMap;

use crate::loader::Repository;
use serde_json::{json, Map, Value};

/// The terminal line APISIX needs to commit a reload (Deployment/10 section 3).
pub const END_MARKER: &str = "#END";

/// The placeholder for the secret of the `edge` client, verbatim in the output (AP-27).
pub const EDGE_CLIENT_SECRET: &str = "${EDGE_CLIENT_SECRET}";

/// The placeholder for the session cookie secret, verbatim in the output (AP-27).
pub const OIDC_SESSION_SECRET: &str = "${OIDC_SESSION_SECRET}";

/// The one confidential OIDC client every browser-facing route logs in with (AP-27).
const EDGE_CLIENT_ID: &str = "edge";

/// The port an app container listens on, the one its upstream points at (AP-26).
const APP_PORT: u16 = 8080;

/// The edge session idles out after an hour and rolls with use, within the realm's SSO
/// idle time (AP-29).
const SESSION_IDLE_SECONDS: u32 = 3600;

/// The edge session ends after ten hours whatever the use (AP-29).
const SESSION_ABSOLUTE_SECONDS: u32 = 36_000;

/// What the routing table needs to know about the installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The primary domain, `city.example.com`. Keycloak lives on `idm.{host}`, the Portal
    /// on `portal.{host}`.
    pub host: String,
    /// The Kubernetes namespace the platform runs in, used for upstream service names.
    pub namespace: String,
    /// The Keycloak realm the gateway validates tokens against.
    pub realm: String,
}

impl Settings {
    /// Settings for one installation.
    pub fn new(
        host: impl Into<String>,
        namespace: impl Into<String>,
        realm: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            namespace: namespace.into(),
            realm: realm.into(),
        }
    }

    fn discovery(&self) -> String {
        format!(
            "https://idm.{}/realms/{}/.well-known/openid-configuration",
            self.host, self.realm
        )
    }

    fn portal_host(&self) -> String {
        format!("portal.{}", self.host)
    }

    fn portal_url(&self, path: &str) -> String {
        format!("https://portal.{}{path}", self.host)
    }

    fn apex_url(&self, path: &str) -> String {
        format!("https://{}{path}", self.host)
    }

    fn node(&self, service: &str, port: u16) -> String {
        format!("{service}.{}.svc.cluster.local:{port}", self.namespace)
    }

    /// The CORS origin pattern: any subdomain of the installation's own host.
    fn origin_regex(&self) -> String {
        format!("^https://.+\\.{}$", self.host.replace('.', "\\."))
    }
}

/// Renders the whole `apisix.yaml`, `#END` included (Deployment/10 section 3).
///
/// Deterministic: the same repository and settings render byte-identical output, so an
/// unchanged configuration produces no ConfigMap churn.
pub fn render(repo: &Repository, settings: &Settings) -> String {
    let apex = settings.host.as_str();
    let portal = settings.portal_host();

    let mut routes = vec![
        redirect_route("apex-redirect", "/*", apex, 0, &settings.portal_url("/")),
        shared_route(
            "portal-ui",
            "/*",
            &portal,
            1,
            "upstream-portal",
            "pc-portal-web",
        ),
        shared_route(
            "portal-api",
            "/api/v1/*",
            &portal,
            10,
            "upstream-portal",
            "pc-authenticated-api",
        ),
        shared_route(
            "gitea-forge",
            "/git/*",
            apex,
            10,
            "upstream-gitea",
            "pc-public-web",
        ),
        shared_route(
            "well-known",
            "/.well-known/*",
            apex,
            10,
            "upstream-portal",
            "pc-public-web",
        ),
        shared_route(
            "context-space",
            "/cs/*",
            apex,
            15,
            "upstream-context-gateway",
            "pc-context-firewall",
        ),
        shared_route(
            "context-endpoint",
            "/api/endpoint/*",
            apex,
            20,
            "upstream-context-gateway",
            "pc-endpoint-surface",
        ),
        shared_route(
            "apps-surface",
            "/apps/*",
            apex,
            25,
            "upstream-portal",
            "pc-apps-surface",
        ),
    ];
    let mut upstreams = vec![
        upstream(
            "upstream-portal",
            &settings.node("portal", 8080),
            30,
            30,
            None,
        ),
        upstream(
            "upstream-gitea",
            &settings.node("gitea-http", 3000),
            60,
            60,
            None,
        ),
        upstream(
            "upstream-context-gateway",
            &settings.node("context-gateway", 8080),
            60,
            300,
            Some(320),
        ),
    ];

    for app in routed_apps(repo) {
        let name = app.name.as_str();
        let upstream_id = if app.own_pod {
            let id = format!("upstream-app-{name}");
            upstreams.push(upstream(
                &id,
                &settings.node(&format!("app-{name}"), APP_PORT),
                30,
                30,
                None,
            ));
            id
        } else {
            "upstream-portal".to_owned()
        };
        routes.push(app_route(settings, &app, &upstream_id));
    }

    routes.sort_by_key(|r| r["id"].as_str().unwrap_or_default().to_owned());

    let document = json!({
        "routes": routes,
        "upstreams": upstreams,
        "plugin_configs": plugin_configs(settings),
    });

    let body = serde_norway::to_string(&document)
        .expect("the rendered configuration is plain data and always serializes");
    format!("# Generated by jcctl — DO NOT EDIT DIRECTLY\n{body}\n{END_MARKER}\n")
}

/// One `App` as the routing table sees it (AP-26, AP-28).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutedApp {
    name: String,
    /// A `service` or `fullstack` app runs in its own pod and gets its own upstream; a
    /// `static` app is served by the Portal's static host.
    own_pod: bool,
    /// `visibility: public`: an anonymous request passes through to the app.
    public: bool,
}

/// Every `App` gets a route, whatever its kind (AP-26); two manifests of one name render
/// one route, the names sorted so the output is stable.
fn routed_apps(repo: &Repository) -> Vec<RoutedApp> {
    let mut apps: BTreeMap<String, RoutedApp> = BTreeMap::new();
    for (id, resource) in repo.iter().filter(|(id, _)| id.kind == "App") {
        let spec = &resource.manifest.spec;
        let own_pod = matches!(
            spec.get("kind").and_then(Value::as_str),
            Some("service" | "fullstack")
        );
        let public = spec.get("visibility").and_then(Value::as_str) == Some("public");
        apps.entry(id.name.clone()).or_insert(RoutedApp {
            name: id.name.clone(),
            own_pod,
            public,
        });
    }
    apps.into_values().collect()
}

/// The route of one app: its own login front, cookie scoped to the app, at a higher
/// priority than the shared `/apps/*` surface (AP-26…AP-29).
fn app_route(settings: &Settings, app: &RoutedApp, upstream_id: &str) -> Value {
    let name = app.name.as_str();
    let base = format!("/apps/{name}/");
    let login = Session {
        unauth_action: if app.public { "pass" } else { "auth" },
        cookie_name: format!("jc_edge_app_{name}"),
        cookie_path: base.clone(),
        redirect_uri: settings.apex_url(&format!("{base}callback")),
        logout_path: format!("{base}logout"),
        post_logout_redirect_uri: settings.apex_url(&base),
    };
    inline_route(
        &format!("app-{name}"),
        &format!("{base}*"),
        &settings.host,
        30,
        upstream_id,
        json!({
            "request-id": { "include_in_response": true },
            "serverless-pre-function": strip_forgeable_headers(),
            "openid-connect": openid_connect_session(settings, &login),
            "response-rewrite": security_headers(WEB_HEADERS),
        }),
    )
}

fn base_route(id: &str, uri: &str, host: &str, priority: u32) -> Value {
    json!({
        "id": id,
        "uri": uri,
        "host": host,
        "priority": priority,
    })
}

fn shared_route(
    id: &str,
    uri: &str,
    host: &str,
    priority: u32,
    upstream_id: &str,
    plugin_config_id: &str,
) -> Value {
    let mut route = base_route(id, uri, host, priority);
    route["upstream_id"] = json!(upstream_id);
    route["plugin_config_id"] = json!(plugin_config_id);
    route
}

fn inline_route(
    id: &str,
    uri: &str,
    host: &str,
    priority: u32,
    upstream_id: &str,
    plugins: Value,
) -> Value {
    let mut route = base_route(id, uri, host, priority);
    route["upstream_id"] = json!(upstream_id);
    route["plugins"] = plugins;
    route
}

/// A route that answers with a redirect and reaches no upstream: the apex root sends
/// people to the Portal (ADR-N-019).
fn redirect_route(id: &str, uri: &str, host: &str, priority: u32, target: &str) -> Value {
    let mut route = base_route(id, uri, host, priority);
    route["plugins"] = json!({
        "redirect": { "uri": target, "ret_code": 302 },
    });
    route
}

fn upstream(id: &str, node: &str, send: u32, read: u32, keepalive: Option<u32>) -> Value {
    let mut nodes = Map::new();
    nodes.insert(node.to_owned(), json!(1));

    let mut value = json!({
        "id": id,
        "type": "roundrobin",
        "nodes": Value::Object(nodes),
        "timeout": { "connect": 6, "send": send, "read": read },
    });
    if let Some(size) = keepalive {
        value["keepalive_pool"] = json!({ "size": size, "idle_timeout": 60, "requests": 1000 });
    }
    value
}

/// The headers the upstream must never receive from a client: any of these would let a
/// caller forge its own tenant, identity or authorization claims (Deployment/10 section 4,
/// AP-28). The strip runs in the `rewrite` phase, before `openid-connect` sets
/// `X-Userinfo` and `X-Access-Token` from the session, whatever the key order of the
/// plugin table: APISIX orders plugins by their own priority, not by the file.
const FORGEABLE_HEADERS: &[&str] = &[
    "NGSILD-Tenant",
    "X-Userinfo",
    "X-Access-Token",
    "X-Allowed-Scope-Ids",
    "X-Endpoint-Slug",
    "X-Consumer-Identity",
];

fn strip_forgeable_headers() -> Value {
    let clears: String = FORGEABLE_HEADERS
        .iter()
        .map(|header| format!("  ngx.req.clear_header(\"{header}\")\n"))
        .collect();
    json!({
        "phase": "rewrite",
        "functions": [format!("return function()\n{clears}end")],
    })
}

/// The response headers of a page a browser renders.
const WEB_HEADERS: &[(&str, &str)] = &[
    ("X-Frame-Options", "SAMEORIGIN"),
    ("Referrer-Policy", "strict-origin-when-cross-origin"),
];

/// The response headers of an authenticated API.
const API_HEADERS: &[(&str, &str)] = &[
    ("X-Frame-Options", "DENY"),
    ("Cache-Control", "no-store, no-cache, must-revalidate"),
];

fn security_headers(extra: &[(&str, &str)]) -> Value {
    let mut set = Map::new();
    set.insert(
        "Strict-Transport-Security".to_owned(),
        json!("max-age=31536000; includeSubDomains; preload"),
    );
    set.insert("X-Content-Type-Options".to_owned(), json!("nosniff"));
    for (name, value) in extra {
        set.insert((*name).to_owned(), json!(value));
    }
    json!({ "headers": { "set": Value::Object(set) } })
}

/// One edge login front: where the code flow returns, where the cookie lives, where the
/// session ends (AP-29).
struct Session {
    /// `auth`: an anonymous browser is sent to Keycloak; `pass`: it reaches the upstream
    /// without identity headers and the upstream decides (AP-28).
    unauth_action: &'static str,
    /// One cookie name per login front, so overlapping paths (`/apps/` under
    /// `/apps/{name}/`) never present the wrong session cookie.
    cookie_name: String,
    cookie_path: String,
    redirect_uri: String,
    logout_path: String,
    post_logout_redirect_uri: String,
}

/// The `openid-connect` plugin in session mode: the code flow with PKCE against the `edge`
/// client, an encrypted cookie, the userinfo and the access token handed to the upstream
/// (ADR-N-019, AP-27, AP-28).
///
/// `use_jwks` stays off here: with it on, a bearer token the plugin cannot verify (the
/// realm signs ES256, lua-resty-openidc verifies RS/HS only) is a hard `401` before
/// `unauth_action` is consulted, which would shut every CLI caller out of `portal-api`. The
/// code flow's ID token is still verified by lua-resty-openidc through discovery. The
/// `session` object takes flat keys (APISIX 3.17); a nested `cookie` block is ignored.
fn openid_connect_session(settings: &Settings, login: &Session) -> Value {
    json!({
        "client_id": EDGE_CLIENT_ID,
        "client_secret": EDGE_CLIENT_SECRET,
        "discovery": settings.discovery(),
        "bearer_only": false,
        "use_jwks": false,
        "use_pkce": true,
        "ssl_verify": true,
        "unauth_action": login.unauth_action,
        "redirect_uri": login.redirect_uri,
        "logout_path": login.logout_path,
        "post_logout_redirect_uri": login.post_logout_redirect_uri,
        "set_userinfo_header": true,
        "set_access_token_header": true,
        "set_id_token_header": false,
        "session": {
            "secret": OIDC_SESSION_SECRET,
            "cookie_name": login.cookie_name,
            "cookie_path": login.cookie_path,
            "cookie_secure": true,
            "cookie_http_only": true,
            "cookie_same_site": "Lax",
            "idling_timeout": SESSION_IDLE_SECONDS,
            "rolling_timeout": SESSION_IDLE_SECONDS,
            "absolute_timeout": SESSION_ABSOLUTE_SECONDS,
        },
    })
}

/// The `openid-connect` plugin on the context surfaces, which never run a code flow: a
/// bearer token is verified against the realm JWKS (`use_jwks` stays on here), so
/// `bearer_only: true` carries no client secret; the public endpoint surface lets an anonymous request through to the
/// gateway's own PEP, which decides what it may see.
fn openid_connect_bearer(settings: &Settings, bearer_only: bool) -> Value {
    let mut plugin = json!({
        "client_id": EDGE_CLIENT_ID,
        "discovery": settings.discovery(),
        "bearer_only": bearer_only,
        "use_jwks": true,
        "ssl_verify": true,
    });
    if !bearer_only {
        plugin["client_secret"] = json!(EDGE_CLIENT_SECRET);
        plugin["unauth_action"] = json!("pass");
    }
    plugin
}

fn plugin_configs(settings: &Settings) -> Value {
    let portal_login = |unauth_action| Session {
        unauth_action,
        cookie_name: "jc_edge".to_owned(),
        cookie_path: "/".to_owned(),
        redirect_uri: settings.portal_url("/callback"),
        logout_path: "/logout".to_owned(),
        post_logout_redirect_uri: settings.portal_url("/"),
    };
    let apps_login = Session {
        unauth_action: "auth",
        cookie_name: "jc_edge_apps".to_owned(),
        cookie_path: "/apps/".to_owned(),
        redirect_uri: settings.apex_url("/apps/callback"),
        logout_path: "/apps/logout".to_owned(),
        post_logout_redirect_uri: settings.apex_url("/apps/"),
    };

    json!([
        {
            "id": "pc-public-web",
            "plugins": {
                "request-id": { "include_in_response": true },
                "response-rewrite": security_headers(WEB_HEADERS),
            },
        },
        {
            "id": "pc-portal-web",
            "plugins": {
                "request-id": { "include_in_response": true },
                "serverless-pre-function": strip_forgeable_headers(),
                "openid-connect": openid_connect_session(settings, &portal_login("auth")),
                "response-rewrite": security_headers(WEB_HEADERS),
            },
        },
        {
            // A browser session becomes `X-Access-Token`; a bearer caller (CLI, service
            // account) passes through and the Portal verifies the token itself.
            "id": "pc-authenticated-api",
            "plugins": {
                "request-id": { "include_in_response": true },
                "serverless-pre-function": strip_forgeable_headers(),
                "openid-connect": openid_connect_session(settings, &portal_login("pass")),
                "response-rewrite": security_headers(API_HEADERS),
            },
        },
        {
            "id": "pc-apps-surface",
            "plugins": {
                "request-id": { "include_in_response": true },
                "serverless-pre-function": strip_forgeable_headers(),
                "openid-connect": openid_connect_session(settings, &apps_login),
                "response-rewrite": security_headers(WEB_HEADERS),
            },
        },
        {
            "id": "pc-context-firewall",
            "plugins": {
                "request-id": { "include_in_response": true },
                "serverless-pre-function": strip_forgeable_headers(),
                "openid-connect": openid_connect_bearer(settings, true),
                "response-rewrite": security_headers(&[
                    ("Cache-Control", "no-store, no-cache, must-revalidate"),
                ]),
            },
        },
        {
            "id": "pc-endpoint-surface",
            "plugins": {
                "request-id": { "include_in_response": true },
                "serverless-pre-function": strip_forgeable_headers(),
                "openid-connect": openid_connect_bearer(settings, false),
                "cors": {
                    "allow_origins_by_regex": [settings.origin_regex()],
                    "allow_methods": "GET,HEAD,POST,OPTIONS",
                    "allow_headers": "Authorization,Content-Type,Accept,Link",
                    "allow_credential": true,
                },
                "response-rewrite": security_headers(&[]),
            },
        },
    ])
}
