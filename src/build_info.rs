//! Compile-time identity for the exact Gila build.
//!
//! Cargo supplies the package version; `build.rs` adds the checked-out Git
//! commit and marks builds made from a modified worktree as `dirty`.

/// SemVer package version from the package manifest.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Twelve-character Gila Git commit captured when the crate was built.
pub const GIT_COMMIT: &str = env!("GILA_BUILD_GIT_COMMIT");

/// Git commit plus a `-dirty` suffix when tracked or untracked changes existed.
pub const SOURCE_ID: &str = env!("GILA_BUILD_SOURCE_ID");

/// User-visible build identity, for example `0.4.0 (6eb3e02644ab)`.
pub const VERSION_WITH_COMMIT: &str = env!("GILA_BUILD_VERSION");

/// Compiled-in default harness/brand name — the GitHub User
/// [`gilamonster-agent`](https://github.com/Gilamonster-Foundation/gilamonster-agent),
/// overridden by `GILA_BRAND_NAME` for a downstream rebrand. Mirrors newt-agent's
/// `build_info::DEFAULT_BRAND_NAME` / `harness_name()` so the two modules are
/// structurally identical (newt uses `NEWT_BRAND_NAME`; gila uses `GILA_BRAND_NAME`).
pub const DEFAULT_BRAND_NAME: &str = "gilamonster-agent";

/// The execution harness identity a live contribution is attributed under
/// (`GILA_BRAND_NAME` env override, else [`DEFAULT_BRAND_NAME`]).
///
/// This is gila's analogue of newt-core's `build_info::harness_name()`. The
/// inherited newt-git attribution footer continues to read `NEWT_BRAND_NAME`
/// (which `main.rs` sets at startup); this fn is the authoritative source for
/// any gila-native code that asks "which harness is this".
#[must_use]
pub fn harness_name() -> String {
    std::env::var("GILA_BRAND_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BRAND_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes the `GILA_BRAND_NAME`-mutating tests (the role `serial_test`
    /// plays in newt-agent; gila avoids that dev-dep with a local lock).
    static BRAND_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn build_version_contains_package_and_source_identity() {
        assert!(VERSION_WITH_COMMIT.starts_with(PACKAGE_VERSION));
        assert!(VERSION_WITH_COMMIT.contains(GIT_COMMIT));
        assert!(SOURCE_ID.starts_with(GIT_COMMIT));
    }

    #[test]
    fn harness_name_defaults_to_gilamonster_agent() {
        let _guard = BRAND_LOCK.lock().unwrap();
        // SAFETY: serialized against other GILA_BRAND_NAME-mutating tests.
        unsafe { std::env::remove_var("GILA_BRAND_NAME") };
        assert_eq!(harness_name(), "gilamonster-agent");
    }

    #[test]
    fn harness_name_honors_rebrand_override() {
        let _guard = BRAND_LOCK.lock().unwrap();
        // SAFETY: serialized against other GILA_BRAND_NAME-mutating tests.
        unsafe { std::env::set_var("GILA_BRAND_NAME", "some-downstream") };
        assert_eq!(harness_name(), "some-downstream");
        unsafe { std::env::remove_var("GILA_BRAND_NAME") };
    }

    #[test]
    fn harness_name_ignores_blank_override() {
        let _guard = BRAND_LOCK.lock().unwrap();
        // SAFETY: serialized against other GILA_BRAND_NAME-mutating tests.
        unsafe { std::env::set_var("GILA_BRAND_NAME", "   ") };
        assert_eq!(harness_name(), "gilamonster-agent");
        unsafe { std::env::remove_var("GILA_BRAND_NAME") };
    }
}
