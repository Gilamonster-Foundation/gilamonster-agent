//! Launch posture for the inherited newt coder: **always ambient**.
//!
//! Gila trades confinement for productive pragmatism (the pi.dev trade). Every
//! `gila` surface starts with full host filesystem, network, and command
//! authority and dispatches commands on the native host shell. There is no
//! OCAP posture to opt back into; newt remains the confined airframe.
//!
//! The baseline is a composition of newt's own switches, applied before any
//! async runtime or inherited newt component starts, then frozen once.

use std::path::{Path, PathBuf};

/// One deterministic mutation applied before newt freezes launch authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvEdit {
    /// Environment variable to mutate.
    pub key: &'static str,
    /// Value to set, or `None` to remove the variable.
    pub value: Option<&'static str>,
}

const AMBIENT_EDITS: [EnvEdit; 4] = [
    EnvEdit {
        key: "NEWT_DISABLE_OCAP",
        value: Some("1"),
    },
    EnvEdit {
        key: "NEWT_FULL_ACCESS",
        value: Some("1"),
    },
    EnvEdit {
        key: "NEWT_NO_ROUTE",
        value: Some("1"),
    },
    EnvEdit {
        key: "NEWT_UNSAFE_HOST_EXEC",
        value: None,
    },
];

/// Returns the environment edits of the ambient posture.
#[must_use]
pub fn environment_edits() -> &'static [EnvEdit] {
    &AMBIENT_EDITS
}

/// Applies the ambient posture and freezes newt's process authority.
///
/// This must run on the single-threaded startup path, before the Tokio
/// runtime or any inherited newt component is initialized.
pub fn apply_and_freeze() {
    apply_and_freeze_with_config(None);
}

/// Applies the ambient posture using one already-resolved configuration.
///
/// Headless runs resolve their explicit profile before launch so shell
/// posture and inference cannot read different configurations.
pub fn apply_and_freeze_with_config(config: Option<&newt_core::Config>) {
    for edit in environment_edits() {
        match edit.value {
            Some(value) => std::env::set_var(edit.key, value),
            None => std::env::remove_var(edit.key),
        }
    }

    let shell = match config {
        Some(config) => config.shell.clone(),
        None => newt_core::Config::resolve()
            .ok()
            .and_then(|config| config.shell),
    };
    std::env::set_var("NEWT_SHELL_ENGINE", shell_engine().as_str());
    let passthrough = shell
        .and_then(|config| config.env_passthrough)
        .unwrap_or_else(newt_core::shell_env_passthrough_default);
    std::env::set_var("NEWT_SHELL_ENV_PASSTHROUGH", passthrough.join(":"));

    arm_flight_recorder();

    let authority = newt_core::launch_authority::LaunchAuthority::from_env();
    newt_core::launch_authority::freeze(authority);
}

/// The fixed startup shell choice: newt's platform-aware full-access engine
/// (`host` on Unix, `brush` on Windows). A configured engine is not consulted —
/// the ambient posture always runs the native shell.
#[must_use]
pub fn shell_engine() -> newt_core::ShellEngine {
    newt_core::full_access_default_engine()
}

/// Returns the default append-only recorder path for unconfined actions.
#[must_use]
pub fn flight_recorder_path(config_path: &Path) -> PathBuf {
    config_path
        .with_file_name("flight-recorder")
        .join("unconfined.jsonl")
}

/// Arms newt's shadow-OCAP flight recorder. This does not confine anything; it
/// records the authority a leash would have needed (observability, not policy).
/// `NEWT_FLIGHT_RECORDER=off` / `0` disables it.
fn arm_flight_recorder() {
    let key = newt_core::flight_recorder::CAPTURE_PATH_ENV;
    if std::env::var_os(key).is_none() {
        if let Some(config_path) = newt_core::Config::user_config_path() {
            std::env::set_var(key, flight_recorder_path(&config_path));
        }
    }
    if std::env::var(key).is_ok_and(|value| value.eq_ignore_ascii_case("off") || value == "0") {
        std::env::remove_var(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambient_sets_full_native_shell_contract() {
        assert_eq!(
            environment_edits(),
            [
                EnvEdit {
                    key: "NEWT_DISABLE_OCAP",
                    value: Some("1"),
                },
                EnvEdit {
                    key: "NEWT_FULL_ACCESS",
                    value: Some("1"),
                },
                EnvEdit {
                    key: "NEWT_NO_ROUTE",
                    value: Some("1"),
                },
                EnvEdit {
                    key: "NEWT_UNSAFE_HOST_EXEC",
                    value: None,
                },
            ]
        );
    }

    #[test]
    fn ambient_never_removes_a_widening_switch() {
        // Regression: the old OCAP posture removed every widening switch. With
        // OCAP gone, no edit may clear NEWT_DISABLE_OCAP / NEWT_FULL_ACCESS.
        for key in ["NEWT_DISABLE_OCAP", "NEWT_FULL_ACCESS", "NEWT_NO_ROUTE"] {
            let edit = environment_edits()
                .iter()
                .find(|e| e.key == key)
                .expect("widening switch present");
            assert_eq!(
                edit.value,
                Some("1"),
                "{key} must be widened, never cleared"
            );
        }
    }

    #[test]
    fn shell_choice_is_the_platform_full_access_engine() {
        assert_eq!(shell_engine(), newt_core::full_access_default_engine());
    }

    #[test]
    fn flight_recorder_lives_beside_the_newt_config() {
        assert_eq!(
            flight_recorder_path(Path::new("/home/gila/.newt/config.toml")),
            PathBuf::from("/home/gila/.newt/flight-recorder/unconfined.jsonl")
        );
    }
}
