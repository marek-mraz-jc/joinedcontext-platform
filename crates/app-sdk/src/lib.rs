//! The guest side of a server WASM App (ADR-N-044, AP-143..AP-145): what an App's component is
//! written against. A request comes in as a [`http::Request`], a handler answers a
//! [`http::Response`], and the App's own database and files are [`sql`] and [`blob`]. The App
//! never holds a connection string, a password or a storage key: the host answers every call as
//! the App and no other.
//!
//! ```no_run
//! use jc_app_sdk::http::{Request, Response, Router};
//!
//! fn notes(_: &Request, _: &jc_app_sdk::http::Params) -> Response {
//!     match jc_app_sdk::sql::query("select id, body from notes order by id", &[]) {
//!         Ok(rows) => Response::json(200, &jc_app_sdk::sql::objects(&rows)),
//!         Err(err) => Response::from_sql(err),
//!     }
//! }
//!
//! fn handle(request: Request) -> Response {
//!     Router::new().get("/api/notes", notes).handle(&request)
//! }
//!
//! jc_app_sdk::app!(handle);
//! ```

wit_bindgen::generate!({
    path: "../wasm-host/wit",
    world: "jc:app/app-host",
    generate_all,
});

#[doc(hidden)]
pub use wasip2 as __wasip2;

pub mod http;

/// The App's own Postgres schema, through bound parameters only (AP-144).
pub mod sql {
    pub use crate::jc::app::sql::{Error, Rows, Value};

    /// Rows of a statement that reads; at most the host's row cap of them.
    pub fn query(statement: &str, params: &[Value]) -> Result<Rows, Error> {
        crate::jc::app::sql::query(statement, params)
    }

    /// The number of rows a statement that writes changed.
    pub fn execute(statement: &str, params: &[Value]) -> Result<u64, Error> {
        crate::jc::app::sql::execute(statement, params)
    }

    /// A value as JSON: a number, a string, a boolean, `null`, a JSON value as itself, bytes as a
    /// list of numbers.
    pub fn json(value: &Value) -> serde_json::Value {
        match value {
            Value::Null => serde_json::Value::Null,
            Value::Boolean(v) => (*v).into(),
            Value::Int(v) => (*v).into(),
            Value::Float(v) => serde_json::Number::from_f64(*v)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Value::Text(v) => v.clone().into(),
            Value::Bytes(v) => v.clone().into(),
            Value::Json(v) => serde_json::from_str(v).unwrap_or_else(|_| v.clone().into()),
        }
    }

    /// The rows as one JSON object per row, keyed by column name.
    pub fn objects(rows: &Rows) -> Vec<serde_json::Map<String, serde_json::Value>> {
        rows.values
            .iter()
            .map(|row| {
                rows.columns
                    .iter()
                    .cloned()
                    .zip(row.iter().map(json))
                    .collect()
            })
            .collect()
    }

    /// The rows read into `T` by their column names, as serde reads a JSON object.
    pub fn rows<T: serde::de::DeserializeOwned>(rows: &Rows) -> Result<Vec<T>, String> {
        objects(rows)
            .into_iter()
            .map(|object| {
                serde_json::from_value(serde_json::Value::Object(object))
                    .map_err(|err| err.to_string())
            })
            .collect()
    }

    impl From<i64> for Value {
        fn from(v: i64) -> Self {
            Value::Int(v)
        }
    }
    impl From<f64> for Value {
        fn from(v: f64) -> Self {
            Value::Float(v)
        }
    }
    impl From<bool> for Value {
        fn from(v: bool) -> Self {
            Value::Boolean(v)
        }
    }
    impl From<&str> for Value {
        fn from(v: &str) -> Self {
            Value::Text(v.to_owned())
        }
    }
    impl From<String> for Value {
        fn from(v: String) -> Self {
            Value::Text(v)
        }
    }
    impl<T: Into<Value>> From<Option<T>> for Value {
        fn from(v: Option<T>) -> Self {
            v.map_or(Value::Null, Into::into)
        }
    }
}

/// The App's own files, under its prefix; keys are relative to it (AP-145).
pub mod blob {
    pub use crate::jc::app::blob::{Error, Method};

    pub fn get(key: &str) -> Result<Vec<u8>, Error> {
        crate::jc::app::blob::get(key)
    }

    pub fn put(key: &str, data: &[u8], content_type: Option<&str>) -> Result<(), Error> {
        crate::jc::app::blob::put(key, data, content_type)
    }

    pub fn list(prefix: &str) -> Result<Vec<String>, Error> {
        crate::jc::app::blob::list_keys(prefix)
    }

    pub fn delete(key: &str) -> Result<(), Error> {
        crate::jc::app::blob::delete(key)
    }

    /// A URL the browser uploads to (`Method::Put`) or downloads from (`Method::Get`), for one
    /// key, valid for at most five minutes.
    pub fn presign(key: &str, method: Method, expires_seconds: u32) -> Result<String, Error> {
        crate::jc::app::blob::presign(key, method, expires_seconds)
    }
}

/// Exports `handler`, a `fn(http::Request) -> http::Response`, as the App's `wasi:http` handler.
/// Only on wasm32: on the build machine the App's unit tests link without it.
#[macro_export]
macro_rules! app {
    ($handler:path) => {
        #[cfg(target_arch = "wasm32")]
        struct __JcApp;
        #[cfg(target_arch = "wasm32")]
        impl $crate::__wasip2::exports::http::incoming_handler::Guest for __JcApp {
            fn handle(
                request: $crate::__wasip2::http::types::IncomingRequest,
                out: $crate::__wasip2::http::types::ResponseOutparam,
            ) {
                $crate::http::serve(request, out, $handler)
            }
        }
        #[cfg(target_arch = "wasm32")]
        $crate::__wasip2::http::proxy::export!(__JcApp);
    };
}
