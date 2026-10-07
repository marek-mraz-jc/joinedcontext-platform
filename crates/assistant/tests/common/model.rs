//! The embedding model (`JC_ASSISTANT_TEST_MODEL_DIR`, filled by `scripts/ci/e5-small.sh`). A
//! missing variable is a failure that says what to set.

use std::path::PathBuf;

/// The model directory the tests load.
pub fn model_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("JC_ASSISTANT_TEST_MODEL_DIR").unwrap_or_else(|_| {
            panic!(
                "set JC_ASSISTANT_TEST_MODEL_DIR to a directory filled by scripts/ci/e5-small.sh"
            )
        }),
    )
}
