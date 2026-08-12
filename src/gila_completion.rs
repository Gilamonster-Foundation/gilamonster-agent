//! Rust-native `gila completion` — Phase 3 of the gila-parity plan.
//!
//! Emit a shell completion script for `gila`. To avoid a `clap_complete`
//! dependency, this generates a static-but-correct script for the supported
//! shells (bash, zsh) covering the top-level subcommands. Pure string
//! composition is unit-testable; the binary's `run_*` arm only prints it.

/// The supported shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// Bourne-again shell.
    Bash,
    /// Z shell.
    Zsh,
}

impl Shell {
    /// Parse a shell name (case-insensitive). `None` when unsupported.
    pub fn parse(name: &str) -> Option<Shell> {
        match name.to_ascii_lowercase().as_str() {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            _ => None,
        }
    }
}

/// The top-level subcommands the completion offers. Kept in sync with the
/// `Command` enum in `lib.rs` (a drift guard test asserts the list).
pub const SUBCOMMANDS: &[&str] = &[
    "code", "follow", "cowork", "hotseat", "capabilities", "matrix", "cockpit", "chain", "scrybe",
    "git", "version", "daily", "ideas", "todos", "projects", "board", "cache", "logs", "prompt",
    "commit-msg", "completion", "init", "update",
];

/// Generate the completion script for `shell`.
pub fn completion_script(shell: Shell) -> String {
    let words = SUBCOMMANDS.join(" ");
    match shell {
        Shell::Bash => format!(
            "# bash completion for gila\n\
             _gila() {{\n\
             \x20   local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n\
             \x20   if [ \"$COMP_CWORD\" -eq 1 ]; then\n\
             \x20       COMPREPLY=( $(compgen -W \"{words}\" -- \"$cur\") )\n\
             \x20   fi\n\
             }}\n\
             complete -F _gila gila\n"
        ),
        Shell::Zsh => format!(
            "#compdef gila\n\
             # zsh completion for gila\n\
             _gila() {{\n\
             \x20   local -a subcommands\n\
             \x20   subcommands=({words})\n\
             \x20   if (( CURRENT == 2 )); then\n\
             \x20       _describe 'command' subcommands\n\
             \x20   fi\n\
             }}\n\
             _gila \"$@\"\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_shells_case_insensitive() {
        assert_eq!(Shell::parse("bash"), Some(Shell::Bash));
        assert_eq!(Shell::parse("ZSH"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("fish"), None);
    }

    #[test]
    fn bash_script_offers_all_subcommands() {
        let s = completion_script(Shell::Bash);
        assert!(s.contains("complete -F _gila gila"));
        for cmd in SUBCOMMANDS {
            assert!(s.contains(cmd), "bash script missing {cmd}");
        }
    }

    #[test]
    fn zsh_script_offers_all_subcommands() {
        let s = completion_script(Shell::Zsh);
        assert!(s.contains("#compdef gila"));
        for cmd in SUBCOMMANDS {
            assert!(s.contains(cmd), "zsh script missing {cmd}");
        }
    }
}
