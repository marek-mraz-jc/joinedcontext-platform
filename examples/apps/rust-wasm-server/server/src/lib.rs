//! Notes with a file each (T-3341): the starting point of a server WASM App. Every note is a row
//! of the App's own `notes` table (`migrations/`), every file an object under the App's own
//! prefix, uploaded and downloaded by the browser through presigned URLs, so no file passes
//! through the App.
//!
//! Roles and edits (T-3825, API/07): a note belongs to the person who wrote it; a `steward` reads
//! and changes every note and the shared notice, and reads the history. Who the caller is and
//! which roles they hold come from `identity::caller()` alone, never from the request.
//!
//! | Route | Who | What it does |
//! |---|---|---|
//! | `GET /api/notes` | signed in | the caller's notes (a steward: all), newest first |
//! | `POST /api/notes` `{"body"}` | signed in | a new note of the caller's |
//! | `PUT /api/notes/{id}` `{"body"}` | owner, steward | the note's text changed |
//! | `DELETE /api/notes/{id}` | owner, steward | the note and its file gone |
//! | `POST /api/notes/{id}/file` `{"name", "contentType"}` | owner, steward | a URL to upload the note's file to |
//! | `GET /api/notes/{id}/file` | owner, steward | a URL to download it from |
//! | `GET /api/notice` | anyone | the shared notice |
//! | `PUT /api/notice` `{"body"}` | steward | the notice changed |
//! | `GET /api/history` | steward | the newest changes, who made them and when |
//! | `PATCH /api/entities/{id}` `{attrs}` | the Endpoint's Policy decides | Attributes of one Entity changed |

use jc_app_sdk::access::{self, Owned};
use jc_app_sdk::blob::{self, Method};
use jc_app_sdk::gateway;
use jc_app_sdk::http::{Params, Request, Response, Router};
use jc_app_sdk::identity::Caller;
use jc_app_sdk::sql::{self, Value};
use serde::Deserialize;

/// How long a presigned URL lives; the host allows at most 300 seconds.
const URL_SECONDS: u32 = 120;

/// The App role that reads and changes every note and the notice.
const STEWARD: &str = "steward";
const NOTES: Owned = Owned::new("notes", STEWARD);
/// One row, id 1, owned by nobody, so the steward role alone changes it.
const NOTICE: Owned = Owned::new("notice", STEWARD);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Text {
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Upload {
    name: String,
    content_type: Option<String>,
    /// The file's length in bytes; the URL takes a body of exactly this length.
    size: u64,
}

/// A note's text: 1 to 2000 characters once trimmed.
pub fn text(body: &str) -> Result<String, String> {
    let body = body.trim();
    match body.chars().count() {
        0 => Err("a note has some text".into()),
        n if n > 2000 => Err("a note has at most 2000 characters".into()),
        _ => Ok(body.to_owned()),
    }
}

/// The key of a note's file: `notes/{id}/{name}`, the name reduced to letters, digits, `.`, `-`
/// and `_`, so a name can never name another key.
pub fn file_key(id: i64, name: &str) -> Result<String, String> {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_matches('.');
    if clean.is_empty() || clean.len() > 120 {
        return Err("a file name has 1 to 120 letters, digits, dots, dashes or underscores".into());
    }
    Ok(format!("notes/{id}/{clean}"))
}

fn id(params: &Params) -> Result<i64, Response> {
    params["id"]
        .parse()
        .map_err(|_| Response::problem(404, "Not Found", "no such note"))
}

fn body(request: &Request) -> Result<String, Response> {
    let note: Text = request.json()?;
    text(&note.body).map_err(|why| Response::problem(400, "Bad Request", &why))
}

fn list(_: &Request, _: &Params) -> Response {
    let caller = match Caller::require() {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let all = caller.has_role(STEWARD);
    match sql::query(
        "select id, owner_id, body, file, created_at from notes where owner_id = $1 or $2::boolean order by id desc limit 200",
        &[Value::from(caller.id.as_str()), Value::Boolean(all)],
    ) {
        Ok(rows) => Response::json(200, &sql::objects(&rows)),
        Err(err) => Response::from_sql(err),
    }
}

fn create(request: &Request, _: &Params) -> Response {
    let caller = match Caller::require() {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let body = match body(request) {
        Ok(body) => body,
        Err(answer) => return answer,
    };
    match NOTES.insert(&caller, &["body"], vec![Value::from(body)], "added a note") {
        Ok(rows) => Response::json(201, &sql::objects(&rows).into_iter().next()),
        Err(answer) => answer,
    }
}

fn change(request: &Request, params: &Params) -> Response {
    let (caller, id) = match Caller::require().and_then(|c| id(params).map(|id| (c, id))) {
        Ok(found) => found,
        Err(answer) => return answer,
    };
    let body = match body(request) {
        Ok(body) => body,
        Err(answer) => return answer,
    };
    match NOTES.update(
        &caller,
        id,
        &["body"],
        vec![Value::from(body)],
        "changed a note",
    ) {
        Ok(()) => Response::no_content(),
        Err(answer) => answer,
    }
}

/// The note's file key, `None` when it has none, or the answer when the note does not exist or is
/// not the caller's to touch.
fn file_of(caller: &Caller, id: i64) -> Result<Option<String>, Response> {
    let rows = sql::query(
        "select file from notes where id = $1 and (owner_id = $2 or $3::boolean)",
        &[
            Value::from(id),
            Value::from(caller.id.as_str()),
            Value::Boolean(caller.has_role(STEWARD)),
        ],
    )
    .map_err(Response::from_sql)?;
    match rows.values.first().and_then(|row| row.first()) {
        None => Err(Response::problem(404, "Not Found", "no such note")),
        Some(Value::Text(key)) => Ok(Some(key.clone())),
        Some(_) => Ok(None),
    }
}

fn remove(_: &Request, params: &Params) -> Response {
    let (caller, id) = match Caller::require().and_then(|c| id(params).map(|id| (c, id))) {
        Ok(found) => found,
        Err(answer) => return answer,
    };
    let file = match file_of(&caller, id) {
        Ok(file) => file,
        Err(answer) => return answer,
    };
    if let Some(key) = file {
        match blob::delete(&key) {
            Ok(()) | Err(blob::Error::NotFound) => {}
            Err(err) => return Response::from_blob(err),
        }
    }
    match NOTES.delete(&caller, id, "removed a note") {
        Ok(()) => Response::no_content(),
        Err(answer) => answer,
    }
}

fn upload(request: &Request, params: &Params) -> Response {
    let (caller, id) = match Caller::require().and_then(|c| id(params).map(|id| (c, id))) {
        Ok(found) => found,
        Err(answer) => return answer,
    };
    let wanted: Upload = match request.json() {
        Ok(wanted) => wanted,
        Err(answer) => return answer,
    };
    let key = match file_key(id, &wanted.name) {
        Ok(key) => key,
        Err(why) => return Response::problem(400, "Bad Request", &why),
    };
    if let Err(answer) = NOTES.update(
        &caller,
        id,
        &["file"],
        vec![Value::from(key.clone())],
        "attached a file",
    ) {
        return answer;
    }
    match blob::presign_put(&key, wanted.size, URL_SECONDS) {
        Ok(url) => Response::json(
            200,
            &serde_json::json!({"url": url, "method": "PUT", "contentType": wanted.content_type}),
        ),
        Err(err) => Response::from_blob(err),
    }
}

fn download(_: &Request, params: &Params) -> Response {
    let (caller, id) = match Caller::require().and_then(|c| id(params).map(|id| (c, id))) {
        Ok(found) => found,
        Err(answer) => return answer,
    };
    match file_of(&caller, id) {
        Err(answer) => answer,
        Ok(None) => Response::problem(404, "Not Found", "the note has no file"),
        Ok(Some(key)) => match blob::presign(&key, Method::Get, URL_SECONDS) {
            Ok(url) => Response::json(200, &serde_json::json!({"url": url})),
            Err(err) => Response::from_blob(err),
        },
    }
}

fn notice(_: &Request, _: &Params) -> Response {
    match sql::query("select body from notice where id = 1", &[]) {
        Ok(rows) => Response::json(200, &sql::objects(&rows).into_iter().next()),
        Err(err) => Response::from_sql(err),
    }
}

/// The role is checked before the body is read, so nothing in the body can stand in for it.
fn change_notice(request: &Request, _: &Params) -> Response {
    let caller = match Caller::require().and_then(|c| c.require_role(&[STEWARD]).map(|()| c)) {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let body = match body(request) {
        Ok(body) => body,
        Err(answer) => return answer,
    };
    match NOTICE.update(
        &caller,
        1,
        &["body"],
        vec![Value::from(body)],
        "changed the notice",
    ) {
        Ok(()) => Response::no_content(),
        Err(answer) => answer,
    }
}

fn history(request: &Request, _: &Params) -> Response {
    if let Err(answer) = Caller::require().and_then(|c| c.require_role(&[STEWARD])) {
        return answer;
    }
    let limit = request
        .param("limit")
        .and_then(|n| n.parse().ok())
        .unwrap_or(50);
    match access::history(limit) {
        Ok(rows) => Response::json(200, &sql::objects(&rows)),
        Err(answer) => answer,
    }
}

/// Context data is changed as the caller through the App's own Endpoint: its Policy decides who
/// may, so the App asks only that the caller signed in, and a refusal passes through as `403`.
fn edit_entity(request: &Request, params: &Params) -> Response {
    let caller = match Caller::require() {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let attrs: serde_json::Value = match request.json() {
        Ok(attrs) => attrs,
        Err(answer) => return answer,
    };
    let Some(names) = attrs
        .as_object()
        .map(|a| a.keys().cloned().collect::<Vec<_>>().join(", "))
    else {
        return Response::problem(400, "Bad Request", "the Attributes are a JSON object");
    };
    let id = &params["id"];
    if let Err(err) = gateway::patch_entity(id, &attrs) {
        return Response::from(err);
    }
    match access::record(&caller, "edit", id, &format!("changed {names}")) {
        Ok(()) => Response::no_content(),
        Err(answer) => answer,
    }
}

pub fn handle(request: Request) -> Response {
    Router::new()
        .get("/api/notes", list)
        .post("/api/notes", create)
        .put("/api/notes/{id}", change)
        .delete("/api/notes/{id}", remove)
        .post("/api/notes/{id}/file", upload)
        .get("/api/notes/{id}/file", download)
        .get("/api/notice", notice)
        .put("/api/notice", change_notice)
        .get("/api/history", history)
        .route("PATCH", "/api/entities/{id}", edit_entity)
        .handle(&request)
}

jc_app_sdk::app!(handle);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_has_some_text_and_not_too_much() {
        assert_eq!(text("  hello "), Ok("hello".into()));
        assert!(text("   ").is_err());
        assert!(text(&"x".repeat(2001)).is_err());
        assert_eq!(text(&"é".repeat(2000)).map(|t| t.chars().count()), Ok(2000));
    }

    #[test]
    fn a_file_name_never_names_another_key() {
        assert_eq!(
            file_key(7, "report 2026.pdf"),
            Ok("notes/7/report_2026.pdf".into())
        );
        assert_eq!(file_key(7, "../../8/x"), Ok("notes/7/_.._8_x".into()));
        assert!(file_key(7, "../../8/x").unwrap().starts_with("notes/7/"));
        assert!(!file_key(7, "../../8/x").unwrap()["notes/7/".len()..].contains('/'));
        assert!(file_key(7, "...").is_err());
        assert!(file_key(7, "").is_err());
        assert!(file_key(7, &"a".repeat(121)).is_err());
    }
}
