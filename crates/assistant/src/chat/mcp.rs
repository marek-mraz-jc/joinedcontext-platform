//! A connector's tools: the MCP surface of one Endpoint on the Context Gateway,
//! `POST /api/endpoint/{slug}/mcp`, one JSON-RPC message per request (EP-37, AG-106). On the
//! anonymous channels it is called without a token, so the gateway answers as it answers anyone;
//! an `internal` deployment's non-public Endpoint is called with the asking person's own token
//! (API/05 §1.7, AG-115).
//!
//! A deployment's `mcpServer` is read once per question through the named server,
//! `POST /api/mcp/{project}/{name}` (EP-92): its `list_endpoints` names the Endpoints this
//! asker reads through it, and each becomes a connector of its own Endpoint, as above (MF-56).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The most of one tool result the model reads; the rest is cut and the cut is said.
pub const MAX_RESULT_CHARS: usize = 20_000;

/// The most of one tool result kept for a script to read (AG-112): `jc-functions` takes a
/// request body of 256 KiB.
pub const MAX_KEPT_CHARS: usize = 250_000;

/// The tools jc-assistant ever calls on a connector, whatever a manifest lists: reads only
/// (T-4140, Architecture/23 §5). Any other name is dropped before a request leaves.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "list_types",
    "get_type",
    "list_attributes",
    "query_entities",
    "summarize",
    "get_entity",
    "get_attribute_value",
    "query_temporal",
    "retrieve_temporal",
    "describe_schema",
    "describe_access",
];

/// One tool of a connector, as the model is offered it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A person's access token, for the one question it came with: never printed, never stored.
#[derive(Clone)]
pub struct PersonToken(pub String);

impl std::fmt::Debug for PersonToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PersonToken(..)")
    }
}

/// The most Endpoints a named server lends one question, the fan-out bound of a named server
/// (EP-121): a server that reaches more lends the first of its `list_endpoints` page.
// ponytail: the first ten in the server's order; a catalogue search by the question (T-4501)
// when a server's reach is larger than that.
pub const MAX_SERVER_ENDPOINTS: usize = 10;

/// One JSON-RPC message to the gateway at `url`, as `token` or as anyone; the `result`.
async fn post(
    http: &reqwest::Client,
    url: String,
    timeout: Duration,
    token: Option<&PersonToken>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let mut request = http
        .post(url)
        .timeout(timeout)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
    if let Some(token) = token {
        request = request.bearer_auth(&token.0);
    }
    let answer = request
        .send()
        .await
        .map_err(|err| err.without_url().to_string())?;
    let status = answer.status();
    if !status.is_success() {
        return Err(format!("the Endpoint answered {status}"));
    }
    let body: Value = answer
        .json()
        .await
        .map_err(|err| format!("the Endpoint's answer is not JSON: {}", err.without_url()))?;
    if let Some(error) = body.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("an error");
        return Err(format!("the Endpoint refused: {message}"));
    }
    body.get("result")
        .cloned()
        .ok_or_else(|| "the Endpoint's answer has no result".to_owned())
}

/// One Endpoint a named server lends: its slug and whether it is public.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lent {
    pub slug: String,
    pub public: bool,
}

/// How long a public Endpoint's tool listing is kept: a changed Endpoint is offered with its
/// new tools within this time.
pub const LISTING_LIFE: Duration = Duration::from_secs(300);

/// The most public listings kept; past it the oldest goes.
const MAX_LISTINGS: usize = 512;

/// Public Endpoints' tool listings, kept per gateway and slug for [`LISTING_LIFE`], at most
/// [`MAX_LISTINGS`] (T-4680).
// ponytail: in memory, one replica; each replica lists for itself, as the answer cache does.
#[derive(Default)]
pub struct Listings(Mutex<Vec<(String, Instant, Vec<Value>)>>);

impl Listings {
    fn kept(&self, key: &str, now: Instant) -> Option<Vec<Value>> {
        let listings = self.0.lock().ok()?;
        listings
            .iter()
            .find(|(k, at, _)| k == key && now.saturating_duration_since(*at) < LISTING_LIFE)
            .map(|(_, _, listed)| listed.clone())
    }

    fn keep(&self, key: String, listed: Vec<Value>, now: Instant) {
        let Ok(mut listings) = self.0.lock() else {
            return;
        };
        listings
            .retain(|(k, at, _)| *k != key && now.saturating_duration_since(*at) < LISTING_LIFE);
        if listings.len() >= MAX_LISTINGS {
            listings.remove(0);
        }
        listings.push((key, now, listed));
    }
}

/// A named MCP server on the gateway, `{project}/{name}` (EP-92).
#[derive(Debug, Clone)]
pub struct Server {
    pub gateway: String,
    pub server: String,
    pub timeout: Duration,
    /// The person the server lists for; `None` lists as anyone.
    pub token: Option<PersonToken>,
}

impl Server {
    /// The Endpoints the server lends this asker, at most [`MAX_SERVER_ENDPOINTS`], in its order.
    pub async fn endpoints(&self, http: &reqwest::Client) -> Result<Vec<Lent>, String> {
        let result = post(
            http,
            format!("{}/api/mcp/{}", self.gateway, self.server),
            self.timeout,
            self.token.as_ref(),
            "tools/call",
            json!({"name": "list_endpoints", "arguments": {"limit": MAX_SERVER_ENDPOINTS}}),
        )
        .await?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err("the server refused to list its Endpoints".to_owned());
        }
        Ok(lent(&result))
    }
}

/// The rows of a `list_endpoints` result, each a slug the gateway could serve; a row without
/// one is dropped.
fn lent(result: &Value) -> Vec<Lent> {
    result
        .pointer("/structuredContent/endpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let slug = row.get("slug").and_then(Value::as_str)?;
            let valid = !slug.is_empty()
                && slug.len() <= 64
                && slug
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
            valid.then(|| Lent {
                slug: slug.to_owned(),
                public: row.get("access").and_then(Value::as_str) == Some("public"),
            })
        })
        .take(MAX_SERVER_ENDPOINTS)
        .collect()
}

/// The MCP surface of one Endpoint.
#[derive(Debug, Clone)]
pub struct Surface {
    /// The gateway, scheme and authority.
    pub gateway: String,
    pub slug: String,
    pub timeout: Duration,
    /// The person the call reads as; `None` calls as anyone.
    pub token: Option<PersonToken>,
}

impl Surface {
    async fn rpc(
        &self,
        http: &reqwest::Client,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        post(
            http,
            format!("{}/api/endpoint/{}/mcp", self.gateway, self.slug),
            self.timeout,
            self.token.as_ref(),
            method,
            params,
        )
        .await
    }

    /// The surface's `tools/list`. A public listing is the same for anyone, so it is kept for
    /// [`LISTING_LIFE`] per gateway and slug (T-4680); one read with a person's token is never
    /// kept, because their grants shape it. A failure is never kept.
    async fn listed(
        &self,
        http: &reqwest::Client,
        listings: &Listings,
    ) -> Result<Vec<Value>, String> {
        let key = (self.token.is_none()).then(|| format!("{}/{}", self.gateway, self.slug));
        if let Some(listed) = key
            .as_deref()
            .and_then(|key| listings.kept(key, Instant::now()))
        {
            return Ok(listed);
        }
        let result = self.rpc(http, "tools/list", json!({})).await?;
        let listed = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(key) = key {
            listings.keep(key, listed.clone(), Instant::now());
        }
        Ok(listed)
    }

    /// The tools the surface offers that `allowed` names and [`READ_ONLY_TOOLS`] holds, in the
    /// order `allowed` names them.
    pub async fn tools(
        &self,
        http: &reqwest::Client,
        listings: &Listings,
        allowed: &[String],
    ) -> Result<Vec<Tool>, String> {
        let allowed: Vec<&String> = allowed
            .iter()
            .filter(|name| READ_ONLY_TOOLS.contains(&name.as_str()))
            .collect();
        if allowed.is_empty() {
            return Ok(Vec::new());
        }
        let listed = self.listed(http, listings).await?;
        Ok(allowed
            .iter()
            .filter_map(|name| {
                let tool = listed
                    .iter()
                    .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()))?;
                Some(Tool {
                    name: (*name).clone(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                })
            })
            .collect())
    }

    /// Calls `name` with `arguments` and returns its text content, cut at [`MAX_KEPT_CHARS`].
    pub async fn call(
        &self,
        http: &reqwest::Client,
        name: &str,
        arguments: Value,
    ) -> Result<String, String> {
        let result = self
            .rpc(
                http,
                "tools/call",
                json!({"name": name, "arguments": arguments}),
            )
            .await?;
        let text: Vec<&str> = result
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let text = text.join("\n");
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(format!("the tool failed: {}", cut(&text, 500)));
        }
        Ok(cut(&text, MAX_KEPT_CHARS))
    }
}

/// `text` cut at `max` characters, saying so.
pub fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((at, _)) => format!(
            "{}\n[cut: the result went on past {max} characters]",
            &text[..at]
        ),
    }
}

#[cfg(test)]
mod tests {

    /// T-4680: a kept listing serves until its life ends, a new one replaces the old for its key,
    /// and the store never holds more than [`MAX_LISTINGS`].
    #[test]
    fn a_public_listing_is_kept_for_its_life_and_the_store_is_bounded() {
        let listings = Listings::default();
        let at = Instant::now();
        let tool = |name: &str| vec![json!({"name": name})];
        listings.keep("kept-test/a".into(), tool("list_types"), at);
        assert_eq!(
            listings.kept("kept-test/a", at + Duration::from_secs(299)),
            Some(tool("list_types"))
        );
        assert_eq!(listings.kept("kept-test/a", at + LISTING_LIFE), None);
        assert_eq!(listings.kept("kept-test/b", at), None);
        listings.keep("kept-test/a".into(), tool("get_entity"), at);
        assert_eq!(listings.kept("kept-test/a", at), Some(tool("get_entity")));
        for n in 0..MAX_LISTINGS + 5 {
            listings.keep(format!("kept-test/many/{n}"), Vec::new(), at);
        }
        assert!(listings.0.lock().map(|l| l.len()).unwrap_or(usize::MAX) <= MAX_LISTINGS);
        assert_eq!(
            listings.kept(&format!("kept-test/many/{}", MAX_LISTINGS + 4), at),
            Some(Vec::new())
        );
    }
    use super::{cut, lent, Lent, Listings, LISTING_LIFE, MAX_LISTINGS, MAX_SERVER_ENDPOINTS};
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn a_server_lends_its_listed_slugs_at_most_ten() {
        let rows: Vec<_> = (0..12)
            .map(|i| json!({"slug": format!("s{i:02}"), "access": if i == 0 { "public" } else { "internal" }}))
            .chain([json!({"slug": "../x"}), json!({"title": "no slug"})])
            .collect();
        let got = lent(&json!({"structuredContent": {"endpoints": rows}}));
        assert_eq!(got.len(), MAX_SERVER_ENDPOINTS);
        assert_eq!(
            got[0],
            Lent {
                slug: "s00".into(),
                public: true
            }
        );
        assert!(!got[1].public);
        let odd =
            lent(&json!({"structuredContent": {"endpoints": [{"slug": "../x"}, {"slug": "A1"}]}}));
        assert!(odd.is_empty());
        assert!(lent(&json!({})).is_empty());
    }

    #[test]
    fn a_long_result_is_cut_on_a_character_and_says_so() {
        assert_eq!(cut("ľščťž", 10), "ľščťž");
        let cut_text = cut("ľščťž", 2);
        assert!(cut_text.starts_with("ľš\n[cut:"));
        assert!(cut_text.contains("2 characters"));
    }
}
