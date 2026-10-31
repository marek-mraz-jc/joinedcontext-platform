//! What a guest hands a host function, arbitrary: a `jc:app/sql` statement, a `jc:app/blob` key and
//! an outgoing request's address. None may panic, and what passes keeps its promise: a key stays
//! under the App's prefix, and a call reaches the App's own Endpoint on the gateway alone.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wasm_host::files::checked;
use wasm_host::host::endpoint_target;
use wasm_host::sql::check;

fuzz_target!(|bytes: &[u8]| {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let _ = check(text);
    if let Ok(key) = checked(text) {
        assert!(!key.starts_with('/') && !key.contains('\\'), "{key:?}");
        assert!(
            key.split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."),
            "{key:?}"
        );
    }
    if let Ok(uri) = text.parse::<http::Uri>() {
        if let Some(url) = endpoint_target("http://gw:8080", "ep1", &uri) {
            let rest = url
                .strip_prefix("http://gw:8080/api/endpoint/ep1/")
                .unwrap_or_else(|| panic!("{url} left the Endpoint"));
            let path = rest.split('?').next().unwrap_or_default();
            assert!(
                path.split('/').all(|s| s != ".." && s != "."),
                "{url} steps out"
            );
        }
    }
});
