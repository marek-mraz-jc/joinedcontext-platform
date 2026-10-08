//! Notes with a file each (T-3341): the starting point of a server WASM App. Every note is a row
//! of the App's own `notes` table (`migrations/0001_notes.sql`), every file an object under the
//! App's own prefix, uploaded and downloaded by the browser through presigned URLs, so no file
//! passes through the App.
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /api/notes` | the notes, newest first |
//! | `POST /api/notes` `{"body"}` | a new note |
//! | `PUT /api/notes/{id}` `{"body"}` | the note's text changed |
//! | `DELETE /api/notes/{id}` | the note and its file gone |
//! | `POST /api/notes/{id}/file` `{"name", "contentType"}` | a URL to upload the note's file to |
//! | `GET /api/notes/{id}/file` | a URL to download it from |

use jc_app_sdk::blob::{self, Method};
use jc_app_sdk::http::{Params, Request, Response, Router};
use jc_app_sdk::sql::{self, Value};
use serde::Deserialize;

/// How long a presigned URL lives; the host allows at most 300 seconds.
const URL_SECONDS: u32 = 120;

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
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let clean = clean.trim_matches('.');
    if clean.is_empty() || clean.len() > 120 {
        return Err("a file name has 1 to 120 letters, digits, dots, dashes or underscores".into());
    }
    Ok(format!("notes/{id}/{clean}"))
}

fn id(params: &Params) -> Result<i64, Response> {
    params["id"].parse().map_err(|_| Response::problem(404, "Not Found", "no such note"))
}

fn list(_: &Request, _: &Params) -> Response {
    match sql::query("select id, body, file, created_at from notes order by id desc limit 200", &[]) {
        Ok(rows) => Response::json(200, &sql::objects(&rows)),
        Err(err) => Response::from_sql(err),
    }
}

fn create(request: &Request, _: &Params) -> Response {
    let note: Text = match request.json() {
        Ok(note) => note,
        Err(answer) => return answer,
    };
    let body = match text(&note.body) {
        Ok(body) => body,
        Err(why) => return Response::problem(400, "Bad Request", &why),
    };
    match sql::query("insert into notes (body) values ($1) returning id, body, file, created_at", &[Value::from(body)]) {
        Ok(rows) => Response::json(201, &sql::objects(&rows).into_iter().next()),
        Err(err) => Response::from_sql(err),
    }
}

fn change(request: &Request, params: &Params) -> Response {
    let id = match id(params) {
        Ok(id) => id,
        Err(answer) => return answer,
    };
    let body = match request.json::<Text>().map(|n| text(&n.body)) {
        Ok(Ok(body)) => body,
        Ok(Err(why)) => return Response::problem(400, "Bad Request", &why),
        Err(answer) => return answer,
    };
    match sql::execute("update notes set body = $1 where id = $2", &[Value::from(body), Value::from(id)]) {
        Ok(0) => Response::problem(404, "Not Found", "no such note"),
        Ok(_) => Response::no_content(),
        Err(err) => Response::from_sql(err),
    }
}

/// The note's file key, `None` when it has none, or the answer when the note does not exist.
fn file_of(id: i64) -> Result<Option<String>, Response> {
    let rows = sql::query("select file from notes where id = $1", &[Value::from(id)]).map_err(Response::from_sql)?;
    match rows.values.first().and_then(|row| row.first()) {
        None => Err(Response::problem(404, "Not Found", "no such note")),
        Some(Value::Text(key)) => Ok(Some(key.clone())),
        Some(_) => Ok(None),
    }
}

fn remove(_: &Request, params: &Params) -> Response {
    let id = match id(params) {
        Ok(id) => id,
        Err(answer) => return answer,
    };
    let file = match file_of(id) {
        Ok(file) => file,
        Err(answer) => return answer,
    };
    if let Some(key) = file {
        match blob::delete(&key) {
            Ok(()) | Err(blob::Error::NotFound) => {}
            Err(err) => return Response::from_blob(err),
        }
    }
    match sql::execute("delete from notes where id = $1", &[Value::from(id)]) {
        Ok(_) => Response::no_content(),
        Err(err) => Response::from_sql(err),
    }
}

fn upload(request: &Request, params: &Params) -> Response {
    let id = match id(params) {
        Ok(id) => id,
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
    match sql::execute("update notes set file = $1 where id = $2", &[Value::from(key.clone()), Value::from(id)]) {
        Ok(0) => return Response::problem(404, "Not Found", "no such note"),
        Ok(_) => {}
        Err(err) => return Response::from_sql(err),
    }
    match blob::presign(&key, Method::Put, URL_SECONDS) {
        Ok(url) => Response::json(200, &serde_json::json!({"url": url, "method": "PUT", "contentType": wanted.content_type})),
        Err(err) => Response::from_blob(err),
    }
}

fn download(_: &Request, params: &Params) -> Response {
    let id = match id(params) {
        Ok(id) => id,
        Err(answer) => return answer,
    };
    match file_of(id) {
        Err(answer) => answer,
        Ok(None) => Response::problem(404, "Not Found", "the note has no file"),
        Ok(Some(key)) => match blob::presign(&key, Method::Get, URL_SECONDS) {
            Ok(url) => Response::json(200, &serde_json::json!({"url": url})),
            Err(err) => Response::from_blob(err),
        },
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
        assert_eq!(file_key(7, "report 2026.pdf"), Ok("notes/7/report_2026.pdf".into()));
        assert_eq!(file_key(7, "../../8/x"), Ok("notes/7/_.._8_x".into()));
        assert!(file_key(7, "../../8/x").unwrap().starts_with("notes/7/"));
        assert!(!file_key(7, "../../8/x").unwrap()["notes/7/".len()..].contains('/'));
        assert!(file_key(7, "...").is_err());
        assert!(file_key(7, "").is_err());
        assert!(file_key(7, &"a".repeat(121)).is_err());
    }
}
