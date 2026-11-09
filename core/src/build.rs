//! Build metadata: which release a binary was built from.
//!
//! `scripts/deploy.sh build <tag>` compiles with `VITALS_VERSION=<tag>`;
//! cargo rebuilds when that variable changes. Any other build (dev, tests,
//! `make serve`) has no version.

use serde::{Deserialize, Serialize};

/// The release tag set at compile time, e.g. `v2026.11.1`.
pub const VERSION: Option<&str> = option_env!("VITALS_VERSION");

/// Which build is answering (`GET /api/version`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    /// Release tag (`vYYYY.MM.N`) this binary was built from; null for a
    /// build that isn't a release.
    pub version: Option<String>,
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        version: VERSION.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_mirrors_the_compile_time_version() {
        assert_eq!(build_info().version.as_deref(), VERSION);
    }
}
