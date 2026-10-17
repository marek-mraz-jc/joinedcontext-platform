# Platform dead code: what no production entry point reaches

T-3918, GOAL point 11. The gateway surfaces moved to Antares, and each one has a delete task.
This inventory lists the code those deletes leave behind, and the code nothing called before
them, so no second path and no fallback flag stays in `joinedcontext-platform`.

## 1. Method

The check is `dev/cg-dead-code.py`; it reads the cg graph (`cg build --fg`) and computes the
reachable set in Python, because a variable-length Cypher walk over every entry point of the
workspace does not finish.

Entry points:

- every binary's `main`: `src/main.rs`, `src/bin/*.rs`, `build.rs`;
- every `pub` item of `jc-app-sdk`, and its `#[macro_export]` macro: the Apps link the SDK;
- every `pub` item of `jc-core` and `jcctl` whose name the Portal's `src/` names, a method only
  when its type is named too: the Portal links both crates. `context-gateway` is only a
  Portal dev-dependency, so the Portal names nothing into it;
- a function a serde attribute names by string (`default`, `skip_serializing_if`,
  `deserialize_with`, `serialize_with`, also `Type::f` and `f::<S, _>`), and a constant a format
  string captures (`"{PAGE_LIMIT}"`): cg has no edge for a string.

Axum handlers, Bento and plugin registrations need no entry of their own: a router is built
from `main`, so its handlers are reached through REFERENCES.

The three statements, run with `cg q --max-rows 1000000`:

```cypher
MATCH (s:Symbol) WHERE s.lang='rust' AND NOT s.external
RETURN s.id, s.path, s.kind, s.package, s.file, s.line, s.loc, s.visibility, s.is_test,
       s.trait_name, s.self_type;
MATCH (a:Symbol)-[r:CALLS|DISPATCHES|REFERENCES]->(b:Symbol)
WHERE a.lang='rust' AND NOT b.external RETURN a.id, b.id;
MATCH (a:Symbol)-[:CONTAINS]->(b:Symbol)
WHERE a.lang='rust' AND NOT a.external AND a.kind IN ['Struct','Enum'] RETURN a.id, b.id;
```

From the entries the walk follows CALLS, DISPATCHES and REFERENCES without a depth limit; a
type reached reaches its fields and variants (CONTAINS) and every trait impl whose `self_type`
is its name, in any crate (derives, `From` through `?`, `Display` in `format!`). A row is a
production symbol (`crates/*/src`, `is_test` false) of kind Function, Method, StaticMethod,
TraitMethod, Struct, Enum, Constant, StaticVariable, TypeAlias, Trait or Macro that the walk
does not reach.

## 2. Counts

Rows from the final checker on a graph built at `1fc2525` (before) and on this branch after the
slice (after):

| Crate | Rows before | Rows after |
|---|---|---|
| agent-proxy | 3 | 3 |
| assistant | 1 | 1 |
| context-gateway | 47 | 46 |
| jc-core | 56 | 47 |
| jcctl | 32 | 32 |
| wasm-host | 6 | 6 |
| **total** | **145** | **135** |

T-4451 (2026-10-17): the ratchet had run only as a self-test, so main drifted to jc-core 51 and
assistant 2 while context-gateway (39) and jcctl (31) fell below their lines unrecorded. After
the slice below, on a graph of the branch: agent-proxy 3, assistant 1, context-gateway 39,
jc-core 46, jcctl 31, wasm-host 6; the baseline holds these. T-4452 then took context-gateway
from 39 to 31; jcctl, wasm-host and agent-proxy rows are all reasoned below and stay.

`ci/cg-dead-code-baseline.txt` holds the after column. The check is red when a crate has more
rows than its line (new code nothing calls, or a caller deleted and its callees kept) and red
when it has fewer until the line is lowered, so the number only goes down. CI runs the
self-test (cg is not on the runner, as for `dev/cg-invariants.py`). The full check runs in
`tasks/push`: a batch branch of a repository that carries `dev/cg-dead-code.py`, whose diff
against main touches Rust or the baseline, is pushed only when the check is green on a fresh
`cg build --fg` of the branch merged with main (T-4451).

## 3. Rows and their reasons

Every row of the before column, by group. GREP below means `git grep -w` over this repo and the
Portal's `src/`.

### 3.1 Deleted on this branch

| Row | Why it was dead |
|---|---|
| `jc_core::kinds::dashboard::DashboardSpec::layer_refs` | no caller outside its own test |
| `jc_core::kinds::data_model::DataModelSpec::is_organization_model` | no caller, not even a test |
| `jc_core::kinds::mapping::MappingSpec::compiled_bloblang_path` | no caller outside its own test |
| `jc_core::kinds::policy::PolicySpec::residual_is_empty` | no caller outside its own test |
| `DataTheme::ALL`, `Frequency::ALL`, `OperationGroup::ALL` | no reader in this repo or the Portal |
| `jc_core::kinds::organization::Contact::new` | no caller, not even a test |
| `jc_core::envelope::de_api_version` | the single-version check `de_api_version_of` (PL-54) replaced; a second path |
| `context_gateway::store::endpoints_of` | every caller passes the model root to `endpoints_with_models` |

The test assertions on the deleted accessors went with them.

T-4451 deleted these too, each with no production caller:

| Row | Why it was dead |
|---|---|
| `jc_core::kinds::assistant::AssistantDeploymentSpec::router_model`, `answer_model`, `DEFAULT_MODEL` | the assistant reads `spec.model` itself and falls back to the worker's configured model (`llm`), a second default |
| `jc_core::kinds::service_account::ServiceAccountSpec::device_keys` | no reader in this repo or the Portal; `validate` keeps checking the keys |
| `assistant::chat::routed::answer_messages` | a wrapper only its tests called; the answer call uses `messages_with` |
| `jc_core::kinds::assistant::KnowledgeSourceSpec::upload_limits` | not deleted: the assistant's upload route repeated its body, and now calls it |

T-4452, context-gateway:

| Row | Why it was dead |
|---|---|
| `store::spaces_of`, `store::endpoints_with_models` | no caller, not even a test: the loader builds both tables from one `ModelSet` |
| `pdp::evaluator::Constraints::in_source_model` | the view-endpoint inversion (DM-51) left with the views for Antares; only its test called it |
| `federation::Federations::needing_caller_identity` | its test now reads `caller_members`, which the read path uses |
| `middleware::response::RESULTS_COUNT` | named nothing; the count passes through as the broker sent it |
| `middleware::response::WARNING` | the scrub passes the broker's warning through without naming it; the const moved into its test |
| `fanout::Index::notified`, `scanned` | not deleted: the test oracles of the index (the hub runs `candidates` and `decides` itself), moved into the test module |

### 3.2 Removed by an open delete task

These go with the surface that used them; deleting them here would collide with those tasks.

| Rows | Task |
|---|---|
| `federation/odrl_compiler.rs` (14 rows), `handlers/access_odrl.rs` (7 rows) | T-3650 |
| `pdp::evaluator::Verdict::constraints`, `pdp::projection::project` | T-3644 |
| `handlers::schema::Visible::only`, `only_each` | T-3646 |
| `app::Gateway::limit_hub_to` (the MCP hub's limiter) | T-3648 |
| `translators::tabular::Table::is_empty`, `translators::view_mapping::ViewMapping::source_of` | T-3652 |

### 3.3 Test API: kept

Constructors, doubles and inspectors the integration tests (`crates/*/tests`) use and production
does not. They are `pub` because an integration test is another crate; a `#[cfg(test)]` would
hide them from it.

- agent-proxy: `RunResolver::with_cached`, `with_cached_at`; `DataCredentials::revoke`.
- assistant: `worker::sources`.
- context-gateway: `Gateway::wait_for_writes`, `limit_anonymous_mcp_to` (the tests shorten the
  write wait and the anonymous MCP limit); `pdp::temporal::Window::admits`;
  `ServiceAccounts::is_empty`, `with_app`; `Agreements::len`, `is_empty`;
  `Federations::is_federated`; `RateLimiter::len`, `is_empty`; `WorkloadToken::new`;
  `Broker::new` (plain HTTP), `Broker::base`; `SlugResolver::with`, `len`, `is_empty`.
- jc-core: `ResourceEnvelope::new`, `strip_status`, `without_status`, `resource_path`, `to_yaml`,
  `to_json`, `from_json`; `ProblemDetails::internal_opaque`, `with_instance`;
  `MultiLanguageMap::new`, `insert`, `require_fallback`, `is_empty`, `len`, `iter`;
  `NativeBlock::new`, `MappingTest::new`; `Did::org_domain`; `kpi::Provenance::to`, `to_all` (cg names them `kpi::Property::to`),
  `KeyPerformanceIndicator::from_json`; `urn::EntityRef` and its `new`, `space`, `urn`.
- jcctl: `bento::InputContext::new`, `with_pipeline_spec`; `csr::InMemoryBroker` (6 rows),
  `platform::InMemory` (4 rows), `publish::ckan::InMemoryCkan` (9 rows);
  `commands::drift::write_adoptions`; `csr::forwards_caller_identity`;
  `ckan_datastore::missing_attributes`; `HttpBao::with_ca_file`; `SecretValue::len`,
  `SecretStore::load_file`, `source_of`; `ModelTools::generate`.
- wasm-host: `Host::cached`, `Host::serve` (production calls `serve_admitted`);
  `provision::{database, shard, owner_of, app}`, which `examples/fleet.rs` and the tests use.

### 3.4 cg blind spots: reached, kept

Checked with GREP; the graph has no edge for these uses.

| Row | The use cg does not see |
|---|---|
| `jc_core::envelope::Kind::PLURAL` | read as `<$spec as Kind>::PLURAL` inside the `catalogue!` macro |
| `jc_core::registry::catalogue` | a `macro_rules!`, invoked once in its own file |
| `urn::EntityRef::fmt`, `jcctl::commands::import::Reference::fmt`, `organization::String::from` | `Display` in `format!` and `From` through `.into()` on a type the walk reaches only from tests |
| `jc_core::kinds::bento_inputs::RUNNER_VERSION` | a generated file; the Portal's `bento-inputs.mjs` writes it |

### 3.5 Checks no production path calls: decision pending

Each one is a rule a requirement names and the reconciler or the Portal does not enforce. Deleting
it silently would drop the rule; wiring it is feature work. Each stays a row until its own task
decides.

| Row | Rule |
|---|---|
| `EndpointSpec::validate_opacity`, `EndpointSlug::is_opaque` | EP-03 opaque slugs; EP-75 minting already makes every new slug opaque |
| `SubscriptionSpec::validate_at` | a subscription's expiry against the clock |
| `pipeline::validate_processor` | a processor allowlist |
| `AppSpec::requires_red_lane`, `AppSpec::write_operations`, `SyncSourceSpec::requires_red_lane`, `MappingSpec::requires_elevated_lane` | the review-lane rules |
| `AgentEgress::max_bytes_per_run` | an agent's egress budget |
| `ServiceAccountSpec::has_api_key` and `context_gateway::auth::api_key` (7 rows, only `KEY_PREFIX` is read) | API keys beside Keycloak tokens, which the non-negotiables rule out |
| `DataModelSpec::can_be_referenced` | cross-organization model references |
| `AgreementState::allows_transition_to`, `DataAgreementSpec::allows_transition_to` | the agreement state machine |
| `ProjectsPolicy::name_cooldown_days` | a project-name cooldown |
| `jcctl::secrets::openbao::Session::lease_seconds`, `renewable` | lease renewal |
| `jc_core::qb::observation_local_id`, `LOCAL_ID_SEPARATOR` | the observation id Architecture/03 §2 names; no pipeline mints it yet |

## 4. Config keys and fallback flags

Env keys the charts set against what the code reads, from the deployment clone
(`components/{context-gateway,wasm-host,assistant,functions}`) and `crates/*/src`:

```bash
grep -rhoE '\bJC_[A-Z0-9_]+' ../joinedcontext-deployment/components/{context-gateway,wasm-host,assistant,functions} | sort -u
git grep -hoE '\bJC_[A-Z0-9_]+' -- 'crates/*/src' | sort -u
```

71 keys are set; 64 are read. The other seven (`JC_FUNCTIONS_URL`, `JC_LANE_TOKEN`,
`JC_PORTAL_APPS_BUCKET`, `JC_PORTAL_APPS_DIR`, `JC_PORTAL_GATEWAY_URL`, `JC_PORTAL_PUBLIC_URL`,
`JC_PORTAL_WASM_SHARDS`) appear only in chart comments that name the Portal's variables; no
workload of this repository is given a key it ignores.

No `fallback`, `legacy`, `old_` or `_v1` flag is read. `git grep -niE 'fallback|legacy|old_|_v1'
-- 'crates/*/src/*'` gives 80 lines: locale fallbacks (PF-25), axum's `Router::fallback`,
`hyper_util::client::legacy`, default values named `fallback` (a type list, an `observedAt`),
the `v1alpha1` API version, and the ODRL compiler T-3650 deletes. None is an env key or a
config field that picks between two paths.

## Related

- `dev/cg-dead-code.py`, `ci/cg-dead-code-baseline.txt`
- `dev/cg-invariants.py`, the security invariants on the same graph (T-3682)
- T-3643, T-3644, T-3646, T-3648, T-3650, T-3652, T-3654
