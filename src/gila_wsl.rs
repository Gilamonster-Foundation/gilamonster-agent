//! Rust-native `gila wsl` — Phase 3 of the gila-parity plan.
//!
//! WSL (Windows Subsystem for Linux) utilities: detect whether the current
//! host is running under WSL and report the Windows-interop status. Pure
//! detection (over injectable file contents / env) is unit-testable; the
//! binary's `run_*` arm reads the real `/proc/version` and prints.

/// True when `/proc/version` content indicates a WSL kernel.
pub fn is_wsl(proc_version: &str) -> bool {
    let v = proc_version.to_ascii_lowercase();
    v.contains("microsoft") || v.contains("wsl")
}

/// True when the `WSL_DISTRO_NAME` env var is set (a strong WSL signal).
pub fn wsl_env_present(env_val: Option<&str>) -> bool {
    env_val.map(|s| !s.is_empty()).unwrap_or(false)
}

/// A human-readable WSL status report from the detection signals.
pub fn wsl_report(proc_version: &str, env_val: Option<&str>) -> String {
    let kernel = is_wsl(proc_version);
    let env = wsl_env_present(env_val);
    match (kernel, env) {
        (true, true) => {
            format!("WSL detected (distro: {})", env_val.unwrap_or("unknown"))
        }
        (true, false) => "WSL kernel detected (no WSL_DISTRO_NAME set)".to_string(),
        (false, true) => {
            format!(
                "WSL env present (distro: {}) but kernel string is not WSL",
                env_val.unwrap_or("unknown")
            )
        }
        (false, false) => "not running under WSL".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WSL_PROC: &str = "Linux version 5.15.90.1-microsoft-standard-WSL2 (root@...)";
    const LINUX_PROC: &str = "Linux version 6.5.0-generic (buildd@...)";

    #[test]
    fn detects_wsl_kernel_string() {
        assert!(is_wsl(WSL_PROC));
        assert!(is_wsl("... WSL2 ..."));
        assert!(!is_wsl(LINUX_PROC));
    }

    #[test]
    fn detects_wsl_env() {
        assert!(wsl_env_present(Some("Ubuntu")));
        assert!(!wsl_env_present(Some("")));
        assert!(!wsl_env_present(None));
    }

    #[test]
    fn report_combines_signals() {
        assert!(wsl_report(WSL_PROC, Some("Ubuntu")).contains("distro: Ubuntu"));
        assert!(wsl_report(WSL_PROC, None).contains("no WSL_DISTRO_NAME"));
        assert!(wsl_report(LINUX_PROC, Some("Ubuntu")).contains("not WSL"));
        assert_eq!(wsl_report(LINUX_PROC, None), "not running under WSL");
    }
}
