//! `gila voice` — the built-in voice interface (agent-voice "Path B").
//!
//! gila compiles the agent-voice facade crate into itself (feature `voice`),
//! so speaking and listening are in-process calls on a [`VoiceStack`] — no
//! subprocess, no MCP hop. The stack is fully local: cpal capture/playback,
//! Silero VAD (bundled model), whisper.cpp STT, Piper TTS via ONNX Runtime.
//! See agent-voice's docs/GILA_PLUGIN.md for the two integration paths; the
//! MCP path (`agent-voice-mcp` in `[[mcp_servers]]`) needs no gila build at
//! all and coexists with this one.
//!
//! Settings: `~/.gila/voice.toml` deserializes into `VoiceSettings` (every
//! table optional); absent file = defaults. Models are provisioned per
//! agent-voice's README (`scripts/fetch-models.sh`).

use std::path::PathBuf;

use agent_voice::{VoiceSettings, VoiceStack};
use anyhow::Context;

/// `gila voice …` subcommands. The whole enum (and the `Voice` arm of the
/// top-level `Command`) is compiled out unless the `voice` feature is on.
#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
pub enum VoiceCmd {
    /// Speak text aloud through the default output device (local Piper TTS).
    Say {
        /// The text to speak.
        text: String,
    },
    /// Record from the microphone and print the transcript (local Whisper).
    Listen {
        /// Record for a fixed number of seconds instead of stopping on
        /// VAD-detected silence.
        #[arg(long)]
        duration: Option<u64>,
    },
    /// List audio input/output devices.
    Devices,
}

/// Resolve `~/.gila/voice.toml` (same env-first convention as the
/// capabilities manifest: HOME, then USERPROFILE for Windows).
fn settings_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".gila").join("voice.toml"))
}

/// Load settings — a missing file yields defaults; a malformed one is an
/// error (silently ignoring a config the operator wrote hides real mistakes).
fn load_settings() -> anyhow::Result<VoiceSettings> {
    let Some(path) = settings_path() else {
        return Ok(VoiceSettings::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(VoiceSettings::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Dispatch a `gila voice` subcommand. Builds the stack fresh per invocation —
/// fine for a one-shot CLI; a resident surface (TUI push-to-talk) should hold
/// one `VoiceStack` for the process lifetime instead.
pub async fn run(cmd: VoiceCmd) -> anyhow::Result<()> {
    let stack = VoiceStack::new(load_settings()?)?;
    match cmd {
        VoiceCmd::Say { text } => {
            let result = stack.say(&text).await;
            if result.success {
                Ok(())
            } else {
                anyhow::bail!(
                    "speech failed: {}",
                    result.message.unwrap_or_else(|| "unknown error".to_string())
                )
            }
        }
        VoiceCmd::Listen { duration } => {
            let result = match duration {
                Some(seconds) => stack.listen_for(seconds as f32).await,
                None => stack.listen().await,
            };
            if result.success {
                println!("{}", result.transcript);
                Ok(())
            } else {
                anyhow::bail!(
                    "listen failed: {}",
                    result.message.unwrap_or_else(|| "unknown error".to_string())
                )
            }
        }
        VoiceCmd::Devices => {
            let listing = stack.devices().await;
            if !listing.success {
                anyhow::bail!(
                    "device enumeration failed: {}",
                    listing.message.unwrap_or_else(|| "unknown error".to_string())
                );
            }
            println!("Input devices:");
            for d in listing.devices.iter().filter(|d| d.supports_input()) {
                println!("  {:>3}  {} ({} ch)", d.index, d.name, d.max_input_channels);
            }
            println!("Output devices:");
            for d in listing.devices.iter().filter(|d| d.supports_output()) {
                println!("  {:>3}  {} ({} ch)", d.index, d.name, d.max_output_channels);
            }
            Ok(())
        }
    }
}
