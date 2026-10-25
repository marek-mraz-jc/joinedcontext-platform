//! Who a request is: the token or key it carries turned into the Subject the PDP judges, and the
//! admission of that Subject on an Endpoint or a Context Space (GW1, EP-03).

use crate::app::Gateway;
use crate::auth::dataspace_token::{self};
use crate::auth::token::{self, Claims};
use crate::resolver::{Endpoint, Space};
use crate::{middleware::tenancy, query};
use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Response};
use axum::response::IntoResponse;
use jc_core::kinds::{Audience, Operation, Representation};
use jc_core::ProblemDetails;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Who is calling, as established by the PEP — never as claimed by the client (GW20).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Subject {
    /// The authenticated user, absent for an anonymous caller.
    pub user: Option<String>,
    /// The service account, when a workload is calling (PF-34).
    pub service_account: Option<String>,
    /// The project that service account belongs to: a `{project}/{name}` assignee matches only
    /// that project's account (PL-92).
    pub service_account_project: Option<String>,
    /// The groups the token asserts.
    pub groups: BTreeSet<String>,
    /// The roles the token asserts. An anonymous caller holds exactly `public` (GW22).
    pub roles: BTreeSet<String>,
    /// The decentralized identifier of a data-space participant, when one is calling.
    pub did: Option<String>,
    /// The data space agreement this caller acts under, which every audit record produced
    /// under it carries (DS-13). Never a grant of its own: what the agreement allows is in
    /// the `Policy` entities compiled from it.
    pub agreement: Option<String>,
    /// The service account a person's call came through, when a delegating client exchanged
    /// their token (ADR-N-038, AG-95). The audit line names it; it grants nothing, so it is
    /// never matched as a principal.
    pub via: Option<String>,
    /// The Keycloak client a person's token was issued to (`azp`): `portal-ui` makes a write's
    /// channel `ui` (ADR-N-054 2.3). Grants nothing.
    pub client: Option<String>,
    /// The Organization of the Project the caller was admitted under, for a write's provenance
    /// (ADR-N-054 2.3). Grants nothing.
    pub organization: Option<String>,
}

impl Subject {
    /// The anonymous caller: the role `public` and nothing else (GW22).
    pub fn anonymous() -> Self {
        Self {
            roles: BTreeSet::from(["public".to_owned()]),
            ..Self::default()
        }
    }
}

/// Establishes who is calling, from the verified token or from nobody (PF-45, PF-46).
///
/// The three answers are: a verified principal, the anonymous `public` role on an endpoint
/// that admits it, or 401. There is no fourth answer where a claim the client made is
/// believed without a signature behind it.
pub(crate) fn authenticate(
    gateway: &Gateway,
    endpoint: &Endpoint,
    headers: &HeaderMap,
) -> Result<Subject, Box<ProblemDetails>> {
    authenticate_on(gateway, endpoint, headers, false)
}

/// [`authenticate`] on an Endpoint's NGSI-LD surface when `federated`, which also admits a token
/// the gateway exchanged for the broker the Endpoint's space lives on (T-3999, PF-114).
pub(crate) fn authenticate_on(
    gateway: &Gateway,
    endpoint: &Endpoint,
    headers: &HeaderMap,
    federated: bool,
) -> Result<Subject, Box<ProblemDetails>> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    let raw = match token::bearer(presented) {
        Ok(raw) => raw,
        // No token at all: the anonymous caller, which only a public endpoint admits
        // (EP-16, GW22).
        Err(token::Rejected::NoToken) if endpoint.admits(None) => {
            return Ok(anonymous_on(endpoint));
        }
        Err(rejected) => return Err(Box::new(rejected.into())),
    };

    // A token was presented and the gateway has no realm to check it against. Believing
    // it would be believing the client.
    let Some(verifier) = &gateway.verifier else {
        tracing::warn!("a token was presented but no realm is configured");
        return Err(Box::new(ProblemDetails::unauthorized()));
    };
    let endpoint_audiences = gateway.audiences_for(endpoint);
    let mut audiences = endpoint_audiences.clone();
    audiences.push(CLI_AUDIENCE.to_owned());
    if federated {
        audiences.push(gateway.broker_audience(endpoint));
    }
    let claims = verifier
        .verify(raw, &audiences)
        .map_err(|rejected| Box::new(ProblemDetails::from(rejected)))?;
    if claims.names_audience(&endpoint_audiences) {
        return subject_from(gateway, endpoint, &claims);
    }
    if claims.names_audience(&[CLI_AUDIENCE.to_owned()]) {
        return cli_subject(gateway, endpoint, &claims);
    }
    exchanged_subject(gateway, endpoint, &claims)
}

/// A token naming only the broker's audience is a federated read's (T-3999, PF-114): the
/// gateway's own exchange alone mints one, and it reads as the person it names.
fn exchanged_subject(
    gateway: &Gateway,
    endpoint: &Endpoint,
    claims: &Claims,
) -> Result<Subject, Box<ProblemDetails>> {
    if claims.azp.as_deref() != Some(tenancy::GATEWAY_CLIENT) {
        tracing::warn!(
            azp = claims.azp.as_deref().unwrap_or_default(),
            slug = %endpoint.slug,
            "a broker-audience token not from the gateway's exchange"
        );
        return Err(Box::new(ProblemDetails::unauthorized()));
    }
    let mut subject = subject_from(gateway, endpoint, claims)?;
    if subject.user.is_none() {
        return Err(Box::new(ProblemDetails::unauthorized()));
    }
    subject.via = Some(tenancy::GATEWAY_CLIENT.to_owned());
    Ok(subject)
}

/// A person's `jcctl token --resource` token (T-5614, PF-45, PF-46): the `jcctl` client's
/// audience reaches an Endpoint only with the `endpoint:{slug}` scope naming that one, so a
/// script holds nothing broader than one Endpoint, and the Endpoint's Policy decides by the
/// person. `endpoint:*` is the hub's consent and reaches nothing here.
fn cli_subject(
    gateway: &Gateway,
    endpoint: &Endpoint,
    claims: &Claims,
) -> Result<Subject, Box<ProblemDetails>> {
    let picked = claims.azp.as_deref() == Some(CLI_CLIENT)
        && claims.endpoint_scopes().any(|slug| slug == endpoint.slug);
    if !picked {
        tracing::info!(slug = %endpoint.slug, "a jcctl-audience token without this Endpoint's scope");
        return Err(Box::new(ProblemDetails::unauthorized()));
    }
    let subject = subject_from(gateway, endpoint, claims)?;
    if subject.user.is_none() {
        return Err(Box::new(ProblemDetails::unauthorized()));
    }
    Ok(subject)
}

/// The subject a verified token is on `endpoint`: who it names, admitted by the Endpoint's
/// audience, with the Endpoint's own roles (PF-46, EP-14, AP-97).
pub(crate) fn subject_from(
    gateway: &Gateway,
    endpoint: &Endpoint,
    claims: &Claims,
) -> Result<Subject, Box<ProblemDetails>> {
    // An App's client reaches the Endpoints its App reads and no other, whatever audience its
    // token names (AP-113): the same answer as a token bound to another resource.
    if let Some(azp) = claims.azp.as_deref() {
        if !gateway.accounts.load().admits_on(azp, &endpoint.slug) {
            tracing::info!(azp, slug = %endpoint.slug, "an App client's token on an Endpoint its App does not read");
            return Err(Box::new(ProblemDetails::unauthorized()));
        }
    }
    let client_roles = endpoint
        .roles
        .app_client
        .as_deref()
        .map(|client| claims.client_roles(client))
        .unwrap_or_default();
    let mut subject = subject_of(claims, endpoint, gateway)?;
    with_endpoint_roles(&mut subject, endpoint, &client_roles);
    Ok(subject)
}

/// The roles an Endpoint gives exist on requests through it alone (AP-97). One that a token or
/// an account asserts is dropped whatever its source, so no realm role, no ServiceAccount
/// template and no other endpoint can carry an application's grant; then this endpoint's own
/// are added for the caller it admitted: on an App's Endpoint, from `client_roles`, the roles
/// the token of that App's own client carries (ADR-N-030).
/// The anonymous caller on `endpoint`, with the roles the Endpoint hands everyone (GW22,
/// AP-96). Only an Endpoint that admits nobody in particular admits it.
pub(crate) fn anonymous_on(endpoint: &Endpoint) -> Subject {
    let mut subject = Subject::anonymous();
    with_endpoint_roles(&mut subject, endpoint, &[]);
    subject
}

fn with_endpoint_roles(subject: &mut Subject, endpoint: &Endpoint, client_roles: &[String]) {
    subject
        .roles
        .retain(|role| !role.starts_with(jc_core::kinds::ENDPOINT_ROLE_PREFIX));
    let held: Vec<String> = endpoint
        .roles
        .held_by(subject.user.as_deref(), &subject.groups, client_roles)
        .map(str::to_owned)
        .collect();
    subject.roles.extend(held);
}

/// The audience of a person signed in at the edge (ADR-N-019): the gateway's own name, accepted
/// on every endpoint, because a session cannot name an endpoint approved after the login. The
/// Policy decision stays per endpoint (PF-46).
pub const EDGE_AUDIENCE: &str = "context-gateway";

/// The audience of every token of the `jcctl` client besides the Portal's (T-5614): on its own
/// it reaches no Endpoint; with an `endpoint:{slug}` scope, the one that scope names.
pub const CLI_AUDIENCE: &str = "jcctl-endpoint";

/// The public Keycloak client of the command line, the only one whose token [`CLI_AUDIENCE`]
/// admits (deployment `components/portal/keycloak-clients.yaml`).
pub const CLI_CLIENT: &str = "jcctl";

/// The audience of the Portal's token on `access/simulate` (EP-103), which no data path
/// accepts: a token minted for it reaches that route and nothing else.
pub const SIMULATE_AUDIENCE: &str = "context-gateway-simulate";

/// The Portal's Keycloak client, the one caller `access/simulate` answers (EP-103, option A of
/// T-3311), and only with its own service account's token.
// ponytail: a constant, as the deployment names the client `portal-api`; an env var when a
// deployment renames it.
pub const PORTAL_CLIENT: &str = "portal-api";

/// The hub's Keycloak client, which is also the audience of the tokens it obtains (ADR-N-025
/// section 4, EP-88).
pub const HUB_AUDIENCE: &str = "mcp-hub";

/// Where the hub answers (EP-87).
pub const HUB_PATH: &str = "/api/mcp";

/// The slug, the public resource URI when the deployment names one, and the edge audience.
pub(crate) fn audiences_of(slug: &str, base_path: &str, public_url: Option<&str>) -> Vec<String> {
    let mut audiences = vec![slug.to_owned()];
    if let Some(base) = public_url {
        audiences.push(format!("{base}{base_path}"));
    }
    audiences.push(EDGE_AUDIENCE.to_owned());
    audiences
}

/// What every App job's client id carries, `{project}-appjob-{app}` (AP-159).
const JOB_CLIENT_MARK: &str = "-appjob-";

/// Turns verified claims into the subject the PDP evaluates.
pub(crate) fn subject_of(
    claims: &Claims,
    endpoint: &Endpoint,
    gateway: &Gateway,
) -> Result<Subject, Box<ProblemDetails>> {
    // A data space consumer, before anything looks at `azp`. The connector obtained this
    // token with its own ServiceAccount, and resolving that account would hand the consumer
    // the connector's grants instead of the agreement's (DS-01, DS-02).
    if dataspace_token::presented(claims) {
        return dataspace_token::subject(
            claims,
            gateway.agreements.load().as_ref(),
            endpoint,
            crate::pdp::now(),
        )
        .map_err(|refused| Box::new(ProblemDetails::from(refused)));
    }

    let groups: BTreeSet<String> = claims
        .groups
        .iter()
        .map(|group| group.trim_start_matches('/').to_owned())
        .collect();

    // A workload: `azp` has to name a ServiceAccount this repository declares, or the
    // token is valid and the account is unknown, which is an account with no grants
    // (PF-46).
    let mut via = None;
    if let Some(azp) = claims.azp.as_deref() {
        let accounts = gateway.accounts.load();
        let account = accounts.resolve(azp);
        // A person's token the account's client exchanged (ADR-N-038, AG-95): the subject is
        // a person, not the client's own service-account user. Decided as that person when the
        // account declares delegation, and refused when it does not, so no client can carry a
        // person's rights, or its own rights under a person's name, without saying so.
        let for_a_person = claims
            .preferred_username
            .as_deref()
            .is_some_and(|user| !user.eq_ignore_ascii_case(&format!("service-account-{azp}")));
        // An App job's client (AP-159) is its derived account or nobody: a token of one the
        // table does not hold (its App gone, or a colliding client id) is never decided as the
        // person or the `service-account-…` user it names (T-3693).
        if account.is_none() && azp.contains(JOB_CLIENT_MARK) {
            tracing::warn!(
                azp,
                "a token from an App job's client no App of the repository runs"
            );
            return Err(Box::new(ProblemDetails::forbidden()));
        }
        match account {
            Some(account) if for_a_person && account.delegates => {
                via = Some(account.name.clone());
            }
            Some(_) if for_a_person => {
                tracing::warn!(
                    azp,
                    "a person's token from a client whose account delegates nothing"
                );
                return Err(Box::new(ProblemDetails::forbidden()));
            }
            _ => {}
        }
        if let Some(account) = account.filter(|_| via.is_none()) {
            if !endpoint.admits(Some(&account.project)) {
                // No decision line follows a refusal here, so this is the one that names it
                // (T-3033): the account, the endpoint, the project; never the token.
                tracing::warn!(
                    azp,
                    slug = %endpoint.slug,
                    project = %account.project,
                    "the account's project is not one this endpoint admits"
                );
                return Err(Box::new(ProblemDetails::forbidden()));
            }
            if !of_its_organization(gateway, endpoint, &account.project) {
                tracing::warn!(
                    azp,
                    slug = %endpoint.slug,
                    project = %account.project,
                    "the account's project is of another organization"
                );
                return Err(Box::new(ProblemDetails::not_found()));
            }
            return Ok(Subject {
                user: None,
                service_account: Some(account.name.clone()),
                service_account_project: Some(account.project.clone()),
                roles: roles_on(
                    endpoint.audience,
                    account.roles_in(&account.project, &endpoint.space),
                ),
                // A workload's grants are its manifest's (PF-35, T-2545): a group the token
                // claims is Keycloak membership nobody reviewed, so it reaches no group Policy.
                groups: BTreeSet::new(),
                did: None,
                agreement: None,
                via: None,
                client: None,
                organization: gateway.organization_of(&account.project),
            });
        }
        if via.is_none() && claims.preferred_username.is_none() {
            tracing::warn!(azp, "token from a client no ServiceAccount manifest names");
            return Err(Box::new(ProblemDetails::forbidden()));
        }
    }

    // A human. The token says they are a member of the organization; a group names the
    // project when the endpoint's audience is a list of them (EP-14, EP-15).
    let Some(user) = claims.preferred_username.clone() else {
        return Err(Box::new(ProblemDetails::unauthorized()));
    };
    // A client's own service-account user (a Policy assignee such as the Portal's derived
    // `service-account-portal-reconciler`) is that client's token or nobody's: the guard sits
    // here, not in the realm's username rules (T-5358).
    if !owns_its_name(&user, claims.azp.as_deref()) {
        tracing::warn!(
            user = %user,
            azp = claims.azp.as_deref().unwrap_or_default(),
            slug = %endpoint.slug,
            "a service-account username from a client it does not name"
        );
        return Err(Box::new(ProblemDetails::forbidden()));
    }
    let project = std::iter::once(&endpoint.project)
        .chain(endpoint.allowed_projects.iter())
        .find(|project| groups.contains(*project))
        .or_else(|| {
            // An `organization` audience admits a member of any project of the Endpoint's
            // Organization (ADR-N-058 2.4): the first group that is one.
            (endpoint.audience == Audience::Organization)
                .then(|| {
                    groups
                        .iter()
                        .find(|group| of_its_organization(gateway, endpoint, group))
                })
                .flatten()
        })
        .cloned()
        .unwrap_or_default();
    if !of_its_organization(gateway, endpoint, &project) {
        // Another Organization's Endpoint is answered as a slug nobody created (PF-32, R20).
        tracing::warn!(
            user = %user,
            azp = claims.azp.as_deref().unwrap_or_default(),
            slug = %endpoint.slug,
            "the token names no project of this endpoint's organization"
        );
        return Err(Box::new(ProblemDetails::not_found()));
    }
    if !endpoint.admits(Some(&project)) {
        // T-3033: an App's client minted tokens with no `groups` claim, and every project-bound
        // read was a 403 with no line anywhere. The person, the client, the endpoint; never the
        // token or its groups.
        tracing::warn!(
            user = %user,
            azp = claims.azp.as_deref().unwrap_or_default(),
            slug = %endpoint.slug,
            "the token names no project this endpoint admits"
        );
        return Err(Box::new(ProblemDetails::forbidden()));
    }

    Ok(Subject {
        user: Some(user),
        service_account: None,
        service_account_project: None,
        roles: roles_on(endpoint.audience, claims.roles().iter().cloned()),
        groups,
        did: None,
        agreement: None,
        via,
        client: claims.azp.clone(),
        organization: gateway.organization_of(&project),
    })
}

/// Whether `project` is of the Endpoint's own Organization, which an `organization` audience
/// asks of every caller (EP-14, ADR-N-058 2.4): one realm serves several Organizations, so a
/// valid token says nothing about which one its holder belongs to. Each project's Organization
/// comes from its Project manifest; while the gateway holds none for the Endpoint's project, the
/// installation is one Organization and every member passes, as before ADR-N-058.
fn of_its_organization(gateway: &Gateway, endpoint: &Endpoint, project: &str) -> bool {
    if endpoint.audience != Audience::Organization {
        return true;
    }
    match gateway.organization_of(&endpoint.project) {
        None => true,
        Some(theirs) => gateway.organization_of(project).as_deref() == Some(theirs.as_str()),
    }
}

/// Whether `user` may be decided under its own name: any person's username, and a
/// `service-account-<client>` one only on a token that client issued (`azp`).
fn owns_its_name(user: &str, azp: Option<&str>) -> bool {
    const MARK: &str = "service-account-";
    match (user.get(..MARK.len()), user.get(MARK.len()..)) {
        (Some(mark), Some(client)) if mark.eq_ignore_ascii_case(MARK) => {
            azp.is_some_and(|azp| !azp.is_empty() && client.eq_ignore_ascii_case(azp))
        }
        _ => true,
    }
}

/// The roles a caller holds on this endpoint: what the token asserts and, on a public
/// endpoint, the synthetic `public` role as well. A signed-in person is a member of the
/// public too, so a login never grants less than no login does (EP-16, GW22).
pub(crate) fn roles_on(
    audience: Audience,
    asserted: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let mut roles: BTreeSet<String> = asserted.into_iter().collect();
    if audience == Audience::Public {
        roles.insert("public".to_owned());
    }
    roles
}

/// The principal, as one string for the audit log.
pub(crate) fn principal_of(subject: &Subject) -> String {
    match (&subject.user, &subject.service_account, &subject.did) {
        (Some(user), _, _) => match &subject.via {
            Some(account) => format!("user:{user} via serviceAccount:{account}"),
            None => format!("user:{user}"),
        },
        (_, Some(account), _) => format!("serviceAccount:{account}"),
        // A data space consumer is its DID and nothing else, so the audit line says so
        // rather than calling a whole agreement anonymous (DS-13).
        (_, _, Some(did)) => did.clone(),
        _ => "anonymous".to_owned(),
    }
}

/// Resolving the slug and establishing the caller: the two steps every surface starts
/// with (EP-03, EP-14, GW20).
///
/// `representation` is the one the surface serves, and `None` for the surfaces that are
/// not a representation of the data at all, like `access`.
pub(crate) fn admit(
    gateway: &Gateway,
    slug: &str,
    representation: Option<Representation>,
    headers: &HeaderMap,
) -> Result<(Arc<Endpoint>, Subject), Box<Response<Body>>> {
    let endpoint = gateway
        .resolver
        .resolve(slug)
        .ok_or_else(|| Box::new(ProblemDetails::not_found().into_response()))?;

    // An endpoint that does not serve the representation is not an endpoint at this URL
    // (EP-05).
    if representation.is_some_and(|wanted| !endpoint.serves(wanted)) {
        return Err(Box::new(ProblemDetails::not_found().into_response()));
    }
    // ADR-N-054 2.6 (DM-122): a public Endpoint never serves a personal attribute, so one whose
    // model and projection would is refused whole until its owner hides them or narrows it.
    let personal = endpoint.public_personal_attributes();
    if !personal.is_empty() {
        tracing::warn!(slug, attributes = ?personal, "refused: a public endpoint exposes personal data");
        return Err(Box::new(
            ProblemDetails::new(
                403,
                "personal-data-public",
                "Personal Data on a Public Endpoint",
            )
            .with_detail(format!(
                "the model marks {} as personal data; hide them or narrow the audience",
                personal.join(", ")
            ))
            .into_response(),
        ));
    }
    // The NGSI-LD surface is where a federated read arrives from another broker (T-3999).
    let federated = representation == Some(Representation::NgsiLd);
    let subject = authenticate_on(gateway, &endpoint, headers, federated)
        .map_err(|problem| Box::new(problem.into_response()))?;
    Ok((endpoint, subject))
}

/// Resolving a space name and establishing the caller (SP-06, SP-11).
///
/// A space the caller may not reach and a space that does not exist answer the same 404,
/// so a probe over the space namespace learns nothing either way (R20).
pub(crate) fn admit_space(
    gateway: &Gateway,
    name: &str,
    representation: Option<Representation>,
    headers: &HeaderMap,
) -> Result<(Arc<Space>, Subject), Box<Response<Body>>> {
    // One answer for every way in which this caller may not have this space: it does not exist,
    // it does not serve this representation, the token does not verify, or no grant of the
    // caller's reaches it. Space names are guessable words — `helsinki`, `air-quality` — so a
    // 403 or a 401 where another space gives 404 is an enumeration oracle for what a deployment
    // runs (R20, SP-06, SP-11). A client that has to discover where to authenticate still can:
    // `/.well-known/oauth-protected-resource/…` answers for every name, resolvable or not.
    let missing = || Box::new(ProblemDetails::not_found().into_response());
    let space = gateway.resolver.resolve_space(name).ok_or_else(missing)?;
    if representation.is_some_and(|wanted| !space.endpoint.serves(wanted)) {
        return Err(missing());
    }
    let subject = authenticate(gateway, &space.endpoint, headers).map_err(|_| missing())?;
    if !discoverable(gateway, &space, &subject) {
        return Err(missing());
    }
    Ok((space, subject))
}

/// Whether this caller holds any grant that reaches this space (SP-11).
///
/// The same PDP that enforces a request decides who may see the space exists, so the
/// catalog cannot list a space the data surface would refuse.
pub(crate) fn discoverable(gateway: &Gateway, space: &Space, subject: &Subject) -> bool {
    !gateway
        .pdp
        .decide(
            subject,
            Operation::QueryEntity,
            &query::requested(&[]),
            &space.endpoint,
        )
        .is_deny()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AG-95: the audit line of a delegated call names the person and the account it came
    /// through; an ordinary person or workload is named alone.
    #[test]
    fn the_audit_principal_names_the_account_a_persons_call_came_through() {
        let person = Subject {
            user: Some("jana".to_owned()),
            ..Subject::default()
        };
        let delegated = Subject {
            via: Some("agent-proxy".to_owned()),
            ..person.clone()
        };
        let workload = Subject {
            service_account: Some("agent-proxy".to_owned()),
            ..Subject::default()
        };
        assert_eq!(principal_of(&person), "user:jana");
        assert_eq!(
            principal_of(&delegated),
            "user:jana via serviceAccount:agent-proxy"
        );
        assert_eq!(principal_of(&workload), "serviceAccount:agent-proxy");
    }

    /// T-5358: a `service-account-<client>` username names that client, so only its token
    /// carries it; a person's username carries no such tie.
    #[test]
    fn a_service_account_username_holds_only_on_its_own_clients_token() {
        let user = "service-account-portal-reconciler";
        assert!(owns_its_name(user, Some("portal-reconciler")));
        assert!(owns_its_name(
            "Service-Account-Portal-Reconciler",
            Some("portal-reconciler")
        ));
        assert!(!owns_its_name(user, Some("portal-ui")));
        assert!(!owns_its_name(user, Some("portal-reconciler-2")));
        assert!(!owns_its_name(user, None));
        assert!(!owns_its_name("service-account-", Some("")));
        assert!(owns_its_name("jana", Some("portal-ui")));
        assert!(owns_its_name("jana", None));
        assert!(owns_its_name("service.account@hel.fi", None));
    }

    #[test]
    fn a_signed_in_caller_on_a_public_endpoint_holds_the_public_role_too() {
        let asserted = || ["steward".to_owned()];
        assert_eq!(
            roles_on(Audience::Public, asserted()),
            BTreeSet::from(["public".to_owned(), "steward".to_owned()])
        );
        assert_eq!(
            roles_on(Audience::Organization, asserted()),
            BTreeSet::from(["steward".to_owned()])
        );
        assert_eq!(
            roles_on(Audience::ProjectList, Vec::new()),
            BTreeSet::new(),
            "no audience but public hands out a role the token did not assert"
        );
    }

    #[test]
    fn a_token_names_the_slug_the_public_resource_or_the_edge_audience() {
        assert_eq!(
            audiences_of(
                "k4y7pq2mzt6vhx3nbwrs5cjd8f",
                "/api/endpoint/k4y7pq2mzt6vhx3nbwrs5cjd8f",
                Some("https://hel.fi")
            ),
            vec![
                "k4y7pq2mzt6vhx3nbwrs5cjd8f".to_owned(),
                "https://hel.fi/api/endpoint/k4y7pq2mzt6vhx3nbwrs5cjd8f".to_owned(),
                EDGE_AUDIENCE.to_owned(),
            ]
        );
        assert_eq!(
            audiences_of(
                "k4y7pq2mzt6vhx3nbwrs5cjd8f",
                "/api/endpoint/k4y7pq2mzt6vhx3nbwrs5cjd8f",
                None
            ),
            vec![
                "k4y7pq2mzt6vhx3nbwrs5cjd8f".to_owned(),
                EDGE_AUDIENCE.to_owned()
            ],
            "no public URL, no resource URI, the edge audience stays"
        );
    }
}
