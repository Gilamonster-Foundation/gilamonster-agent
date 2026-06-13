//! `gila follow` — read-only "follow me" shell observation, and the **shared
//! shell-observation channel** the whole cowork stack is built around.
//!
//! # What this is
//!
//! The human runs their *own* interactive shell (zsh/bash, `ssh`, the full
//! command suite) under `script -F <typescript>`. `gila follow` tails that
//! growing typescript, turns each new burst of shell activity into a
//! [`newt_core::agentic::ShellObservation`], and folds it into the agent's
//! context so the agent can **comment and assist** — without ever driving the
//! human's shell. The agent is a passenger, not a pilot.
//!
//! # The seam (the load-bearing part — issue #8)
//!
//! Tier A (this file: a typescript tail) is only the *first* producer. Tier B
//! (the hosted-PTY split pane, issue #10) feeds the **same** channel from a
//! different source. So the producer is factored out behind one trait and the
//! agent side is built exactly once:
//!
//! ```text
//!   ┌────────────────────┐   chunks    ┌──────────────────────┐   ShellObservation
//!   │ ObservationSource  │ ──────────► │  ObservationChannel  │ ─────────────────► TurnDriver
//!   │  (swappable)       │   (String)  │  redact + wrap + feed │   (submit_observation)
//!   └────────────────────┘             └──────────────────────┘
//!     Tier A: TypescriptTail
//!     Tier B (#10): PtyMirror  ← swaps in WITHOUT touching the channel/agent side
//! ```
//!
//! - [`ObservationSource`] is the producer seam: `next_chunk()` yields the next
//!   burst of raw shell activity, or `None` at end-of-stream. [`TypescriptTail`]
//!   is the Tier A implementation; Tier B implements the same trait over a PTY.
//! - [`ObservationChannel`] is the agent side, built once: every chunk becomes a
//!   `ShellObservation` and is handed to [`TurnDriver::submit_observation`],
//!   which redacts and frames it ([`ShellObservation::into_mem_message`]) before
//!   it can reach the model. **Redaction is enforced by construction** — there
//!   is no path from a raw chunk to the transcript that skips the scrub.
//!
//! # Read-only posture
//!
//! The human's shell is the *human's* shell — `follow` never executes or drives
//! it. The agent itself is clamped too: the channel's [`TurnDriver`] is
//! configured with [`read_only_caveats`], which deny filesystem writes, command
//! execution, and network egress, and forbid tool calls outright. The agent can
//! read the conversation and comment; it cannot act. That is the whole safety
//! story for Tier A, and it carries forward unchanged to Tier B.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use newt_core::agentic::{ShellObservation, TurnDriver, TurnDriverConfig};
use newt_core::{BackendKind, Caveats, CountBound, Scope};

/// The source tag stamped on every Tier-A observation, so the model can see
/// where the shell activity came from. Tier B (#10) uses its own tag against the
/// same channel.
pub const TYPESCRIPT_SOURCE_TAG: &str = "typescript";

/// Caveats that clamp the follow agent to a **read-only observer** posture.
///
/// The agent watching the human's shell must never act on the world: no
/// filesystem writes, no command execution, no network egress, and — because a
/// pure commentator has no business calling tools at all — a zero tool-call
/// budget. Reads stay permissive so the agent can still orient itself.
///
/// This is the attenuation floor for `follow`; it is built on
/// [`Caveats::top`] and narrowed, so any future axis added upstream defaults to
/// the permissive top and must be deliberately clamped here if it grants
/// authority. The human keeps full control of their own shell regardless — these
/// caveats bind the *agent*, not the human.
pub fn read_only_caveats() -> Caveats {
    Caveats {
        // Reading is fine — the agent orients itself from context.
        fs_read: Scope::All,
        // Everything that touches the world is denied.
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        // A commentator makes no tool calls at all.
        max_calls: CountBound::AtMost(0),
        ..Caveats::top()
    }
}

/// A swappable producer of raw shell-activity chunks — **the channel's only
/// input seam**.
///
/// Tier A ([`TypescriptTail`]) reads chunks off a growing `script -F`
/// typescript. Tier B (issue #10) will implement this same trait over a hosted
/// PTY. Because the channel consumes nothing but `next_chunk()`, swapping the
/// producer never touches the agent side.
///
/// A chunk is one burst of new shell activity (it may span several lines). The
/// channel wraps each chunk as a single [`ShellObservation`]; chunking
/// granularity is the source's concern, not the channel's.
pub trait ObservationSource {
    /// The next burst of raw shell activity, or `None` when the stream is
    /// exhausted (the typescript stopped growing and the producer is done).
    ///
    /// Returning `Some("")` is allowed but pointless; the channel treats an
    /// all-whitespace chunk as nothing to observe and skips it.
    fn next_chunk(&mut self) -> Option<String>;

    /// A short, descriptive tag for where this source's activity comes from
    /// (`"typescript"`, `"pty"`, …). Shown to the model verbatim for
    /// orientation.
    fn source_tag(&self) -> &str;
}

/// The shared shell-observation **channel** — the agent side, built once.
///
/// Owns the [`TurnDriver`] (configured read-only) and turns every chunk a
/// [`ObservationSource`] produces into a redacted, framed observation in the
/// agent's context. Tier A and Tier B drive the same `ObservationChannel`; only
/// the source differs.
pub struct ObservationChannel {
    driver: TurnDriver,
}

impl ObservationChannel {
    /// Build a channel around a turn driver. The caller is expected to have
    /// configured the driver with [`read_only_caveats`] (see
    /// [`read_only_config`]); `follow` always does.
    pub fn new(driver: TurnDriver) -> Self {
        Self { driver }
    }

    /// Feed one raw chunk of shell activity into the channel.
    ///
    /// The chunk is wrapped as a [`ShellObservation`] tagged with `source` and
    /// handed to [`TurnDriver::submit_observation`], which redacts credentials
    /// and frames it as a non-instruction observation **before** it enters the
    /// transcript. An all-whitespace chunk carries no activity and is skipped;
    /// the return value says whether an observation was actually submitted.
    ///
    /// Submitting an observation does **not** start a turn — it only adds
    /// context for the *next* human-driven turn (or an explicit
    /// [`drive_comment`]). That is the read-only contract: the agent
    /// accumulates awareness, it does not act on its own.
    pub fn feed(&mut self, source: &str, chunk: impl Into<String>) -> bool {
        let chunk = chunk.into();
        if chunk.trim().is_empty() {
            return false;
        }
        self.driver
            .submit_observation(ShellObservation::new(source.to_string(), chunk));
        true
    }

    /// Convenience: feed a chunk straight off a [`ObservationSource`], using the
    /// source's own [`source_tag`](ObservationSource::source_tag). Returns
    /// whether an observation was submitted.
    pub fn feed_from(&mut self, source: &mut dyn ObservationSource, chunk: String) -> bool {
        let tag = source.source_tag().to_string();
        self.feed(&tag, chunk)
    }

    /// Borrow the underlying driver — for polling completion, submitting the
    /// human's own messages, or rendering the transcript via
    /// [`newt_core::agentic::transcript_lines`].
    pub fn driver(&mut self) -> &mut TurnDriver {
        &mut self.driver
    }

    /// The number of messages accumulated in the transcript so far — observations
    /// plus any turns. Lets a caller see how much context has built up without
    /// reaching through to the driver.
    pub fn observation_count(&mut self) -> usize {
        self.driver.transcript().len()
    }
}

/// Build a [`TurnDriverConfig`] clamped to the read-only follow posture.
///
/// Identical to [`TurnDriverConfig::new`] except the caveats are
/// [`read_only_caveats`] instead of the permissive `top()` default — so a
/// channel built from this config can never act on the world.
pub fn read_only_config(
    url: impl Into<String>,
    model: impl Into<String>,
    kind: BackendKind,
    workspace: impl Into<String>,
) -> TurnDriverConfig {
    let mut config = TurnDriverConfig::new(url, model, kind, workspace);
    config.caveats = read_only_caveats();
    config
}

/// Build a read-only follow [`TurnDriverConfig`] from a newt
/// [`BackendConfig`](newt_core::BackendConfig) — the backend the operator
/// already configured for `newt`/`gila code`.
///
/// Reuses the endpoint, model, wire protocol, and resolved bearer token from the
/// backend, then clamps the caveats to [`read_only_caveats`]. This is how `gila
/// follow` talks to the same inference endpoint the rest of the airframe uses,
/// without the agent gaining any authority to act. `workspace` is the directory
/// the (toolless) turn nominally runs against; for follow it is purely
/// informational.
pub fn config_from_backend(
    backend: &newt_core::BackendConfig,
    workspace: impl Into<String>,
) -> TurnDriverConfig {
    let mut config = read_only_config(
        backend.endpoint.clone(),
        backend.model.clone(),
        backend.kind,
        workspace,
    );
    config.api_key = backend.resolve_api_key();
    config
}

/// The standing instruction `gila follow` gives the agent: it is a read-only
/// observer of the human's shell and should comment briefly and only when it has
/// something useful to add. Submitted as the human-side turn that drives a
/// comment after fresh observations accumulate.
pub const FOLLOW_COMMENT_NUDGE: &str =
    "You are watching my shell read-only. If the latest activity above is worth \
     a brief comment or a heads-up, say so concisely; otherwise reply with just \
     \"(watching)\". Never tell me to run anything — you are an observer.";

/// Outcome of one [`follow_tick`]: what the channel did with the current poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowTick {
    /// New shell activity was observed and folded into context.
    Observed,
    /// The source produced nothing new this tick.
    Idle,
    /// The source is exhausted — stop following.
    Exhausted,
}

/// Pull whatever the source has *right now* and fold it into the channel.
///
/// This is the side-effect-free heart of the follow loop, factored out so it is
/// directly unit-testable without a runtime or a live backend: one
/// [`next_chunk`](ObservationSource::next_chunk), one [`feed`](
/// ObservationChannel::feed). The binary's loop calls this on a cadence,
/// deciding when to drive a comment turn and when to sleep between ticks.
///
/// - [`FollowTick::Observed`] — a non-empty chunk was redacted, framed, and
///   added to context.
/// - [`FollowTick::Idle`] — the source yielded `None` (no growth yet) or only
///   whitespace; nothing was added.
///
/// A source signals end-of-stream by returning `None` from `next_chunk`; this
/// function maps that to `Idle` (a typescript that stopped growing may grow
/// again). The *binary* decides exhaustion from its own stop policy. A mock
/// source in tests can be drained to `Idle` deterministically.
pub fn follow_tick(
    channel: &mut ObservationChannel,
    source: &mut dyn ObservationSource,
) -> FollowTick {
    match source.next_chunk() {
        Some(chunk) => {
            let tag = source.source_tag().to_string();
            if channel.feed(&tag, chunk) {
                FollowTick::Observed
            } else {
                FollowTick::Idle
            }
        }
        None => FollowTick::Idle,
    }
}

/// Tier A producer: tails a growing `script -F` typescript file.
///
/// `script -F` flushes each write immediately, so the typescript grows in step
/// with the human's shell. Each [`next_chunk`](ObservationSource::next_chunk)
/// reads everything appended since the last read, strips the one-line
/// `Script started …` banner the first time, and returns it as one observation
/// chunk. When the file has not grown, `next_chunk` returns an empty-trimming
/// chunk the channel skips; the caller decides when to stop polling.
///
/// The tail is *position-based* (it remembers the byte offset it last read to),
/// so it survives across polls without re-reading the whole file. It is a pure
/// reader — it never writes to or truncates the typescript, honoring the
/// read-only contract end to end.
pub struct TypescriptTail {
    path: PathBuf,
    offset: u64,
    header_stripped: bool,
}

impl TypescriptTail {
    /// Tail the typescript at `path` from the beginning.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            header_stripped: false,
        }
    }

    /// The typescript path being tailed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read everything appended since the last read, returning the new bytes as
    /// a UTF-8 (lossy) string. `Ok(None)` means the file has not grown (or does
    /// not exist yet); `Ok(Some(_))` is a fresh chunk. Errors surface real I/O
    /// problems (permission denied, etc.).
    ///
    /// This is the fallible core; [`next_chunk`](ObservationSource::next_chunk)
    /// wraps it and swallows the not-yet-grown / transient-error cases into
    /// `None` so a polling loop stays simple.
    pub fn read_new(&mut self) -> std::io::Result<Option<String>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len <= self.offset {
            // No growth (or the file was truncated/rotated — we don't chase
            // rotations in Tier A; a fresh `gila follow` picks up the new file).
            return Ok(None);
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        file.read_to_end(&mut buf)?;
        self.offset = len;

        let mut text = String::from_utf8_lossy(&buf).into_owned();
        if !self.header_stripped {
            text = strip_script_header(&text);
            self.header_stripped = true;
        }
        Ok(Some(text))
    }
}

impl ObservationSource for TypescriptTail {
    fn next_chunk(&mut self) -> Option<String> {
        // Transient I/O errors and "not grown yet" both collapse to None — the
        // caller's polling loop treats them identically (nothing to observe
        // right now).
        self.read_new().ok().flatten()
    }

    fn source_tag(&self) -> &str {
        TYPESCRIPT_SOURCE_TAG
    }
}

/// Strip the leading `Script started on …` banner `script(1)` writes as the
/// first line of a typescript, so it never becomes an observation. Only the very
/// first line is considered; everything after the first newline is shell
/// activity and is preserved verbatim. If the text does not start with the
/// banner, it is returned unchanged.
fn strip_script_header(text: &str) -> String {
    if text.starts_with("Script started on") {
        match text.split_once('\n') {
            Some((_, rest)) => rest.to_string(),
            // Header line with no newline yet — nothing of substance follows.
            None => String::new(),
        }
    } else {
        text.to_string()
    }
}

/// Locate the typescript to follow.
///
/// - `explicit` — if given, that exact path (it need not exist yet; the tail
///   waits for `script -F` to create it).
/// - otherwise — the most-recently-modified regular file directly inside `dir`,
///   which is where a `script -F` session typically drops its typescript.
///
/// Returns `None` only when no explicit path was given and `dir` holds no
/// candidate file (empty, unreadable, or only subdirectories).
pub fn locate_typescript(explicit: Option<&Path>, dir: &Path) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let replace = match &newest {
            Some((best, _)) => mtime > *best,
            None => true,
        };
        if replace {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, p)| p)
}

/// Drive the channel to produce one agent **comment** about the activity
/// observed so far.
///
/// Submits [`FOLLOW_COMMENT_NUDGE`] as the human-side turn (the agent never acts
/// on its own — a comment is always in response to a human-side prompt) and
/// pumps the read-only [`TurnDriver`] to completion, returning the agent's
/// reply. This is the only place `follow` starts a turn; observations alone
/// never do.
///
/// `poll_interval` is how long to sleep between non-blocking polls; `max_polls`
/// bounds the wait so a wedged backend can't hang the follow loop. Returns
/// `Ok(None)` if no comment was produced within the budget (the caller simply
/// keeps watching).
pub async fn drive_comment(
    channel: &mut ObservationChannel,
    poll_interval: std::time::Duration,
    max_polls: usize,
) -> Result<Option<String>, newt_core::agentic::TurnDriverError> {
    use newt_core::agentic::TurnStatus;

    channel.driver().submit(FOLLOW_COMMENT_NUDGE)?;
    for _ in 0..max_polls {
        match channel.driver().poll() {
            TurnStatus::Completed(outcome) => return Ok(Some(outcome.reply)),
            TurnStatus::Failed(_) => return Ok(None),
            TurnStatus::Running => tokio::time::sleep(poll_interval).await,
            TurnStatus::Idle => return Ok(None),
        }
    }
    // Budget exhausted with the turn still running — abandon it cleanly so the
    // driver is free for the next comment.
    channel.driver().cancel();
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::agentic::SHELL_OBSERVATION_PREFIX;
    use newt_core::{CaveatsExt, CountBoundExt};
    use std::io::Write;

    fn test_config() -> TurnDriverConfig {
        read_only_config("http://127.0.0.1:1", "test-model", BackendKind::Ollama, ".")
    }

    fn test_channel() -> ObservationChannel {
        ObservationChannel::new(TurnDriver::new(test_config()))
    }

    // --- read-only posture --------------------------------------------------

    #[test]
    fn read_only_caveats_deny_writes_exec_and_net() {
        let c = read_only_caveats();
        // Reads are permitted (the agent orients itself).
        assert!(c.permits_fs_read("/anywhere"));
        // Everything that touches the world is denied.
        assert!(!c.permits_fs_write("/tmp/anything"));
        assert!(!c.permits_exec("rm"));
        assert!(!c.permits_exec("git"));
        assert!(!c.permits_net("example.com"));
        // A pure commentator gets no tool-call budget at all.
        assert!(!c.max_calls.permits_one_more(0));
    }

    #[test]
    fn read_only_config_carries_the_clamp() {
        let cfg = test_config();
        assert!(!cfg.caveats.permits_fs_write("/etc/passwd"));
        assert!(!cfg.caveats.permits_exec("sh"));
        assert!(!cfg.caveats.permits_net("evil.example.com"));
        assert!(cfg.caveats.permits_fs_read("/proc/self/status"));
    }

    // --- channel: redaction-by-construction ---------------------------------

    /// THE load-bearing security test: a secret-shaped chunk is scrubbed before
    /// it can land in the transcript. The channel never sees a path that skips
    /// redaction — it goes through `ShellObservation`, which redacts by
    /// construction.
    #[test]
    fn channel_redacts_a_secret_chunk_before_it_reaches_the_transcript() {
        let mut ch = test_channel();
        let secret = "secret_key=wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY";
        let submitted = ch.feed("bash", format!("$ cat creds\n{secret}\n"));
        assert!(submitted, "a non-empty chunk must be submitted");

        let transcript = ch.driver().transcript();
        assert_eq!(transcript.len(), 1, "one observation message accumulated");
        let body = &transcript[0].content;
        // Framed as an observation, redacted, and the secret value is gone.
        assert!(body.starts_with(SHELL_OBSERVATION_PREFIX));
        assert!(body.contains("[REDACTED]"), "redaction must fire: {body}");
        assert!(
            !body.contains("wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY"),
            "secret leaked into transcript: {body}"
        );
    }

    /// A typescript-shaped chunk becomes a `ShellObservation` in context: framed,
    /// tagged with its source, body preserved. Submitting an observation does NOT
    /// start a turn (read-only: the agent accumulates, it does not act).
    #[test]
    fn typescript_chunk_becomes_a_framed_observation() {
        let mut ch = test_channel();
        let submitted = ch.feed(
            TYPESCRIPT_SOURCE_TAG,
            "$ cargo test\n   Compiling gilamonster-agent\n",
        );
        assert!(submitted);
        assert!(
            !ch.driver().is_running(),
            "an observation must not start a turn"
        );

        let body = ch.driver().transcript()[0].content.clone();
        assert!(body.starts_with(SHELL_OBSERVATION_PREFIX));
        assert!(body.contains("NOT an instruction"));
        assert!(body.contains("NOT a tool result"));
        assert!(body.contains(&format!("source: {TYPESCRIPT_SOURCE_TAG}")));
        assert!(body.contains("cargo test"));
        assert!(body.contains("Compiling gilamonster-agent"));
    }

    #[test]
    fn whitespace_only_chunk_is_skipped() {
        let mut ch = test_channel();
        assert!(!ch.feed("bash", "   \n\t\n"));
        assert!(!ch.feed("bash", ""));
        assert_eq!(ch.observation_count(), 0, "nothing submitted for blanks");
    }

    #[test]
    fn benign_chatter_passes_through_unredacted() {
        let mut ch = test_channel();
        ch.feed("bash", "$ echo 'the api key is in the vault'\n");
        let body = &ch.driver().transcript()[0].content;
        assert!(!body.contains("[REDACTED]"), "{body}");
        assert!(body.contains("the api key is in the vault"));
    }

    // --- the source seam is swappable (mock source) -------------------------

    /// A mock source proves the producer seam: a non-typescript producer feeds
    /// the SAME channel and lands a framed observation. This is exactly how Tier
    /// B (#10) plugs a PTY in without touching the channel/agent side.
    struct MockSource {
        chunks: std::collections::VecDeque<String>,
        tag: String,
    }

    impl MockSource {
        fn new(tag: &str, chunks: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                chunks: chunks.into_iter().map(str::to_string).collect(),
                tag: tag.to_string(),
            }
        }
    }

    impl ObservationSource for MockSource {
        fn next_chunk(&mut self) -> Option<String> {
            self.chunks.pop_front()
        }
        fn source_tag(&self) -> &str {
            &self.tag
        }
    }

    #[test]
    fn mock_source_drives_the_same_channel() {
        let mut ch = test_channel();
        let mut src = MockSource::new("pty", ["$ whoami\nhuman\n", "$ pwd\n/home/human\n"]);

        let mut submitted = 0usize;
        while let Some(chunk) = src.next_chunk() {
            if ch.feed_from(&mut src, chunk) {
                submitted += 1;
            }
        }
        assert_eq!(submitted, 2, "both mock chunks reached the channel");
        let transcript = ch.driver().transcript();
        assert_eq!(transcript.len(), 2);
        // Tagged with the MOCK source's tag, not the typescript one — proof the
        // channel is source-agnostic.
        assert!(transcript[0].content.contains("source: pty"));
        assert!(transcript[0].content.contains("whoami"));
        assert!(transcript[1].content.contains("pwd"));
    }

    // --- follow_tick (the loop's testable heart) ----------------------------

    #[test]
    fn follow_tick_observes_then_goes_idle_when_drained() {
        let mut ch = test_channel();
        let mut src = MockSource::new("pty", ["$ uname -a\nLinux\n"]);

        // First tick: there is a chunk → Observed.
        assert_eq!(follow_tick(&mut ch, &mut src), FollowTick::Observed);
        assert_eq!(ch.observation_count(), 1);
        // Second tick: source drained → Idle, nothing added.
        assert_eq!(follow_tick(&mut ch, &mut src), FollowTick::Idle);
        assert_eq!(ch.observation_count(), 1);
    }

    #[test]
    fn follow_tick_idle_on_whitespace_chunk() {
        let mut ch = test_channel();
        // A source that yields a whitespace-only chunk then drains.
        let mut src = MockSource::new("pty", ["   \n\t\n"]);
        assert_eq!(follow_tick(&mut ch, &mut src), FollowTick::Idle);
        assert_eq!(ch.observation_count(), 0);
    }

    // --- config_from_backend ------------------------------------------------

    #[test]
    fn config_from_backend_clamps_read_only_and_reuses_endpoint() {
        let backend = newt_core::BackendConfig {
            name: "local".into(),
            endpoint: "http://10.0.0.5:11434".into(),
            model: "qwen2.5-coder".into(),
            tiers: vec![],
            kind: BackendKind::Ollama,
            api_key_file: None,
            api_key_env: None,
        };
        let cfg = config_from_backend(&backend, "/work");
        // Endpoint/model/kind threaded through.
        assert_eq!(cfg.url, "http://10.0.0.5:11434");
        assert_eq!(cfg.model, "qwen2.5-coder");
        assert_eq!(cfg.kind, BackendKind::Ollama);
        assert_eq!(cfg.workspace, "/work");
        assert!(cfg.api_key.is_none());
        // And it is read-only.
        assert!(!cfg.caveats.permits_fs_write("/work/x"));
        assert!(!cfg.caveats.permits_exec("ls"));
    }

    // --- drive_comment against a mock backend -------------------------------

    /// THE end-to-end channel test: an observation feeds the read-only driver,
    /// and `drive_comment` pumps a real (mocked) turn so the agent's comment
    /// comes back — proving the observation reached the turn driver and a secret
    /// in it never reached the model.
    #[tokio::test]
    async fn drive_comment_returns_the_agents_reply_and_redacts_context() {
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(String::new()));
        struct Capture {
            seen: Arc<Mutex<String>>,
        }
        impl Respond for Capture {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                *self.seen.lock().unwrap() = String::from_utf8_lossy(&req.body).into_owned();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "(watching) nice, the build is green" }
                }))
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(Capture { seen: seen.clone() })
            .mount(&server)
            .await;

        let cfg = read_only_config(server.uri(), "test-model", BackendKind::Ollama, ".");
        let mut ch = ObservationChannel::new(TurnDriver::new(cfg));

        // Observe a chunk carrying a secret.
        ch.feed(
            TYPESCRIPT_SOURCE_TAG,
            "$ cat .env\nsecret_key=wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY\n$ cargo test\nok\n",
        );

        let reply = drive_comment(&mut ch, std::time::Duration::from_millis(10), 600)
            .await
            .expect("drive ok")
            .expect("a reply within budget");
        assert!(reply.contains("watching"));

        // The model saw the observation framing + the benign activity, but NOT
        // the secret value.
        let body = seen.lock().unwrap().clone();
        assert!(body.contains("shell observation"), "{body}");
        assert!(body.contains("cargo test"), "{body}");
        assert!(
            !body.contains("wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY"),
            "secret leaked to the model: {body}"
        );
    }

    #[tokio::test]
    async fn drive_comment_returns_none_when_backend_fails() {
        // Point at a dead port: the turn fails fast → Ok(None), loop keeps going.
        let cfg = read_only_config("http://127.0.0.1:1", "m", BackendKind::Ollama, ".");
        let mut ch = ObservationChannel::new(TurnDriver::new(cfg));
        ch.feed(TYPESCRIPT_SOURCE_TAG, "$ echo hi\nhi\n");
        let got = drive_comment(&mut ch, std::time::Duration::from_millis(5), 600)
            .await
            .expect("no driver error");
        assert!(got.is_none(), "a failed turn yields no comment");
    }

    // --- TypescriptTail (Tier A producer) -----------------------------------

    fn write_typescript(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn tail_strips_the_script_header_on_first_read() {
        let f = write_typescript("Script started on 2026-06-12 10:00:00\n$ ls\nfoo.txt\nbar.txt\n");
        let mut tail = TypescriptTail::new(f.path());
        let chunk = tail.next_chunk().expect("first chunk");
        assert!(
            !chunk.contains("Script started on"),
            "banner must be stripped: {chunk}"
        );
        assert!(chunk.contains("$ ls"));
        assert!(chunk.contains("foo.txt"));
        assert_eq!(tail.source_tag(), TYPESCRIPT_SOURCE_TAG);
        assert_eq!(tail.path(), f.path());
    }

    #[test]
    fn tail_reads_only_appended_bytes_on_subsequent_reads() {
        let mut f = write_typescript("Script started on now\n$ first\n");
        let mut tail = TypescriptTail::new(f.path());

        let first = tail.next_chunk().expect("first chunk");
        assert!(first.contains("$ first"));
        // Nothing new yet.
        assert!(tail.next_chunk().is_none(), "no growth → no chunk");

        // The human runs another command; `script -F` appends.
        f.write_all(b"$ second\noutput-2\n").unwrap();
        f.flush().unwrap();

        let second = tail.next_chunk().expect("second chunk");
        assert!(second.contains("$ second"));
        assert!(second.contains("output-2"));
        // The header is NOT re-stripped, and the first command is NOT repeated.
        assert!(!second.contains("$ first"));
        assert!(!second.contains("Script started"));
    }

    #[test]
    fn tail_of_missing_file_yields_nothing() {
        let mut tail = TypescriptTail::new("/nonexistent/typescript/path/xyz");
        assert!(tail.next_chunk().is_none());
        // read_new surfaces NotFound as Ok(None), not an error.
        assert!(matches!(tail.read_new(), Ok(None)));
    }

    #[test]
    fn header_only_typescript_strips_to_empty() {
        // A typescript that is just the banner with no trailing newline yet.
        let f = write_typescript("Script started on 2026-06-12 10:00:00");
        let mut tail = TypescriptTail::new(f.path());
        // The lone banner strips to empty → trimmed-empty → skipped by the
        // channel; next_chunk still returns Some("") here (the channel filters).
        let chunk = tail.next_chunk().expect("a chunk (possibly empty)");
        assert!(
            chunk.trim().is_empty(),
            "banner-only strips to empty: {chunk:?}"
        );
    }

    #[test]
    fn tail_feeds_the_channel_end_to_end() {
        // Tier A wired end-to-end: typescript → tail → channel → transcript.
        let f = write_typescript("Script started on now\n$ echo hi\nhi\n");
        let mut tail = TypescriptTail::new(f.path());
        let mut ch = test_channel();

        let chunk = tail.next_chunk().expect("chunk");
        assert!(ch.feed_from(&mut tail, chunk));
        let body = &ch.driver().transcript()[0].content;
        assert!(body.contains("source: typescript"));
        assert!(body.contains("echo hi"));
    }

    // --- locate_typescript --------------------------------------------------

    #[test]
    fn locate_prefers_explicit_path_even_if_absent() {
        let explicit = Path::new("/some/explicit/typescript");
        let dir = tempfile::tempdir().unwrap();
        let got = locate_typescript(Some(explicit), dir.path()).unwrap();
        assert_eq!(got, explicit);
    }

    #[test]
    fn locate_picks_newest_file_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.typescript");
        let new = dir.path().join("new.typescript");
        std::fs::write(&old, b"old").unwrap();
        // Ensure a distinct, later mtime for `new`.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::write(&new, b"new").unwrap();
        filetime_set(&new, later);

        let got = locate_typescript(None, dir.path()).unwrap();
        assert_eq!(got, new);
    }

    #[test]
    fn locate_returns_none_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(locate_typescript(None, dir.path()).is_none());
    }

    #[test]
    fn locate_returns_none_for_missing_dir() {
        assert!(locate_typescript(None, Path::new("/no/such/dir/here")).is_none());
    }

    #[test]
    fn locate_skips_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        // Only a subdir, no files → None.
        assert!(locate_typescript(None, dir.path()).is_none());
        // Add a real file; now it's found.
        std::fs::write(dir.path().join("real.typescript"), b"x").unwrap();
        assert_eq!(
            locate_typescript(None, dir.path()).unwrap(),
            dir.path().join("real.typescript")
        );
    }

    /// Set a file's mtime without pulling in the `filetime` crate — uses a second
    /// write plus an explicit `set_modified` via `File::set_times` (stable since
    /// 1.75). Keeps the locate "newest wins" test deterministic.
    fn filetime_set(path: &Path, when: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }
}
