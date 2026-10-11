//! `jc:app/identity`, `jc:app/email` and `jc:app/ai` as the host links them (API/06 §5, T-3622).
//! Each is one call of the Portal's own route, on the App's own host and with the request's
//! person's token, so the layers, the quotas and the person's rights stay the Portal's (AP-164,
//! AP-165) and the component holds no address, key or token (AP-147). A job's run has no person:
//! it learns no caller and sends nothing.

use serde_json::{json, Value};

use crate::host::State;
use crate::storage::jc::app::{ai, email, identity, services};

pub use services::{Error as ServiceError, Refusal};

/// What one instance may ask the Portal, and as whom.
pub struct Services {
    /// The Portal's origin inside the cluster, and the client it is called with; `None` on a shard
    /// that names no Portal (`JC_WASM_PORTAL_URL`).
    pub portal: Option<(String, reqwest::Client)>,
    /// The request's person's token; `None` for an anonymous visitor and in a job.
    pub token: Option<String>,
    /// The `Host` the request came on, the App's own origin, which the Portal's service routes
    /// answer on alone (AP-133).
    pub origin: Option<String>,
    /// The largest answer read from the Portal.
    pub response_bytes: usize,
    /// The caller once asked, so a request asks the Portal once.
    caller: Option<Option<identity::Person>>,
}

impl Services {
    pub fn new(
        portal: Option<(String, reqwest::Client)>,
        token: Option<String>,
        origin: Option<String>,
        response_bytes: usize,
    ) -> Self {
        Self {
            portal,
            token,
            origin,
            response_bytes,
            caller: None,
        }
    }
}

const NO_PORTAL: &str = "this shard reaches no Portal (JC_WASM_PORTAL_URL)";
const NO_PERSON: &str = "a service acts in a person's name, and this request has none";

/// The Portal's answer: its status and its body read as JSON (`Null` when it is none), to the cap.
async fn call(
    services: &Services,
    request: reqwest::RequestBuilder,
) -> Result<(u16, Value), ServiceError> {
    let mut response = request.send().await.map_err(|err| {
        tracing::warn!(error = %err, "the Portal did not answer a service call");
        ServiceError::Unavailable("the Portal did not answer".into())
    })?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ServiceError::Unavailable("the Portal's answer broke off".into()))?
    {
        if bytes.len() + chunk.len() > services.response_bytes {
            return Err(ServiceError::Unavailable(
                "the Portal's answer is larger than the host takes".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

/// A `POST` of `body` to `/apps/{app}/api/services/{path}` on the App's own host, as the person.
async fn post(
    services: &Services,
    app: &str,
    path: &str,
    body: &Value,
) -> Result<(u16, Value), ServiceError> {
    let (portal, client) = services
        .portal
        .as_ref()
        .ok_or_else(|| ServiceError::Unavailable(NO_PORTAL.into()))?;
    let token = services
        .token
        .as_deref()
        .ok_or_else(|| ServiceError::Rejected(NO_PERSON.into()))?;
    let mut request = client
        .post(format!("{portal}/apps/{app}/api/services/{path}"))
        .bearer_auth(token)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(body.to_string());
    if let Some(origin) = &services.origin {
        request = request.header(http::header::HOST, origin);
    }
    call(services, request).await
}

/// A refusal the Portal answered, as the component reads it (API/06 §3): a `/service-off` or a
/// `/quota` problem is `refused` with its members, any other no is `rejected` with its detail.
pub fn refusal(status: u16, problem: &Value) -> ServiceError {
    let text = |key: &str| problem.get(key).and_then(Value::as_str).map(str::to_owned);
    let kind = text("type").unwrap_or_default();
    if kind.ends_with("/service-off") || kind.ends_with("/quota") {
        return ServiceError::Refused(Refusal {
            service: text("service").unwrap_or_default(),
            layer: text("layer"),
            quota: text("quota"),
            reset_at: text("resetAt"),
        });
    }
    let said = text("detail")
        .or_else(|| text("title"))
        .unwrap_or_else(|| "no reason given".into());
    if status >= 500 {
        ServiceError::Unavailable(format!("{status}: {said}"))
    } else {
        ServiceError::Rejected(format!("{status}: {said}"))
    }
}

impl identity::Host for State {
    async fn caller(&mut self) -> Option<identity::Person> {
        if let Some(known) = &self.services.caller {
            return known.clone();
        }
        let found = match (&self.services.portal, &self.services.token) {
            (Some((portal, client)), Some(token)) => {
                let request = client
                    .get(format!(
                        "{portal}/api/v1/projects/{}/apps/{}/me",
                        self.app.tenant, self.app.name
                    ))
                    .bearer_auth(token);
                match call(&self.services, request).await {
                    Ok((200, me)) => {
                        serde_json::from_value::<Me>(me)
                            .ok()
                            .map(|me| identity::Person {
                                id: me.id,
                                name: me.name,
                                email: me.email,
                                roles: me.roles,
                            })
                    }
                    Ok((status, _)) => {
                        // 401 is an anonymous visitor or a token for another App: nobody.
                        if status != 401 {
                            tracing::warn!(app = %self.app.id, status, "the Portal did not say who the caller is");
                        }
                        None
                    }
                    Err(_) => None,
                }
            }
            _ => None,
        };
        self.services.caller = Some(found.clone());
        found
    }
}

/// `GET …/apps/{name}/me` as the Portal answers it (AP-109).
#[derive(serde::Deserialize)]
struct Me {
    id: String,
    name: String,
    email: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
}

impl services::Host for State {}

impl email::Host for State {
    async fn send(&mut self, message: email::Message) -> Result<String, ServiceError> {
        let to = match message.to {
            email::Recipients::Me => json!("me"),
            email::Recipients::People(ids) => json!(ids),
        };
        let mut body = json!({ "to": to, "subject": message.subject, "text": message.text });
        if let Some(html) = message.html {
            body["html"] = json!(html);
        }
        match post(&self.services, &self.app.name, "email/send", &body).await? {
            (200..=299, sent) => Ok(sent
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()),
            (status, problem) => Err(refusal(status, &problem)),
        }
    }
}

impl ai::Host for State {
    async fn complete(&mut self, request: ai::Completion) -> Result<ai::Answer, ServiceError> {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|message| {
                let role = match message.role {
                    ai::Role::System => "system",
                    ai::Role::User => "user",
                    ai::Role::Assistant => "assistant",
                };
                json!({ "role": role, "content": message.content })
            })
            .collect();
        let mut body = json!({ "messages": messages });
        if let Some(max) = request.max_tokens {
            body["maxTokens"] = json!(max);
        }
        if let Some(schema) = &request.schema {
            body["schema"] = serde_json::from_str(schema)
                .map_err(|err| ServiceError::Rejected(format!("the schema is not JSON: {err}")))?;
        }
        match post(&self.services, &self.app.name, "ai/complete", &body).await? {
            (200, answer) => {
                let count = |key: &str| {
                    answer["tokens"][key]
                        .as_u64()
                        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
                };
                Ok(ai::Answer {
                    text: answer["text"].as_str().unwrap_or_default().to_owned(),
                    json: answer.get("json").map(Value::to_string),
                    tokens_in: count("in"),
                    tokens_out: count("out"),
                })
            }
            (status, problem) => Err(refusal(status, &problem)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_off_or_a_used_up_quota_is_refused_with_its_members() {
        let off = refusal(
            403,
            &json!({ "type": "https://joinedcontext.com/errors/service-off", "service": "ai", "layer": "organization" }),
        );
        assert_eq!(
            off,
            ServiceError::Refused(Refusal {
                service: "ai".into(),
                layer: Some("organization".into()),
                quota: None,
                reset_at: None,
            })
        );
        let spent = refusal(
            429,
            &json!({ "type": "https://joinedcontext.com/errors/quota", "service": "email", "quota": "emailsPerDay", "resetAt": "2026-10-12T00:00:00Z" }),
        );
        assert_eq!(
            spent,
            ServiceError::Refused(Refusal {
                service: "email".into(),
                layer: None,
                quota: Some("emailsPerDay".into()),
                reset_at: Some("2026-10-12T00:00:00Z".into()),
            })
        );
    }

    #[test]
    fn any_other_no_is_rejected_or_unavailable_with_what_the_portal_said() {
        assert_eq!(
            refusal(
                403,
                &json!({ "type": "https://joinedcontext.com/errors/recipient-refused", "title": "Forbidden", "detail": "not a person of the organization" })
            ),
            ServiceError::Rejected("403: not a person of the organization".into())
        );
        assert_eq!(
            refusal(503, &json!({ "title": "Service Unavailable" })),
            ServiceError::Unavailable("503: Service Unavailable".into())
        );
        assert_eq!(
            refusal(400, &Value::Null),
            ServiceError::Rejected("400: no reason given".into())
        );
    }
}
