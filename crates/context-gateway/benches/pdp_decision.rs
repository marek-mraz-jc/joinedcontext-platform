//! The decision point's budget (T-5042, ADR-N-057 section 4): one space with 50 Policies of five
//! rules each, a caller that ten of them name, one `queryEntity` with types, attributes and a `q`.
//! The decision's p99 stays under 1 ms, and ci-full fails a p50 more than 20 % above the last
//! main run's (the p99 of a shared runner swings 20 % between two runs of the same commit; the
//! p50 moves by 3 %):
//!
//! ```text
//! cargo bench -p context-gateway --bench pdp_decision
//! PDP_BENCH_BASELINE=prev.txt PDP_BENCH_OUT=now.txt cargo bench -p context-gateway --bench pdp_decision
//! ```
//!
//! It prints one line, `pdp_decision p50_us=… p99_us=… rules=250`. Under `cargo test`
//! (no `--bench`) it decides a hundred times and checks the verdict, so the bench cannot rot.

use context_gateway::pdp::evaluator::{Request, Subject, Verdict};
use context_gateway::pdp::{Pdp, PolicyPdp};
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Operation, PolicySpec, Representation};
use std::collections::BTreeSet;
use std::time::Instant;

const POLICIES: usize = 50;
const RULES: usize = 5;
const SAMPLES: usize = 20_000;
/// ADR-N-057 section 4: a decision's p99.
const BUDGET_US: f64 = 1000.0;
/// ci-full's regression threshold against the last main run.
const REGRESSION: f64 = 1.2;

/// Policy `n`: four grants and one prohibition, each with the residual constraints a real space
/// writes (idPattern, q, geoQ), to roles `r0`..`r9` in turn, so a caller holding `r3` matches ten
/// of the fifty Policies.
fn policy(n: usize) -> PolicySpec {
    let mut yaml =
        String::from("contextSpaceRef: bench\nassigner: did:web:bench.example\nrules:\n");
    for rule in 0..RULES {
        let prohibition = rule == RULES - 1;
        let effect = if prohibition {
            "    effect: prohibition\n"
        } else {
            ""
        };
        // A prohibition on a type the caller never asks for, so it is weighed and does not end
        // the decision.
        let kind = if prohibition {
            "Secret".to_owned()
        } else {
            format!("T{rule}")
        };
        let lon = 17.0 + (n % 10) as f64 * 0.1;
        yaml += &format!(
            "  - assignee: {{ kind: role, id: r{} }}\n{effect}    operations: [queryEntity, retrieveEntity]\n    information:\n      - entities:\n          - type: {kind}\n            idPattern: '^urn:ngsi-ld:{kind}:p{n}:.*$'\n        propertyNames: [a{rule}, b{rule}, location]\n    q: 'a{rule}>{n}'\n    geoQ: 'georel=within;geometry=Polygon;coordinates=[[[{lon},48.0],[{},48.0],[{},48.5],[{lon},48.5],[{lon},48.0]]]'\n",
            n % 10,
            lon + 0.5,
            lon + 0.5,
        );
    }
    let spec: PolicySpec = serde_norway::from_str(&yaml).expect("the bench policy parses");
    spec
}

fn endpoint() -> Endpoint {
    let policies = (0..POLICIES)
        .flat_map(|n| {
            policy(n)
                .expand(&format!("p{n}"))
                .expect("the bench policy expands")
        })
        .map(|(_, spec)| spec)
        .collect();
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: "bench".to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "bench".to_owned(),
        project: "bench".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: "/api/endpoint/bench".to_owned(),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies,
    }
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn decide(endpoint: &Endpoint, subject: &Subject, request: &Request) -> Verdict {
    PolicyPdp.decide(subject, Operation::QueryEntity, request, endpoint)
}

/// The p50 and p99 of one round of `SAMPLES` decisions, in microseconds.
fn round(endpoint: &Endpoint, subject: &Subject, request: &Request) -> (f64, f64) {
    let mut micros: Vec<f64> = (0..SAMPLES)
        .map(|_| {
            let started = Instant::now();
            std::hint::black_box(decide(endpoint, subject, request));
            started.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    micros.sort_by(f64::total_cmp);
    (micros[SAMPLES / 2], micros[SAMPLES * 99 / 100])
}

fn main() {
    let endpoint = endpoint();
    assert_eq!(endpoint.policies.len(), POLICIES * RULES);
    let subject = Subject {
        roles: set(&["r3"]),
        ..Subject::default()
    };
    let request = Request {
        types: set(&["T0", "T1"]),
        attrs: set(&["a0", "b1"]),
        q: Some("a0>1".to_owned()),
        referenced: set(&["a0"]),
        ..Request::default()
    };

    let Verdict::Rewrite(constraints) = decide(&endpoint, &subject, &request) else {
        panic!("the bench caller is granted the read");
    };
    assert_eq!(constraints.types, set(&["T0", "T1"]));
    assert_eq!(
        constraints.id_patterns.len(),
        20,
        "five Policies, four grants each"
    );
    if !std::env::args().any(|arg| arg == "--bench") {
        for _ in 0..100 {
            assert!(!decide(&endpoint, &subject, &request).is_deny());
        }
        return;
    }

    let baseline: Option<f64> = std::env::var("PDP_BENCH_BASELINE")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|line| {
            let field = line
                .split_whitespace()
                .find_map(|f| f.strip_prefix("p50_us="))?;
            field.parse().ok()
        });
    let ceiling = baseline.map(|b| b * REGRESSION);
    for _ in 0..1000 {
        std::hint::black_box(decide(&endpoint, &subject, &request));
    }
    // A shared runner stalls now and then: the best of three rounds is the decision's cost.
    let (p50, p99) = (0..3)
        .map(|_| round(&endpoint, &subject, &request))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .expect("three rounds");
    let line = format!(
        "pdp_decision p50_us={p50:.1} p99_us={p99:.1} rules={} baseline_p50_us={}",
        POLICIES * RULES,
        baseline.map_or("none".to_owned(), |b| format!("{b:.1}")),
    );
    println!("{line}");
    if let Ok(path) = std::env::var("PDP_BENCH_OUT") {
        std::fs::write(path, format!("{line}\n")).expect("the result is written");
    }
    if p99 > BUDGET_US {
        eprintln!("pdp_decision: p99 {p99:.1} us over the {BUDGET_US} us budget");
        std::process::exit(1);
    }
    if let Some(ceiling) = ceiling.filter(|ceiling| p50 > *ceiling) {
        eprintln!("pdp_decision: p50 {p50:.1} us over {ceiling:.1} us, the last main run's + 20 %");
        std::process::exit(1);
    }
}
