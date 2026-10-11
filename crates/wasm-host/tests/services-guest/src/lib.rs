//! A test App over `jc:app@0.1.1`: `/api/caller` (who the host says the caller is), `/api/email`
//! (a message to the caller) and `/api/ai` (a completion under a schema), each answering what the
//! host said, a refusal as `Response::from_service` writes it.

use jc_app_sdk::ai::{self, Completion, Message, Role};
use jc_app_sdk::email::{self, Email, Recipients};
use jc_app_sdk::http::{Params, Request, Response, Router};

fn caller(_: &Request, _: &Params) -> Response {
    match jc_app_sdk::identity::caller() {
        Some(person) => Response::json(
            200,
            &serde_json::json!({ "id": person.id, "name": person.name, "email": person.email, "roles": person.roles }),
        ),
        None => Response::text(200, "nobody"),
    }
}

fn mail(_: &Request, _: &Params) -> Response {
    let message = Email {
        to: Recipients::Me,
        subject: "Today's defects".into(),
        text: "Three new.".into(),
        html: None,
    };
    match email::send(&message) {
        Ok(id) => Response::text(202, &id),
        Err(err) => Response::from_service(err),
    }
}

fn complete(_: &Request, _: &Params) -> Response {
    let request = Completion {
        messages: vec![Message { role: Role::User, content: "Rate a loose kerb stone, 1 to 5.".into() }],
        max_tokens: Some(20),
        schema: Some(r#"{"type":"object","required":["severity"]}"#.into()),
    };
    match ai::complete(&request) {
        Ok(answer) => Response::text(200, &format!("{} {} {}", answer.json.unwrap_or_default(), answer.tokens_in, answer.tokens_out)),
        Err(err) => Response::from_service(err),
    }
}

fn handle(request: Request) -> Response {
    Router::new()
        .get("/api/caller", caller)
        .post("/api/email", mail)
        .post("/api/ai", complete)
        .handle(&request)
}

jc_app_sdk::app!(handle);
