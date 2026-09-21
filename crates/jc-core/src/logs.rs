//! OPS-15: the one log format every platform binary writes — one JSON object per line on
//! stdout, the event's fields at the top level beside `level`, `target` and `timestamp`, and
//! nothing on the container's filesystem.

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Installs the JSON subscriber on stdout, filtered by `RUST_LOG` or by `default` when it is
/// unset, empty or unreadable. Fails only when a subscriber is already installed.
pub fn init(default: &str) -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    let filter = EnvFilter::builder().parse_lossy(match std::env::var("RUST_LOG") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => default.to_owned(),
    });
    tracing::subscriber::set_global_default(subscriber(std::io::stdout, filter))
}

/// The subscriber [`init`] installs, writing to `writer`.
pub fn subscriber<W>(writer: W, filter: EnvFilter) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(filter)
        .with_writer(writer)
        .finish()
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use serde_json::Value;
    use tracing_subscriber::EnvFilter;

    use super::subscriber;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// OPS-15: every line is one JSON object a collector parses without a pattern, and what is
    /// below the filter is not written at all.
    #[test]
    fn every_log_line_is_one_json_object_and_what_is_filtered_stays_out() {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let filter = EnvFilter::new("context_gateway=info");
        tracing::subscriber::with_default(subscriber(move || writer.clone(), filter), || {
            tracing::info!(target: "context_gateway", tenant = "bbsk", status = 403, "read refused");
            tracing::debug!(target: "context_gateway", "not written at info");
            tracing::info!(target: "hyper", "not a target the filter names");
        });
        let written = String::from_utf8(buffer.0.lock().expect("buffer").clone()).expect("utf-8");
        let lines: Vec<Value> = written
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
            .collect();
        assert_eq!(lines.len(), 1, "{written}");
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[0]["target"], "context_gateway");
        assert_eq!(lines[0]["message"], "read refused");
        assert_eq!(lines[0]["tenant"], "bbsk");
        assert_eq!(lines[0]["status"], 403);
        assert!(lines[0]["timestamp"].is_string(), "{written}");
    }
}
