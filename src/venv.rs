//! Resolve which `gilacap` multiplexer to invoke, and under which interpreter.
//!
//! A capability is served by the SDK's `gilacap` console script living in a
//! Python venv — the managed `~/.gila/caps-venv` by default. The harness must
//! decide *which* `gilacap` to spawn. Resolution is pure (its inputs are
//! injected), so the precedence is unit-tested without touching the real
//! environment or filesystem; [`from_env`] is the thin real-world wrapper.

use std::path::{Path, PathBuf};

/// How to invoke `gilacap`: the program to exec, plus any leading args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GilacapCmd {
    /// The program to execute — a `gilacap` console script, or a Python
    /// interpreter (when resolved via `GILA_CAP_PYTHON`).
    pub program: String,
    /// Leading args before the subcommand (e.g. `-m gilamonster_capability.console`
    /// for the interpreter form; empty for a direct `gilacap`).
    pub base_args: Vec<String>,
}

impl GilacapCmd {
    /// Build the full argv for a `gilacap` subcommand, e.g. `argv(&["mcp",
    /// "confluence"])` → `[base_args…, "mcp", "confluence"]`.
    #[must_use]
    pub fn argv(&self, sub: &[&str]) -> Vec<String> {
        let mut v = self.base_args.clone();
        v.extend(sub.iter().map(|s| (*s).to_string()));
        v
    }
}

/// The environment inputs venv resolution consults (injected for testability).
#[derive(Debug, Default, Clone)]
pub struct VenvEnv {
    /// `GILA_CAP_PYTHON` — an interpreter to run the console module under.
    pub gila_cap_python: Option<String>,
    /// `GILA_CAP_VENV` — a venv whose `bin/gilacap` to run.
    pub gila_cap_venv: Option<String>,
    /// `VIRTUAL_ENV` — the currently-activated venv, if any.
    pub virtual_env: Option<String>,
    /// `HOME` — to locate the managed default venv.
    pub home: Option<String>,
}

impl VenvEnv {
    /// Read the real process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            gila_cap_python: std::env::var("GILA_CAP_PYTHON").ok(),
            gila_cap_venv: std::env::var("GILA_CAP_VENV").ok(),
            virtual_env: std::env::var("VIRTUAL_ENV").ok(),
            home: std::env::var("HOME").ok(),
        }
    }
}

fn venv_gilacap(venv: &str) -> GilacapCmd {
    GilacapCmd {
        program: Path::new(venv)
            .join("bin")
            .join("gilacap")
            .to_string_lossy()
            .into_owned(),
        base_args: Vec::new(),
    }
}

/// Resolve the `gilacap` invocation. Precedence, first match wins:
///
/// 1. an explicit `venv_override` (a capability's `venv =` / the manifest global)
///    → `<venv>/bin/gilacap`;
/// 2. `GILA_CAP_PYTHON` → `<python> -m gilamonster_capability.console`;
/// 3. `GILA_CAP_VENV` → `<venv>/bin/gilacap`;
/// 4. `$VIRTUAL_ENV` → `<venv>/bin/gilacap`;
/// 5. the managed default `<home>/.gila/caps-venv/bin/gilacap` (iff it `exists`);
/// 6. otherwise bare `gilacap` on `PATH`.
pub fn resolve(
    venv_override: Option<&str>,
    env: &VenvEnv,
    exists: &dyn Fn(&Path) -> bool,
) -> GilacapCmd {
    if let Some(v) = venv_override {
        return venv_gilacap(v);
    }
    if let Some(py) = &env.gila_cap_python {
        return GilacapCmd {
            program: py.clone(),
            base_args: vec!["-m".into(), "gilamonster_capability.console".into()],
        };
    }
    if let Some(v) = &env.gila_cap_venv {
        return venv_gilacap(v);
    }
    if let Some(v) = &env.virtual_env {
        return venv_gilacap(v);
    }
    if let Some(home) = &env.home {
        let managed: PathBuf = Path::new(home).join(".gila/caps-venv/bin/gilacap");
        if exists(&managed) {
            return GilacapCmd {
                program: managed.to_string_lossy().into_owned(),
                base_args: Vec::new(),
            };
        }
    }
    GilacapCmd {
        program: "gilacap".into(),
        base_args: Vec::new(),
    }
}

/// Resolve against the real process environment and filesystem.
#[must_use]
pub fn from_env(venv_override: Option<&str>) -> GilacapCmd {
    resolve(venv_override, &VenvEnv::from_process(), &|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_exists(_: &Path) -> bool {
        false
    }
    fn always_exists(_: &Path) -> bool {
        true
    }

    fn env() -> VenvEnv {
        VenvEnv {
            gila_cap_python: Some("/opt/py/bin/python3".into()),
            gila_cap_venv: Some("/opt/capvenv".into()),
            virtual_env: Some("/opt/active".into()),
            home: Some("/home/op".into()),
        }
    }

    #[test]
    fn explicit_override_wins_over_everything() {
        let g = resolve(Some("/srv/v"), &env(), &always_exists);
        assert_eq!(g.program, "/srv/v/bin/gilacap");
        assert!(g.base_args.is_empty());
    }

    #[test]
    fn python_env_uses_the_console_module() {
        let mut e = env();
        e.gila_cap_python = Some("/opt/py/bin/python3".into());
        let g = resolve(None, &e, &never_exists);
        assert_eq!(g.program, "/opt/py/bin/python3");
        assert_eq!(
            g.argv(&["list"]),
            ["-m", "gilamonster_capability.console", "list"]
        );
    }

    #[test]
    fn cap_venv_then_virtual_env_then_managed_then_path() {
        // GILA_CAP_VENV beats VIRTUAL_ENV.
        let e = VenvEnv {
            gila_cap_python: None,
            gila_cap_venv: Some("/cap".into()),
            virtual_env: Some("/active".into()),
            home: Some("/home/op".into()),
        };
        assert_eq!(resolve(None, &e, &never_exists).program, "/cap/bin/gilacap");

        // VIRTUAL_ENV when no GILA_CAP_VENV.
        let e2 = VenvEnv {
            gila_cap_venv: None,
            ..e.clone()
        };
        assert_eq!(
            resolve(None, &e2, &never_exists).program,
            "/active/bin/gilacap"
        );

        // Managed default when it exists and nothing else is set.
        let e3 = VenvEnv {
            virtual_env: None,
            ..e2.clone()
        };
        assert_eq!(
            resolve(None, &e3, &always_exists).program,
            "/home/op/.gila/caps-venv/bin/gilacap"
        );

        // Bare `gilacap` when the managed venv is absent.
        assert_eq!(resolve(None, &e3, &never_exists).program, "gilacap");
    }

    #[test]
    fn argv_prepends_base_args() {
        let g = GilacapCmd {
            program: "py".into(),
            base_args: vec!["-m".into(), "mod".into()],
        };
        assert_eq!(g.argv(&["mcp", "x"]), ["-m", "mod", "mcp", "x"]);
    }
}
