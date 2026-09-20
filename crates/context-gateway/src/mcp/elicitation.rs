//! The question a destructive data-plane tool asks before it runs (AG-08, T-0849).
//!
//! An agent may read anything its grants cover, but it may not create something that outlives
//! the conversation on its own word. `create_subscription` answers the first call with an
//! elicitation and creates nothing; the host shows it to the person, and the client repeats the
//! same call carrying the answer.
//!
//! The façade is one stateless POST, so the elicitation cannot be a server→client request on an
//! open stream: it is the answer to the first call and an argument of the second, the shape the
//! Portal's configuration MCP already uses (Architecture/07 section 3, ADR-N-021). What makes
//! the second call the person's rather than the model's is that the **server** minted the id: a
//! boolean the model writes into its own call proves nothing, because the model writes both
//! calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

/// How long a person has to answer before the id is no longer accepted.
pub const EXPIRES_IN_SECONDS: i64 = 600;

/// One question waiting for its answer.
struct Pending {
    /// Who asked: the caller the token names. Nobody else may answer it.
    owner: String,
    /// The endpoint or space the question was asked on: one space's answer is not another's.
    surface: String,
    tool: String,
    /// The arguments the question was asked about: a different call needs a new answer.
    digest: String,
    asked_at: i64,
}

/// What the caller sent back with the second call.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// The person allowed it: the tool runs.
    Accepted,
    /// The person refused, or the host could not ask: nothing runs.
    Declined,
    /// No such question for this caller, or it was answered, expired, or asked about something
    /// else. Nothing runs, and the caller asks again.
    Unknown,
}

/// Every question this replica is waiting on.
///
/// ponytail: per-replica map. A question lives ten minutes, so a restart costs one repeated
/// ask and never a lost decision; a gateway behind more than one replica needs the question and
/// its answer to reach the same one, or a shared store for them.
#[derive(Clone, Default)]
pub struct Elicitations {
    pending: Arc<Mutex<HashMap<String, Pending>>>,
}

impl std::fmt::Debug for Elicitations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Elicitations")
    }
}

impl Elicitations {
    /// A store with no question open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a question and returns its id.
    pub fn ask(&self, owner: &str, surface: &str, tool: &str, digest: &str) -> String {
        self.forget_expired();
        let id = format!(
            "eli-{}",
            short_digest(&format!("{owner}{surface}{tool}{}", now_nanos()))
        );
        if let Ok(mut map) = self.pending.lock() {
            map.insert(
                id.clone(),
                Pending {
                    owner: owner.to_owned(),
                    surface: surface.to_owned(),
                    tool: tool.to_owned(),
                    digest: digest.to_owned(),
                    asked_at: now_unix(),
                },
            );
        }
        id
    }

    /// Reads the answer the client sent, and spends the question: an id answers once.
    pub fn answer(
        &self,
        owner: &str,
        surface: &str,
        tool: &str,
        digest: &str,
        sent: &Value,
    ) -> Answer {
        let Some(id) = sent
            .get("elicitationId")
            .or_else(|| sent.get("id"))
            .and_then(Value::as_str)
        else {
            return Answer::Unknown;
        };
        let Ok(mut map) = self.pending.lock() else {
            return Answer::Unknown;
        };
        let Some(pending) = map.get(id) else {
            return Answer::Unknown;
        };
        // The question, the caller, the surface and the arguments all have to be the ones it
        // was asked about.
        if pending.owner != owner
            || pending.surface != surface
            || pending.tool != tool
            || pending.digest != digest
            || now_unix() - pending.asked_at > EXPIRES_IN_SECONDS
        {
            return Answer::Unknown;
        }
        map.remove(id);
        match sent
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("accept")
        {
            "accept" | "accepted" => Answer::Accepted,
            _ => Answer::Declined,
        }
    }

    fn forget_expired(&self) {
        let now = now_unix();
        if let Ok(mut map) = self.pending.lock() {
            map.retain(|_, pending| now - pending.asked_at <= EXPIRES_IN_SECONDS);
        }
    }
}

/// The question as the client receives it: what would happen, and the form the person fills.
pub fn document(id: &str, message: &str, schema: Value) -> Value {
    json!({
        "isError": false,
        "status": "input_required",
        "content": [{ "type": "text", "text": message }],
        "structuredContent": { "elicitation": {
            "elicitationId": id,
            "mode": "form",
            "schema": schema,
            "message": message,
            "expiresIn": EXPIRES_IN_SECONDS,
        }},
    })
}

/// A stable short digest of the arguments a question was asked about.
///
/// The answer is an argument of the second call, so it is not part of what the question was
/// about: a client that repeats the call with `params.elicitation` has not changed the call.
pub fn digest_of(arguments: &Map<String, Value>) -> String {
    short_digest(
        &serde_json::to_string(&canonical(&Value::Object(arguments.clone()))).unwrap_or_default(),
    )
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> =
                map.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn short_digest(text: &str) -> String {
    crate::app::sha256_hex(text.as_bytes())[..16].to_owned()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(kind: &str) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("subscription".to_owned(), json!({ "type": kind }));
        map
    }

    #[test]
    fn an_id_the_server_never_minted_is_unknown() {
        let store = Elicitations::new();
        let digest = digest_of(&args("Device"));
        assert_eq!(
            store.answer(
                "sub-1",
                "air",
                "create_subscription",
                &digest,
                &json!({ "elicitationId": "eli-0000000000000000", "action": "accept" })
            ),
            Answer::Unknown
        );
    }

    #[test]
    fn an_id_answers_once_and_only_for_who_asked_about_what() {
        let store = Elicitations::new();
        let digest = digest_of(&args("Device"));
        let id = store.ask("sub-1", "air", "create_subscription", &digest);
        let accept = json!({ "elicitationId": id, "action": "accept" });

        // Another caller, another surface, another tool and other arguments are each a
        // different question.
        assert_eq!(
            store.answer("sub-2", "air", "create_subscription", &digest, &accept),
            Answer::Unknown
        );
        assert_eq!(
            store.answer("sub-1", "traffic", "create_subscription", &digest, &accept),
            Answer::Unknown
        );
        assert_eq!(
            store.answer("sub-1", "air", "upsert_entity", &digest, &accept),
            Answer::Unknown
        );
        assert_eq!(
            store.answer(
                "sub-1",
                "air",
                "create_subscription",
                &digest_of(&args("Vehicle")),
                &accept
            ),
            Answer::Unknown
        );

        assert_eq!(
            store.answer("sub-1", "air", "create_subscription", &digest, &accept),
            Answer::Accepted
        );
        // Spent.
        assert_eq!(
            store.answer("sub-1", "air", "create_subscription", &digest, &accept),
            Answer::Unknown
        );
    }

    #[test]
    fn a_decline_is_a_decline_and_an_answer_with_no_id_is_unknown() {
        let store = Elicitations::new();
        let digest = digest_of(&args("Device"));
        let id = store.ask("sub-1", "air", "create_subscription", &digest);
        assert_eq!(
            store.answer(
                "sub-1",
                "air",
                "create_subscription",
                &digest,
                &json!({ "action": "accept" })
            ),
            Answer::Unknown
        );
        assert_eq!(
            store.answer(
                "sub-1",
                "air",
                "create_subscription",
                &digest,
                &json!({ "elicitationId": id, "action": "decline" })
            ),
            Answer::Declined
        );
    }

    /// The arguments are what the question was about, whatever order they arrive in.
    #[test]
    fn the_digest_does_not_depend_on_key_order() {
        let mut one = Map::new();
        one.insert("a".to_owned(), json!(1));
        one.insert("b".to_owned(), json!({ "y": 2, "x": 1 }));
        let mut other = Map::new();
        other.insert("b".to_owned(), json!({ "x": 1, "y": 2 }));
        other.insert("a".to_owned(), json!(1));
        assert_eq!(digest_of(&one), digest_of(&other));
    }
}
