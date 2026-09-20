//! Edge cases of `auth::accounts::accounts_of` (T-1892, EP-26, MP-02).
//!
//! Contract, in one sentence: the table maps a Keycloak `azp` to exactly one ServiceAccount of
//! exactly one project, and an `azp` that could mean two accounts, or that no manifest declares,
//! resolves to nobody — never to whichever manifest happened to be read last (T-1454, PF-46).
//!
//! `azp_resolves_to_the_service_account_the_repository_declares` in `token_tests.rs` covers the
//! happy path. These are the cases where it would be dangerous to guess.

use context_gateway::auth::accounts::{accounts_of, client_id};
use std::path::PathBuf;

/// A repository directory of its own per test, removed on the way out.
struct Repo(PathBuf);

impl Repo {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gateway-accounts-edge-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a repository");
        Self(dir)
    }

    /// One ServiceAccount manifest, written where the loader looks for it.
    fn account(&self, project: &str, name: &str, body: &str) -> &Self {
        self.space(project, "mobility", &format!("{project}-mobility"));
        let dir = self
            .0
            .join(format!("projects/{project}/access/serviceaccounts"));
        std::fs::create_dir_all(&dir).expect("the account directory");
        std::fs::write(dir.join(format!("{name}.yaml")), body).expect("the account is written");
        self
    }

    /// A ServiceAccount with the roles given as YAML, or none.
    fn simple(&self, project: &str, name: &str, roles: &str) -> &Self {
        self.space(project, "mobility", &format!("{project}-mobility"));
        self.account(
            project,
            name,
            &format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: {name}\n  namespace: {project}\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"edge cases\"\n  roles:\n{roles}  credentials:\n    - kind: oauth-client\n      name: default\n",
            ),
        )
    }

    fn space(&self, project: &str, name: &str, segment: &str) -> &Self {
        let dir = self.0.join(format!("projects/{project}/spaces/{name}"));
        std::fs::create_dir_all(&dir).expect("the space directory");
        std::fs::write(
            dir.join("space.yaml"),
            format!("apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {name}\n  namespace: {project}\nspec:\n  urnSegment: {segment}\n"),
        )
        .expect("the space is written");
        self
    }

    fn load(&self) -> jcctl::loader::Repository {
        jcctl::loader::Repository::load(&self.0).expect("the repository loads")
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_repository_with_no_service_accounts_has_an_empty_table() {
    let repo = Repo::new("empty");
    let accounts = accounts_of(&repo.load());
    assert!(accounts.is_empty());
    assert_eq!(accounts.len(), 0);
    assert!(accounts.resolve("").is_none());
    assert!(accounts.resolve("anything-at-all").is_none());
}

#[test]
fn two_accounts_that_derive_one_client_id_resolve_to_nobody() {
    // `helsinki` + `kpi-writer` and `helsinki-kpi` + `writer` are the same client id, and the
    // hyphen cannot say which (T-1454). Handing a token either account's roles would be handing a
    // project's grants to another project's workload.
    let repo = Repo::new("ambiguous");
    repo.simple(
        "helsinki",
        "kpi-writer",
        "    - role: writer\n      scope:\n        organization: hel\n",
    );
    repo.simple(
        "helsinki-kpi",
        "writer",
        "    - role: owner\n      scope:\n        organization: hel\n",
    );
    assert_eq!(
        client_id("helsinki", "kpi-writer"),
        client_id("helsinki-kpi", "writer")
    );

    let accounts = accounts_of(&repo.load());
    assert!(
        accounts.resolve("helsinki-kpi-writer").is_none(),
        "an ambiguous client id resolves to nobody, not to the last manifest read",
    );
    assert_eq!(accounts.len(), 0);
}

#[test]
fn a_third_account_does_not_revive_an_ambiguous_client_id() {
    // The order the loader reads files in must not decide: whichever way round, the ambiguous id
    // stays unresolvable, and an unrelated account beside it still resolves.
    let repo = Repo::new("ambiguous-plus-one");
    repo.simple(
        "helsinki",
        "kpi-writer",
        "    - role: writer\n      scope:\n        organization: hel\n",
    );
    repo.simple(
        "helsinki-kpi",
        "writer",
        "    - role: owner\n      scope:\n        organization: hel\n",
    );
    repo.simple(
        "espoo",
        "collector",
        "    - role: reader\n      scope:\n        organization: hel\n",
    );

    let accounts = accounts_of(&repo.load());
    assert!(accounts.resolve("helsinki-kpi-writer").is_none());
    let other = accounts
        .resolve("espoo-collector")
        .expect("the unrelated account resolves");
    assert_eq!(other.project, "espoo");
    assert_eq!(other.name, "collector");
}

#[test]
fn an_account_whose_spec_cannot_be_read_is_left_out_rather_than_guessed() {
    let repo = Repo::new("bad-spec");
    // `roles` as a string instead of a list: the spec does not deserialize.
    repo.account(
        "helsinki",
        "broken",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: broken\n  namespace: helsinki\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"edge\"\n  roles: everything\n",
    );
    repo.simple(
        "helsinki",
        "good",
        "    - role: reader\n      scope:\n        organization: hel\n",
    );

    let accounts = accounts_of(&repo.load());
    assert!(
        accounts.resolve("helsinki-broken").is_none(),
        "a manifest that cannot be read grants nothing",
    );
    assert!(accounts.resolve("helsinki-good").is_some());
}

#[test]
fn a_space_scope_is_stored_as_the_segment_the_gateway_sees() {
    // PF-84: the gateway knows a space by its segment, so a role scoped to `ovzdusie` must reach the
    // call whose space is that space's segment and no other.
    let repo = Repo::new("segment");
    // The space pins a segment that is not its manifest name, which is the only way to tell the two
    // apart in an assertion.
    repo.space("helsinki", "waste", "hel-waste");
    repo.simple(
        "helsinki",
        "etl",
        "    - role: space-writer\n      scope:\n        contextSpace: waste\n",
    );

    let accounts = accounts_of(&repo.load());
    let account = accounts.resolve("helsinki-etl").expect("azp resolves");
    let here = account.roles_in("helsinki", "hel-waste");
    assert!(
        here.contains("space-writer"),
        "the segment is what the call carries"
    );
    assert!(
        account.roles_in("helsinki", "waste").is_empty(),
        "the manifest name is not the segment and must not be accepted as one",
    );
}

#[test]
fn an_account_with_no_roles_resolves_and_grants_nothing() {
    let repo = Repo::new("no-roles");
    repo.simple("helsinki", "silent", "    []\n");

    let accounts = accounts_of(&repo.load());
    let account = accounts.resolve("helsinki-silent").expect("azp resolves");
    assert!(account.roles.is_empty());
    assert!(account.roles_in("helsinki", "mobility").is_empty());
}

#[test]
fn an_account_manifest_without_a_roles_field_is_left_out_entirely() {
    // `roles` is a required field of `ServiceAccountSpec`, so a manifest that omits it cannot be
    // read, and `accounts_of` leaves it out rather than treating it as an account with no roles.
    // An account that cannot be read grants nothing, which is the safe direction.
    let repo = Repo::new("no-roles-field");
    repo.account(
        "helsinki",
        "silent",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ServiceAccount\nmetadata:\n  name: silent\n  namespace: helsinki\nspec:\n  owner:\n    user: demo.steward\n  purpose: \"edge\"\n  credentials:\n    - kind: oauth-client\n      name: default\n",
    );

    let accounts = accounts_of(&repo.load());
    assert!(accounts.resolve("helsinki-silent").is_none());
}

#[test]
fn an_azp_that_is_a_prefix_a_suffix_or_the_bare_name_resolves_to_nobody() {
    let repo = Repo::new("near-miss");
    repo.simple(
        "helsinki",
        "etl",
        "    - role: reader\n      scope:\n        organization: hel\n",
    );
    let accounts = accounts_of(&repo.load());

    assert!(accounts.resolve("helsinki-etl").is_some());
    for near in [
        "etl",
        "helsinki",
        "helsinki-et",
        "helsinki-etl2",
        "helsinki-etl ",
        " helsinki-etl",
        "Helsinki-etl",
        "HELSINKI-ETL",
        "helsinki--etl",
        "helsinki%2detl",
        "helsinki-etl\n",
        "helsinki-etl\0",
    ] {
        assert!(
            accounts.resolve(near).is_none(),
            "{near:?} is not the client id and must resolve to nobody",
        );
    }
}
