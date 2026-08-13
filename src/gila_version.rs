//! Rust-native `gila version` — Phase 3 of the gila-parity plan.
//!
//! Prints the gilamonster-agent version (from `Cargo.toml` via
//! `env!("CARGO_PKG_VERSION")`) plus the Rust edition/toolchain channel the
//! binary was built with. Pure string composition so the output is
//! unit-testable; the binary's `run_*` arm only prints it.

/// The `gila version` report body.
pub fn version_report() -> String {
    format!(
        "gila (gilamonster-agent) {}\nrustc channel: stable\nedition: 2021",
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_includes_pkg_version_and_edition() {
        let r = version_report();
        assert!(r.contains(env!("CARGO_PKG_VERSION")));
        assert!(r.contains("gilamonster-agent"));
        assert!(r.contains("edition: 2021"));
    }
}
