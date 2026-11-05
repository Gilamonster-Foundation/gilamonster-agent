//! Rust-native `gila version` — Phase 3 of the gila-parity plan.
//!
//! Prints the gilamonster-agent package version and exact Git source identity
//! plus the Rust edition/toolchain channel the binary was built with. Pure
//! string composition so the output is unit-testable; the binary's `run_*`
//! arm only prints it.

use crate::build_info;

/// The `gila version` report body.
pub fn version_report() -> String {
    format!(
        "gila (gilamonster-agent) {}\nrustc channel: stable\nedition: 2021",
        build_info::VERSION_WITH_COMMIT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_includes_build_identity_and_edition() {
        let r = version_report();
        assert!(r.contains(build_info::VERSION_WITH_COMMIT));
        assert!(r.contains("gilamonster-agent"));
        assert!(r.contains("edition: 2021"));
    }
}
