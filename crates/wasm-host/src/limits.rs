//! What one request and one App may use (ADR-N-044 §2.6, AP-146). The defaults are the ADR's; a
//! deployment may lower them, never raise them past the hard ceilings below.

use std::time::Duration;

/// The limits a shard enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Linear memory of one instance.
    pub memory_bytes: usize,
    /// Wall time of one request, enforced by epoch interruption.
    pub wall_time: Duration,
    /// CPU fuel of one request: roughly one unit per WebAssembly instruction.
    pub fuel: u64,
    /// The request body an App is handed, read whole before it runs.
    pub request_bytes: usize,
    /// The response body an App may send.
    pub response_bytes: usize,
    /// Requests of one App running at once.
    pub per_app_concurrency: usize,
    /// Requests of one tenant (project) running at once.
    pub per_tenant_concurrency: usize,
    /// Components compiled and kept in memory per shard.
    pub cached_components: usize,
    /// Instances the pooling allocator keeps ready, across all Apps.
    pub pooled_instances: u32,
}

/// The epoch ticks every this often; a deadline is a number of ticks.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 << 20,
            wall_time: Duration::from_secs(5),
            // Above what the wall time lets a busy loop burn (measured about 15 billion units a
            // second on the dev shard's CPU, T-3339): the wall time stops a spinning App, and fuel
            // is the backstop should the epoch clock ever stall.
            fuel: 200_000_000_000,
            request_bytes: 2 << 20,
            response_bytes: 8 << 20,
            per_app_concurrency: 16,
            per_tenant_concurrency: 64,
            cached_components: 2_000,
            pooled_instances: 1_000,
        }
    }
}

impl Limits {
    /// The defaults, each lowered by its `JC_WASM_*` variable when that holds a smaller number.
    /// A larger one, or one that is not a number, is refused with the variable named.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut limits = Self::default();
        let lower = |name: &str, ceiling: u64| -> Result<u64, String> {
            match var(name) {
                None => Ok(ceiling),
                Some(text) => match text.trim().parse::<u64>() {
                    Ok(value) if value > 0 && value <= ceiling => Ok(value),
                    _ => Err(format!(
                        "{name} must be a whole number from 1 to {ceiling}, not `{text}`"
                    )),
                },
            }
        };
        limits.memory_bytes = lower("JC_WASM_MEMORY_BYTES", limits.memory_bytes as u64)? as usize;
        limits.wall_time = Duration::from_millis(lower(
            "JC_WASM_WALL_MS",
            limits.wall_time.as_millis() as u64,
        )?);
        limits.fuel = lower("JC_WASM_FUEL", limits.fuel)?;
        limits.request_bytes =
            lower("JC_WASM_REQUEST_BYTES", limits.request_bytes as u64)? as usize;
        limits.response_bytes =
            lower("JC_WASM_RESPONSE_BYTES", limits.response_bytes as u64)? as usize;
        limits.per_app_concurrency =
            lower("JC_WASM_APP_CONCURRENCY", limits.per_app_concurrency as u64)? as usize;
        limits.per_tenant_concurrency = lower(
            "JC_WASM_TENANT_CONCURRENCY",
            limits.per_tenant_concurrency as u64,
        )? as usize;
        limits.cached_components =
            lower("JC_WASM_CACHED_COMPONENTS", limits.cached_components as u64)? as usize;
        limits.pooled_instances = lower(
            "JC_WASM_POOLED_INSTANCES",
            u64::from(limits.pooled_instances),
        )? as u32;
        Ok(limits)
    }

    /// The wall time as epoch ticks, at least one.
    pub fn deadline_ticks(&self) -> u64 {
        (self.wall_time.as_millis() / EPOCH_TICK.as_millis()).max(1) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variable_lowers_a_limit_and_never_raises_it() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let lowered = Limits::from_env(env(&[
            ("JC_WASM_WALL_MS", "1000"),
            ("JC_WASM_MEMORY_BYTES", "1048576"),
        ]))
        .unwrap();
        assert_eq!(lowered.wall_time, Duration::from_secs(1));
        assert_eq!(lowered.memory_bytes, 1 << 20);
        assert_eq!(lowered.deadline_ticks(), 100);
        let raised = Limits::from_env(env(&[("JC_WASM_MEMORY_BYTES", "134217728")])).unwrap_err();
        assert!(
            raised.contains("JC_WASM_MEMORY_BYTES") && raised.contains("67108864"),
            "{raised}"
        );
        assert!(Limits::from_env(env(&[("JC_WASM_FUEL", "lots")])).is_err());
        assert!(Limits::from_env(env(&[("JC_WASM_FUEL", "0")])).is_err());
        assert_eq!(Limits::from_env(|_| None).unwrap(), Limits::default());
    }
}
