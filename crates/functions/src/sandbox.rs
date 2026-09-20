//! One invocation in a fresh QuickJS runtime, thrown away afterwards (SDK-22, Architecture/20 §3).
//!
//! The context holds the invocation's files and nothing else: the loader resolves an import only
//! to a key of `files`, QuickJS itself has no `fetch`, timer, file system or environment, and the
//! one host function is the endpoint request of [`crate::endpoint`]. Memory is capped by the
//! engine's allocator, CPU by its interrupt handler, and the wall clock (which includes waiting on
//! the gateway) by a timeout around the whole call.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::function::Async;
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx, Function, Module};
use serde::Serialize;
use serde_json::Value;

use crate::endpoint::{self, Endpoint};

pub const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
pub const TIME_LIMIT: Duration = Duration::from_secs(5);
pub const RESPONSE_LIMIT: usize = 1024 * 1024;
const STACK_LIMIT: usize = 1024 * 1024;
const LOG_LINES: usize = 200;
const LOG_LINE_CHARS: usize = 2000;
/// The module a function's context comes from: the SDK's server entry, sent with the files.
pub const SDK_SERVER: &str = "@joinedcontext/sdk/server";
const GLUE: &str = "@jc/invoke";

/// What the Portal hands over for one call.
pub struct Invocation {
    pub files: BTreeMap<String, String>,
    pub entry: String,
    /// `FnRequest`, as JSON.
    pub request: Value,
    /// The SDK configuration `ctx.jc` is created with.
    pub config: Value,
    pub endpoint: Endpoint,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            file: None,
            line: None,
        }
    }
}

/// The function's answer, or why there is none.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
    pub logs: Vec<String>,
}

struct Files(Arc<BTreeMap<String, String>>);

impl Resolver for Files {
    fn resolve<'js>(
        &mut self,
        _: &Ctx<'js>,
        base: &str,
        name: &str,
        _: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if self.0.contains_key(name) {
            Ok(name.to_owned())
        } else {
            Err(rquickjs::Error::new_resolving_message(
                base,
                name,
                "only the invocation's files may be imported",
            ))
        }
    }
}

impl Loader for Files {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        let source = self
            .0
            .get(name)
            .ok_or_else(|| rquickjs::Error::new_loading(name))?;
        Module::declare(ctx.clone(), name, source.as_str())
    }
}

/// Calls the entry's default export with the request and `{jc, log}`, where `jc` is the SDK's own
/// client over the host request.
fn glue(entry: &str) -> String {
    format!(
        r#"import handler from {entry};
import {{ createClient }} from {sdk};
const input = JSON.parse(globalThis.__jc_input);
const hostRequest = globalThis.__jc_request;
const hostLog = globalThis.__jc_log;
const transport = async (r) => hostRequest(r.method, r.path, r.body === undefined ? undefined : JSON.stringify(r.body));
const text = (part) => {{
  if (typeof part === "string") return part;
  try {{ return JSON.stringify(part) ?? String(part); }} catch {{ return String(part); }}
}};
const log = (...parts) => hostLog(parts.map(text).join(" "));
const response = await handler(input.request, {{ jc: createClient(input.config, transport), log }});
globalThis.__jc_output = JSON.stringify(response === undefined ? {{}} : response);
"#,
        entry = serde_json::to_string(entry).unwrap_or_default(),
        sdk = serde_json::to_string(SDK_SERVER).unwrap_or_default(),
    )
}

/// `name (file:line:column)` or `file:line:column` in the first frame of a QuickJS stack.
fn location(stack: &str) -> (Option<String>, Option<u32>) {
    for frame in stack
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("at "))
    {
        let place = frame
            .rsplit_once(" (")
            .map_or(frame, |(_, p)| p.trim_end_matches(')'));
        let mut parts = place.rsplitn(3, ':');
        let (Some(_column), Some(line), Some(file)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if file == GLUE || file == "<native>" {
            continue;
        }
        if let Ok(line) = line.parse() {
            return (Some(file.to_owned()), Some(line));
        }
    }
    (None, None)
}

fn failure(error: CaughtError<'_>) -> Failure {
    match error {
        CaughtError::Exception(exception) => {
            let (file, line) = location(&exception.stack().unwrap_or_default());
            let message = exception.message().unwrap_or_default();
            Failure {
                message: if message.is_empty() {
                    "the function threw".to_owned()
                } else {
                    message
                },
                file,
                line,
            }
        }
        CaughtError::Value(value) => {
            Failure::new(format!("the function threw {}", value.type_name()))
        }
        CaughtError::Error(error) => Failure::new(error.to_string()),
    }
}

async fn execute(invocation: Invocation, logs: Arc<Mutex<Vec<String>>>) -> Result<String, Failure> {
    let Invocation {
        files,
        entry,
        request,
        config,
        endpoint,
    } = invocation;
    if !files.contains_key(&entry) {
        return Err(Failure::new(format!(
            "the entry {entry} is not among the files"
        )));
    }
    let internal =
        |error: rquickjs::Error| Failure::new(format!("the runtime could not start: {error}"));
    let runtime = AsyncRuntime::new().map_err(internal)?;
    runtime.set_memory_limit(MEMORY_LIMIT).await;
    runtime.set_max_stack_size(STACK_LIMIT).await;
    let deadline = Instant::now() + TIME_LIMIT;
    runtime
        .set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
        .await;
    let files = Arc::new(files);
    runtime.set_loader(Files(files.clone()), Files(files)).await;
    let context = AsyncContext::full(&runtime).await.map_err(internal)?;
    let input = serde_json::json!({ "request": request, "config": config }).to_string();
    let source = glue(&entry);
    let endpoint = Arc::new(endpoint);

    let work = context.async_with(async move |ctx| -> Result<String, Failure> {
        let globals = ctx.globals();
        let setup = || -> rquickjs::Result<()> {
            globals.set("__jc_input", input)?;
            let host = endpoint.clone();
            globals.set(
                "__jc_request",
                Function::new(
                    ctx.clone(),
                    Async(move |method: String, path: String, body: Option<String>| {
                        let host = host.clone();
                        async move {
                            rquickjs::Result::Ok(Json(
                                endpoint::request(&host, &method, &path, body).await,
                            ))
                        }
                    }),
                )?,
            )?;
            globals.set(
                "__jc_log",
                Function::new(ctx.clone(), move |line: String| {
                    let mut logs = logs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if logs.len() < LOG_LINES {
                        logs.push(line.chars().take(LOG_LINE_CHARS).collect());
                    }
                })?,
            )?;
            Ok(())
        };
        setup().map_err(|error| Failure::new(format!("the runtime could not start: {error}")))?;
        let (_, promise) = Module::declare(ctx.clone(), GLUE, source)
            .and_then(Module::eval)
            .catch(&ctx)
            .map_err(failure)?;
        promise
            .into_future::<()>()
            .await
            .catch(&ctx)
            .map_err(failure)?;
        globals
            .get::<_, Option<String>>("__jc_output")
            .ok()
            .flatten()
            .ok_or_else(|| Failure::new("the function returned nothing the runtime can read"))
    });
    let timed_out = || {
        Failure::new(format!(
            "the function ran longer than {} s",
            TIME_LIMIT.as_secs()
        ))
    };
    match tokio::time::timeout(TIME_LIMIT, work).await {
        Err(_) => Err(timed_out()),
        Ok(Err(_)) if Instant::now() >= deadline => Err(timed_out()),
        Ok(result) => result,
    }
}

/// A JSON value handed to JavaScript as the object it describes.
struct Json(Value);

impl<'js> rquickjs::IntoJs<'js> for Json {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
        ctx.json_parse(self.0.to_string())
    }
}

/// Runs one invocation to its answer. Never panics on the function's behalf: whatever the code
/// does ends as an `Outcome`.
pub async fn run(invocation: Invocation) -> Outcome {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let result = execute(invocation, logs.clone()).await;
    let logs = std::mem::take(&mut *logs.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    let failed = |error: Failure| Outcome {
        status: 500,
        body: None,
        error: Some(error),
        logs: logs.clone(),
    };
    let output = match result {
        Ok(output) => output,
        Err(error) => return failed(error),
    };
    if output.len() > RESPONSE_LIMIT {
        return failed(Failure::new("the function's response is larger than 1 MiB"));
    }
    let response: Value = match serde_json::from_str(&output) {
        Ok(Value::Object(response)) => Value::Object(response),
        _ => return failed(Failure::new("a function returns an object {status, body}")),
    };
    let status = match response.get("status") {
        None | Some(Value::Null) => 200,
        Some(status) => match status.as_u64().filter(|s| (100..=599).contains(s)) {
            Some(status) => status as u16,
            None => return failed(Failure::new("status must be an integer from 100 to 599")),
        },
    };
    Outcome {
        status,
        body: Some(response.get("body").cloned().unwrap_or(Value::Null)),
        error: None,
        logs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A stand-in for the SDK's server module: `ctx.jc` with one method over the transport.
    const SDK: &str = r#"
export function createClient(config, transport) {
  return {
    config,
    entities: {
      async list(type) {
        const answer = await transport({ method: "GET", path: `/api/endpoint/${config.slug}/ngsi-ld/v1/entities?type=${type}` });
        if (answer.status !== 200) throw new Error(`HTTP ${answer.status}`);
        return answer.body;
      },
    },
    raw: transport,
  };
}
"#;
    pub const SLUG: &str = "k7m2qz4tv6xh3n5jb2ryd3wcfa";

    pub fn invocation(function: &str, gateway: &str, token: Option<&str>) -> Invocation {
        Invocation {
            files: BTreeMap::from([
                ("@app/functions/f.ts".to_owned(), function.to_owned()),
                (SDK_SERVER.to_owned(), SDK.to_owned()),
            ]),
            entry: "@app/functions/f.ts".to_owned(),
            request: json!({ "method": "POST", "query": {}, "body": { "n": 2 }, "user": null }),
            config: json!({ "slug": SLUG, "orgDomain": "hel.fi", "space": "mobility" }),
            endpoint: Endpoint {
                http: reqwest::Client::new(),
                gateway: gateway.to_owned(),
                slug: SLUG.to_owned(),
                token: token.map(str::to_owned),
            },
        }
    }

    async fn call(function: &str) -> Outcome {
        run(invocation(function, "http://127.0.0.1:9", None)).await
    }

    #[tokio::test]
    async fn a_function_answers_with_its_status_body_and_logs() {
        let outcome = call(
            "export default async (request, ctx) => { ctx.log('doubling', request.body.n, { x: 1 }); return { status: 201, body: { n: request.body.n * 2 } }; };",
        )
        .await;
        assert_eq!(
            (outcome.status, outcome.error.clone()),
            (201, None),
            "{outcome:?}"
        );
        assert_eq!(outcome.body, Some(json!({ "n": 4 })));
        assert_eq!(outcome.logs, vec![r#"doubling 2 {"x":1}"#]);
    }

    #[tokio::test]
    async fn an_endless_loop_is_interrupted() {
        let started = Instant::now();
        let outcome = call("export default async () => { for (;;) {} };").await;
        assert_eq!(outcome.status, 500);
        assert!(outcome.error.unwrap().message.contains("longer than 5 s"));
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[tokio::test]
    async fn a_100_mib_allocation_fails() {
        let outcome = call("export default async () => { const s = 'x'.repeat(100 * 1024 * 1024); return { body: s.length }; };").await;
        assert_eq!(outcome.status, 500, "{outcome:?}");
        assert!(outcome.error.unwrap().message.contains("out of memory"));
        // The next call has its whole budget again.
        assert_eq!(
            call("export default async () => ({ body: 'x'.repeat(1024 * 1024).length });")
                .await
                .status,
            200
        );
    }

    #[tokio::test]
    async fn an_oversized_response_is_refused() {
        let outcome =
            call("export default async () => ({ body: 'x'.repeat(1024 * 1024 + 1) });").await;
        assert_eq!(outcome.status, 500);
        assert_eq!(
            outcome.error.unwrap().message,
            "the function's response is larger than 1 MiB"
        );
    }

    #[tokio::test]
    async fn a_second_call_sees_no_global_of_the_first() {
        let set = "export default async () => { globalThis.leak = (globalThis.leak ?? 0) + 1; return { body: globalThis.leak }; };";
        assert_eq!(call(set).await.body, Some(json!(1)));
        assert_eq!(call(set).await.body, Some(json!(1)));
    }

    #[tokio::test]
    async fn only_the_files_can_be_imported_and_there_is_no_fetch_timer_or_process() {
        let outcome =
            call("import fs from 'node:fs';\nexport default async () => ({ body: 1 });").await;
        assert_eq!(outcome.status, 500);
        assert!(outcome.error.unwrap().message.contains("node:fs"));
        let outcome = call(
            "export default async () => ({ body: [typeof fetch, typeof setTimeout, typeof process, typeof require, typeof std, typeof os] });",
        )
        .await;
        assert_eq!(
            outcome.body,
            Some(json!([
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined"
            ]))
        );
    }

    #[tokio::test]
    async fn a_throw_names_its_file_and_line() {
        let outcome =
            call("export default async () => {\n  const a = null;\n  return a.b;\n};").await;
        let error = outcome.error.unwrap();
        assert_eq!(outcome.status, 500);
        assert_eq!(
            (error.file.as_deref(), error.line),
            (Some("@app/functions/f.ts"), Some(3)),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_request_outside_the_endpoint_is_answered_403_without_leaving() {
        let outcome = call(
            "export default async (r, ctx) => ({ body: await ctx.jc.raw({ method: 'GET', path: '/api/v1/projects/helsinki/changes' }) });",
        )
        .await;
        assert_eq!(outcome.body.unwrap()["status"], 403);
    }

    #[tokio::test]
    async fn ctx_jc_reads_the_endpoint_with_the_callers_token_or_anonymously() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let gateway = MockServer::start().await;
        let rows = json!([{ "id": "urn:ngsi-ld:A:hel.fi:mobility:1", "type": "A" }]);
        Mock::given(method("GET"))
            .and(path(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities")))
            .and(query_param("type", "A"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&rows))
            .mount(&gateway)
            .await;
        let function =
            "export default async (r, ctx) => ({ body: await ctx.jc.entities.list('A') });";

        let outcome = run(invocation(function, &gateway.uri(), Some("caller-token"))).await;
        assert_eq!(outcome.body, Some(rows.clone()), "{outcome:?}");
        let outcome = run(invocation(function, &gateway.uri(), None)).await;
        assert_eq!(outcome.body, Some(rows));

        let seen = gateway.received_requests().await.unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0].headers.get("authorization").unwrap(),
            "Bearer caller-token"
        );
        assert!(
            seen[1].headers.get("authorization").is_none(),
            "no token, no credential"
        );
    }

    #[test]
    fn a_stack_frame_gives_file_and_line() {
        assert_eq!(
            location(
                "    at handler (@app/functions/f.ts:12:5)\n    at <anonymous> (@jc/invoke:9:24)"
            ),
            (Some("@app/functions/f.ts".to_owned()), Some(12))
        );
        assert_eq!(
            location("    at @app/functions/f.ts:3:9"),
            (Some("@app/functions/f.ts".to_owned()), Some(3))
        );
        assert_eq!(
            location("    at <anonymous> (@jc/invoke:9:24)"),
            (None, None)
        );
    }
}
