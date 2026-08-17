//! Launch posture for the inherited newt coder.

use std::path::{Path, PathBuf};

/// Selects the authority baseline for `gila code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchPosture {
    /// Full ambient authority with commands executed on the native host shell.
    Ambient,
    /// Newt's configured object-capability confinement posture.
    Ocap,
}

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

const OCAP_EDITS: [EnvEdit; 6] = [
    EnvEdit {
        key: "NEWT_DISABLE_OCAP",
        value: None,
    },
    EnvEdit {
        key: "NEWT_FULL_ACCESS",
        value: None,
    },
    EnvEdit {
        key: "NEWT_UNSAFE_HOST_EXEC",
        value: None,
    },
    EnvEdit {
        key: "NEWT_NO_ROUTE",
        value: None,
    },
    EnvEdit {
        key: "NEWT_SHELL_ENGINE",
        value: None,
    },
    EnvEdit {
        key: "NEWT_SHELL_ENV_PASSTHROUGH",
        value: None,
    },
];

impl LaunchPosture {
    /// Returns the environment edits for this launch posture.
    #[must_use]
    pub fn environment_edits(self) -> &'static [EnvEdit] {
        match self {
            Self::Ambient => &AMBIENT_EDITS,
            Self::Ocap => &OCAP_EDITS,
        }
    }

    /// Applies this posture and freezes newt's process authority.
    ///
    /// This must run on the single-threaded startup path, before the Tokio
    /// runtime or any inherited newt component is initialized.
    pub fn apply_and_freeze(self) {
        self.apply_and_freeze_with_config(None);
    }

    /// Applies this posture using one already-resolved configuration.
    ///
    /// Headless runs resolve their explicit profile before launch so shell
    /// posture and inference cannot read different configurations.
    pub fn apply_and_freeze_with_config(self, config: Option<&newt_core::Config>) {
        for edit in self.environment_edits() {
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
        if let Some(engine) = shell_engine(self, shell.as_ref()) {
            std::env::set_var("NEWT_SHELL_ENGINE", engine.as_str());
        }
        let passthrough = shell
            .and_then(|config| config.env_passthrough)
            .unwrap_or_else(newt_core::shell_env_passthrough_default);
        std::env::set_var("NEWT_SHELL_ENV_PASSTHROUGH", passthrough.join(":"));

        if self == Self::Ambient {
            arm_flight_recorder();
        }

        let authority = newt_core::launch_authority::LaunchAuthority::from_env();
        newt_core::launch_authority::freeze(authority);
    }
}

/// Resolves the fixed startup shell choice for a launch posture.
///
/// Ambient Gila always selects Newt's platform-aware full-access engine
/// (`host` on Unix, `brush` on Windows). OCAP honors an explicitly configured
/// engine, while `None` deliberately leaves Newt's confined default to its
/// dispatch-time kernel-fence check.
#[must_use]
pub fn shell_engine(
    posture: LaunchPosture,
    configured: Option<&newt_core::ShellConfig>,
) -> Option<newt_core::ShellEngine> {
    match posture {
        LaunchPosture::Ambient => Some(newt_core::full_access_default_engine()),
        LaunchPosture::Ocap => configured.and_then(|shell| shell.engine),
    }
}

/// Returns the default append-only recorder path for unconfined actions.
#[must_use]
pub fn flight_recorder_path(config_path: &Path) -> PathBuf {
    config_path
        .with_file_name("flight-recorder")
        .join("unconfined.jsonl")
}

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
            LaunchPosture::Ambient.environment_edits(),
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
    fn ocap_removes_every_inherited_widening_switch() {
        assert_eq!(
            LaunchPosture::Ocap.environment_edits(),
            [
                EnvEdit {
                    key: "NEWT_DISABLE_OCAP",
                    value: None,
                },
                EnvEdit {
                    key: "NEWT_FULL_ACCESS",
                    value: None,
                },
                EnvEdit {
                    key: "NEWT_UNSAFE_HOST_EXEC",
                    value: None,
                },
                EnvEdit {
                    key: "NEWT_NO_ROUTE",
                    value: None,
                },
                EnvEdit {
                    key: "NEWT_SHELL_ENGINE",
                    value: None,
                },
                EnvEdit {
                    key: "NEWT_SHELL_ENV_PASSTHROUGH",
                    value: None,
                },
            ]
        );
    }

    #[test]
    fn ambient_shell_choice_is_platform_aware_and_ocap_honors_config() {
        assert_eq!(
            shell_engine(LaunchPosture::Ambient, None),
            Some(newt_core::full_access_default_engine())
        );
        let configured = newt_core::ShellConfig {
            engine: Some(newt_core::ShellEngine::Brush),
            env_passthrough: None,
        };
        assert_eq!(
            shell_engine(LaunchPosture::Ocap, Some(&configured)),
            Some(newt_core::ShellEngine::Brush)
        );
        assert_eq!(shell_engine(LaunchPosture::Ocap, None), None);
    }

    #[test]
    fn flight_recorder_lives_beside_the_newt_config() {
        assert_eq!(
            flight_recorder_path(Path::new("/home/gila/.newt/config.toml")),
            PathBuf::from("/home/gila/.newt/flight-recorder/unconfined.jsonl")
        );
    }
}
