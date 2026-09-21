//! Edge cases of `sandbox::run` and `execute` (T-2500, SDK-22, SDK-23).
//!
//! **The contract.** One invocation runs in a fresh QuickJS runtime capped at 64 MiB, 5 s and a
//! 1 MiB stack. Whatever the code does ends as an `Outcome`: a status from 100 to 599, an object
//! answer, at most 200 log lines of at most 2000 characters, and never a panic.
//!
//! **Inputs.** `files`, `entry`, the request and config JSON, and what the function's code
//! returns, throws or logs. The module's own tests cover an endless loop, 100 MiB, an answer over
//! 1 MiB, globals between calls, imports, a throw's file and line, and a request outside the
//! endpoint; these are the gaps.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use functions::endpoint::Endpoint;
use functions::sandbox::{run, Invocation, Outcome, SDK_SERVER, TIME_LIMIT};
use serde_json::json;

const SLUG: &str = "k7m2qz4tv6xh3n5jb2ryd3wcfa";
const ENTRY: &str = "@app/functions/f.ts";
const SDK: &str = "export const createClient = (config, transport) => ({ raw: transport });";

fn invocation(files: &[(&str, &str)], entry: &str, gateway: &str) -> Invocation {
    let mut all: BTreeMap<String, String> = files
        .iter()
        .map(|(name, source)| ((*name).to_owned(), (*source).to_owned()))
        .collect();
    all.insert(SDK_SERVER.to_owned(), SDK.to_owned());
    Invocation {
        files: all,
        entry: entry.to_owned(),
        request: json!({ "method": "POST", "query": {}, "body": null, "user": null }),
        config: json!({ "slug": SLUG }),
        endpoint: Endpoint {
            http: reqwest::Client::new(),
            gateway: gateway.to_owned(),
            slug: SLUG.to_owned(),
            token: None,
        },
    }
}

async fn call(function: &str) -> Outcome {
    run(invocation(
        &[(ENTRY, function)],
        ENTRY,
        "http://127.0.0.1:9",
    ))
    .await
}

fn message(outcome: &Outcome) -> String {
    outcome
        .error
        .as_ref()
        .map(|e| e.message.clone())
        .unwrap_or_default()
}

/// SDK-22: a promise nobody resolves is ended by the wall clock, not waited on for ever.
#[tokio::test]
async fn a_promise_that_never_resolves_is_stopped_by_the_wall_clock() {
    let started = Instant::now();
    let outcome = call("export default async () => { await new Promise(() => {}); };").await;
    assert!(
        started.elapsed() < TIME_LIMIT + Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(outcome.status, 500, "{outcome:?}");
    assert!(message(&outcome).contains("longer than"), "{outcome:?}");
}

/// SDK-22: a gateway that takes the call and never answers is bounded by the same clock.
#[tokio::test]
async fn a_host_request_to_a_gateway_that_never_answers_is_stopped_by_the_wall_clock() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            open.push(socket);
        }
    });
    let started = Instant::now();
    let outcome = run(invocation(
        &[(
            ENTRY,
            &format!("export default async (r, ctx) => ({{ body: await ctx.jc.raw({{ method: 'GET', path: '/api/endpoint/{SLUG}/access' }}) }});"),
        )],
        ENTRY,
        &format!("http://{address}"),
    ))
    .await;
    held.abort();
    assert!(
        started.elapsed() < TIME_LIMIT + Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(outcome.status, 500, "{outcome:?}");
    assert!(message(&outcome).contains("longer than"), "{outcome:?}");
}

/// SDK-22: recursion without an end meets the 1 MiB stack as an error of the function.
#[tokio::test]
async fn unbounded_recursion_hits_the_stack_limit_and_answers_500_without_crashing_the_process() {
    let outcome =
        call("const f = (n) => f(n + 1) + 1; export default async () => ({ body: f(0) });").await;
    assert_eq!(outcome.status, 500, "{outcome:?}");
    assert!(!message(&outcome).is_empty(), "{outcome:?}");
    // The process is still here, and the next invocation runs.
    let next = call("export default async () => ({ body: 1 });").await;
    assert_eq!(next.status, 200, "{next:?}");
}

/// SDK-23: logs are bounded: 200 lines, each cut at 2000 characters.
#[tokio::test]
async fn the_201st_log_line_is_dropped_and_a_line_is_cut_at_2000_characters() {
    let outcome = call(
        "export default async (r, ctx) => { ctx.log('x'.repeat(5000)); for (let i = 1; i < 250; i++) ctx.log(`line ${i}`); return {}; };",
    )
    .await;
    assert_eq!(outcome.status, 200, "{outcome:?}");
    assert_eq!(outcome.logs.len(), 200);
    assert_eq!(outcome.logs[0].chars().count(), 2000);
    assert_eq!(outcome.logs[199], "line 199");

    let outcome =
        call("export default async (r, ctx) => { ctx.log('ž'.repeat(3000)); return {}; };").await;
    assert_eq!(
        outcome.logs[0].chars().count(),
        2000,
        "cut at characters, not bytes"
    );
}

/// SDK-23: a status is an integer from 100 to 599, and a refusal says so.
#[tokio::test]
async fn a_status_of_99_600_or_a_string_is_refused_by_name() {
    for status in ["99", "600", "'200'", "200.5", "-1", "true", "[200]", "1e3"] {
        let outcome = call(&format!(
            "export default async () => ({{ status: {status} }});"
        ))
        .await;
        assert_eq!(outcome.status, 500, "{status}: {outcome:?}");
        assert!(
            message(&outcome).contains("status"),
            "{status}: {outcome:?}"
        );
    }
    for status in ["100", "599", "null"] {
        let outcome = call(&format!(
            "export default async () => ({{ status: {status} }});"
        ))
        .await;
        assert_ne!(outcome.status, 500, "{status}: {outcome:?}");
    }
}

/// SDK-23: the answer is an object; anything else is refused, and nothing undefined is an object.
#[tokio::test]
async fn an_array_or_a_string_returned_instead_of_an_object_is_refused() {
    for value in ["[]", "[1, 2]", "'ok'", "42", "null", "true"] {
        let outcome = call(&format!("export default async () => {value};")).await;
        assert_eq!(outcome.status, 500, "{value}: {outcome:?}");
        assert!(message(&outcome).contains("object"), "{value}: {outcome:?}");
    }
    let outcome = call("export default async () => undefined;").await;
    assert_eq!(
        outcome.status, 200,
        "no answer is an empty one: {outcome:?}"
    );
}

/// SDK-23: an entry that is not among the files is named in the refusal.
#[tokio::test]
async fn an_entry_that_is_not_among_the_files_answers_500_naming_it() {
    let outcome = run(invocation(
        &[(ENTRY, "export default async () => ({});")],
        "@app/functions/missing.ts",
        "http://127.0.0.1:9",
    ))
    .await;
    assert_eq!(outcome.status, 500);
    assert!(
        message(&outcome).contains("@app/functions/missing.ts"),
        "{outcome:?}"
    );
}

/// SDK-23: a function that tampers with the runtime's output slot gets a refusal, never a panic.
#[tokio::test]
async fn a_function_that_overwrites_jc_output_with_non_json_is_refused_not_a_panic() {
    for trick in [
        "Object.defineProperty(globalThis, '__jc_output', { get() { return 'not json'; }, set() {} });",
        "Object.defineProperty(globalThis, '__jc_output', { get() { return 42; }, set() {} });",
        "Object.defineProperty(globalThis, '__jc_output', { get() { throw new Error('no'); }, set() {} });",
        "globalThis.JSON.stringify = () => undefined;",
    ] {
        let outcome = call(&format!("export default async () => {{ {trick} return {{ body: 1 }}; }};")).await;
        assert_eq!(outcome.status, 500, "{trick}: {outcome:?}");
    }
    let outcome = call("export default async () => ({ body: 10n });").await;
    assert_eq!(
        outcome.status, 500,
        "a BigInt does not serialize: {outcome:?}"
    );
}

/// SDK-23: a throw of something that is not an Error still reads as a sentence.
#[tokio::test]
async fn a_throw_of_a_string_or_undefined_answers_500_with_a_readable_message() {
    for thrown in [
        "'broken'",
        "undefined",
        "null",
        "42",
        "{ code: 7 }",
        "new Error('')",
    ] {
        let outcome = call(&format!(
            "export default async () => {{ throw {thrown}; }};"
        ))
        .await;
        assert_eq!(outcome.status, 500, "{thrown}");
        let message = message(&outcome);
        assert!(!message.is_empty(), "{thrown}: {outcome:?}");
        assert!(!message.contains("CaughtError"), "{thrown}: {message}");
    }
}

/// SDK-22: an import resolves to the files key it names and nothing else, `..` included.
#[tokio::test]
async fn an_import_of_a_files_key_with_dot_dot_resolves_only_to_that_exact_key() {
    let files = [
        (
            ENTRY,
            "import secret from '../secret.ts'; export default async () => ({ body: secret });",
        ),
        (
            "../secret.ts",
            "export default 'the key named ../secret.ts';",
        ),
        ("@app/secret.ts", "export default 'a neighbour'"),
    ];
    let outcome = run(invocation(&files, ENTRY, "http://127.0.0.1:9")).await;
    assert_eq!(
        outcome.body,
        Some(json!("the key named ../secret.ts")),
        "{outcome:?}"
    );

    for import in [
        "../../etc/passwd",
        "./secret.ts",
        "@app/functions/../secret.ts",
        "/etc/passwd",
    ] {
        let outcome = call(&format!(
            "import x from '{import}'; export default async () => ({{ body: x }});"
        ))
        .await;
        assert_eq!(outcome.status, 500, "{import}: {outcome:?}");
    }
}
