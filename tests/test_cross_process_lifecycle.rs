#![cfg(feature = "jupyter-test-fixture")]

//! Black-box durable Jupyter lifecycle coverage.
//!
//! Every operation is a separate `gila` process sharing only an isolated
//! `GILA_HOME`. The fixture is copied into PATH as `jupyter`, so these tests hit
//! production command construction, durable logs, endpoint parsing, HTTP
//! probes, registry transactions, and process-tree rollback.

use gilamonster_agent::gila_jupyter::RegistryFile;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    _home: TempDir,
    work: TempDir,
    gila_home: TempDir,
    gila: PathBuf,
    fixture: PathBuf,
    path: OsString,
}

impl Harness {
    fn new() -> Self {
        let home = TempDir::new().expect("temporary HOME");
        let work = TempDir::new().expect("temporary workdir");
        let gila_home = TempDir::new().expect("temporary GILA_HOME");
        let fixture = PathBuf::from(env!("CARGO_BIN_EXE_gila-jupyter-fixture"));
        let bin_dir = work.path().join("bin");
        fs::create_dir(&bin_dir).expect("fixture bin directory");
        #[cfg(unix)]
        let installed_fixture = bin_dir.join("jupyter");
        #[cfg(windows)]
        let installed_fixture = bin_dir.join("jupyter.exe");
        fs::copy(&fixture, &installed_fixture).expect("copy Jupyter fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&installed_fixture, fs::Permissions::from_mode(0o755))
                .expect("fixture executable mode");
        }

        let mut paths = vec![bin_dir];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        let path = std::env::join_paths(paths).expect("joined fixture PATH");
        Self {
            _home: home,
            work,
            gila_home,
            gila: PathBuf::from(env!("CARGO_BIN_EXE_gila")),
            fixture,
            path,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.gila);
        command
            .env("HOME", self._home.path())
            .env("GILA_HOME", self.gila_home.path())
            .env("PATH", &self.path)
            .env("GILA_AUTH_TOKEN", "must-not-reach-child")
            .env("GILA_OPERATOR_KEY", "must-not-reach-child")
            .env("NEWT_API_KEY", "must-not-reach-child")
            .env("OPENAI_API_KEY", "must-not-reach-child")
            // Lifecycle requests must bypass hostile ambient proxy state and
            // connect directly to the validated loopback endpoint.
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .env("http_proxy", "http://127.0.0.1:9")
            .env("https_proxy", "http://127.0.0.1:9")
            .env("all_proxy", "http://127.0.0.1:9")
            .env("no_proxy", "")
            .current_dir(self.work.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("run gila subprocess")
    }

    fn registry_path(&self) -> PathBuf {
        self.gila_home.path().join("servers.json")
    }

    fn registry(&self) -> RegistryFile {
        let bytes = fs::read(self.registry_path()).expect("read schema-v1 registry");
        serde_json::from_slice(&bytes).expect("parse schema-v1 registry")
    }

    fn write_registry(&self, registry: &RegistryFile) {
        let encoded = serde_json::to_vec_pretty(registry).expect("encode schema-v1 registry");
        fs::write(self.registry_path(), encoded).expect("write schema-v1 registry");
    }

    fn mode_path(&self) -> PathBuf {
        self.work.path().join(".fixture-mode")
    }
}

struct ChildCleanup(Child);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct ServerCleanup<'a> {
    harness: &'a Harness,
    handle: u64,
    active: bool,
}

impl ServerCleanup<'_> {
    fn stopped(&mut self) {
        self.active = false;
    }
}

impl Drop for ServerCleanup<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_file(self.harness.mode_path());
            let _ = self
                .harness
                .run(&["jupyter", "stop", &self.handle.to_string()]);
        }
    }
}

struct RegistryRestoringCleanup<'a> {
    harness: &'a Harness,
    handle: u64,
    original: RegistryFile,
    active: bool,
}

impl RegistryRestoringCleanup<'_> {
    fn stopped(&mut self) {
        self.active = false;
    }
}

impl Drop for RegistryRestoringCleanup<'_> {
    fn drop(&mut self) {
        if self.active {
            self.harness.write_registry(&self.original);
            let _ = self
                .harness
                .run(&["jupyter", "stop", &self.handle.to_string()]);
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free loopback port")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn parse_handle(output: &Output) -> u64 {
    stdout(output)
        .split("handle ")
        .nth(1)
        .and_then(|tail| tail.split_ascii_whitespace().next())
        .and_then(|encoded| encoded.parse().ok())
        .expect("start output handle")
}

fn assert_record_present(harness: &Harness, handle: u64, instance_id: [u8; 16]) {
    let registry = harness.registry();
    let current = registry.servers.get(&handle).expect("registered handle");
    assert_eq!(current.instance_id, instance_id);
}

fn assert_invalid_record_preserved(
    harness: &Harness,
    handle: u64,
    expected_registry: &RegistryFile,
    diagnostic: &str,
) {
    harness.write_registry(expected_registry);

    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(
        status.status.success(),
        "invalid-record status failed: {}",
        stderr(&status)
    );
    assert!(stdout(&status).contains("unreachable"));
    assert!(
        stdout(&status).contains(diagnostic),
        "missing {diagnostic:?} in status: {}",
        stdout(&status)
    );

    let list = harness.run(&["jupyter", "list"]);
    assert!(
        list.status.success(),
        "invalid-record list failed: {}",
        stderr(&list)
    );
    assert!(stdout(&list).contains(&format!("handle {handle}: unreachable")));

    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!stop.status.success(), "invalid-record stop exited zero");
    assert_eq!(
        &harness.registry(),
        expected_registry,
        "status/list/stop mutated an invalid registry entry"
    );
}

fn spawn_unrelated_listener(harness: &Harness, port: u16) -> ChildCleanup {
    let ready = harness.work.path().join(".fixture-unrelated-ready");
    let child = Command::new(&harness.fixture)
        .arg("--fixture-listener-only")
        .arg("--port")
        .arg(port.to_string())
        .arg("--fixture-ready-file")
        .arg(&ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn unrelated listener fixture");
    let mut cleanup = ChildCleanup(child);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(
            cleanup.0.try_wait().unwrap().is_none(),
            "unrelated listener exited before ready"
        );
        assert!(Instant::now() < deadline, "unrelated listener not ready");
        thread::sleep(Duration::from_millis(20));
    }
    cleanup
}

fn wait_for_text(path: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if fs::read_to_string(path).is_ok_and(|contents| contents.contains(needle)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?} never appeared in log"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn assert_listener_refused(port: u16) {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => return,
            _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            result => panic!("listener {port} did not become refused: {result:?}"),
        }
    }
}

fn fixture_shutdown(port: u16, token: &str) {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("fixture connection");
    write!(
        stream,
        "POST /api/shutdown HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: token {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .expect("fixture shutdown request");
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 204"), "{response}");
}

#[test]
fn separate_cli_lifecycle_uses_schema_v1_registry_and_durable_log() {
    let harness = Harness::new();
    let port = free_port();
    let token = "fixture + punctuation/%!";
    let extra = "--fixture-base-path /user/test/lab --fixture-spoof-candidates --ServerApp.default_url /voila --ip 0.0.0.0 --port 1 --NotebookApp.token attacker";
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        token,
        &format!("--extra={extra}"),
    ]);
    assert!(start.status.success(), "start failed: {}", stderr(&start));
    let handle = parse_handle(&start);
    let mut cleanup = ServerCleanup {
        harness: &harness,
        handle,
        active: true,
    };

    let registry = harness.registry();
    assert_eq!(registry.schema_version, 1);
    assert!(registry.next_handle > handle);
    let record = registry.servers.get(&handle).expect("server record");
    assert_eq!(record.port, port);
    assert_eq!(record.token, token);
    assert_eq!(record.url, format!("http://127.0.0.1:{port}/user/test/lab"));
    let log_path = record.log_path.as_deref().expect("durable log_path");
    assert!(log_path.starts_with("jupyter/logs/start-"));
    let absolute_log = harness.gila_home.path().join(log_path);
    assert!(absolute_log.is_file());
    wait_for_text(&absolute_log, "fixture heartbeat");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&absolute_log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let environment = fs::read_to_string(harness.work.path().join(".fixture-env"))
        .expect("fixture environment capture");
    for secret in [
        "GILA_HOME=",
        "GILA_AUTH_TOKEN=",
        "GILA_OPERATOR_KEY=",
        "NEWT_API_KEY=",
        "OPENAI_API_KEY=",
    ] {
        assert!(
            !environment.contains(secret),
            "child environment leaked {secret}"
        );
    }
    for required in ["PATH=", "HOME="] {
        assert!(environment.contains(required), "child missed {required}");
    }

    let argv = fs::read_to_string(harness.work.path().join(".fixture-argv"))
        .expect("fixture argv capture");
    let args: Vec<_> = argv.lines().collect();
    let final_ip = args.iter().rposition(|arg| *arg == "--ip").unwrap();
    let final_port = args.iter().rposition(|arg| *arg == "--port").unwrap();
    let legacy_token = args
        .iter()
        .rposition(|arg| *arg == "--NotebookApp.token")
        .unwrap();
    let final_token = args
        .iter()
        .rposition(|arg| *arg == "--IdentityProvider.token")
        .unwrap();
    let final_default_url = args
        .iter()
        .rposition(|arg| *arg == "--ServerApp.default_url")
        .unwrap();
    assert_eq!(args[final_ip + 1], "127.0.0.1");
    assert_eq!(args[final_port + 1], port.to_string());
    assert_eq!(args[legacy_token + 1], token);
    assert_eq!(args[final_token + 1], token);
    assert_eq!(args[final_default_url + 1], "/voila");
    assert!(final_token > legacy_token);

    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(
        status.status.success(),
        "status failed: {}",
        stderr(&status)
    );
    assert!(stdout(&status).contains("running"));
    assert!(stdout(&status).contains("/user/test/lab"));
    let list = harness.run(&["jupyter", "list"]);
    assert!(list.status.success(), "list failed: {}", stderr(&list));
    assert!(stdout(&list).contains("registered server"));
    assert!(stdout(&list).contains(&format!("handle {handle}: running")));

    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
    assert!(stdout(&stop).contains("stopped"));
    cleanup.stopped();
    assert_listener_refused(port);
    assert!(!harness.registry().servers.contains_key(&handle));

    let second_stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(second_stop.status.success());
    assert!(stdout(&second_stop).contains("not running"));
}

#[test]
fn persistence_failure_kills_listener_and_descendant_tree() {
    let harness = Harness::new();
    let port = free_port();
    let descendant_port = free_port();
    let broken_registry = harness.registry_path();
    let extra = format!(
        "--fixture-break-registry {} --fixture-descendant-port {descendant_port}",
        broken_registry.display()
    );
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "rollback-token",
        &format!("--extra={extra}"),
    ]);
    assert!(!start.status.success(), "persistence failure exited zero");
    assert!(
        stderr(&start).contains("registry") || stderr(&start).contains("persist"),
        "unexpected error: {}",
        stderr(&start)
    );
    assert!(
        harness
            .work
            .path()
            .join(".fixture-descendant-ready")
            .exists(),
        "descendant never became live before rollback"
    );
    assert_listener_refused(port);
    assert_listener_refused(descendant_port);
}

#[test]
fn definitely_refused_listener_is_cas_cleaned_as_already_stopped() {
    let harness = Harness::new();
    let port = free_port();
    let token = "already-stopped-token";
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        token,
    ]);
    assert!(start.status.success(), "start failed: {}", stderr(&start));
    let handle = parse_handle(&start);
    let instance_id = harness.registry().servers[&handle].instance_id;
    let mut cleanup = ServerCleanup {
        harness: &harness,
        handle,
        active: true,
    };

    fixture_shutdown(port, token);
    assert_listener_refused(port);
    assert_record_present(&harness, handle, instance_id);
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(
        stop.status.success(),
        "stale cleanup failed: {}",
        stderr(&stop)
    );
    assert!(stdout(&stop).contains("not running"));
    cleanup.stopped();
    assert!(!harness.registry().servers.contains_key(&handle));
}

#[test]
fn tampered_records_are_preserved_and_pid_remains_informational() {
    let harness = Harness::new();
    let port = free_port();
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "original-token",
    ]);
    assert!(start.status.success(), "start failed: {}", stderr(&start));
    let handle = parse_handle(&start);
    let original = harness.registry();
    let original_record = original.servers[&handle].clone();
    let mut cleanup = RegistryRestoringCleanup {
        harness: &harness,
        handle,
        original: original.clone(),
        active: true,
    };

    let mut non_loopback = original.clone();
    non_loopback.servers.get_mut(&handle).unwrap().url = format!("http://203.0.113.9:{port}");
    assert_invalid_record_preserved(&harness, handle, &non_loopback, "not loopback");

    let mut port_mismatch = original.clone();
    port_mismatch.servers.get_mut(&handle).unwrap().port =
        if port == u16::MAX { port - 1 } else { port + 1 };
    assert_invalid_record_preserved(&harness, handle, &port_mismatch, "does not match");

    let mut empty_token = original.clone();
    empty_token.servers.get_mut(&handle).unwrap().token.clear();
    assert_invalid_record_preserved(&harness, handle, &empty_token, "token is empty");

    let mut mismatched_token = original.clone();
    mismatched_token.servers.get_mut(&handle).unwrap().token = "attacker-token".to_string();
    assert_invalid_record_preserved(&harness, handle, &mismatched_token, "403");

    let unrelated_port = free_port();
    let mut unrelated = spawn_unrelated_listener(&harness, unrelated_port);
    let mut informational_pid = original.clone();
    let record = informational_pid.servers.get_mut(&handle).unwrap();
    *record = original_record;
    record.pid = Some(unrelated.0.id());
    harness.write_registry(&informational_pid);

    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "stop trusted and killed the unrelated informational PID"
    );
    cleanup.stopped();
    assert_listener_refused(port);
    assert!(!harness.registry().servers.contains_key(&handle));
}

#[test]
fn transient_and_forbidden_failures_preserve_exact_record() {
    let harness = Harness::new();
    let port = free_port();
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "preserve-token",
        "--password",
        "fixture-password",
    ]);
    assert!(start.status.success(), "start failed: {}", stderr(&start));
    let handle = parse_handle(&start);
    let instance_id = harness.registry().servers[&handle].instance_id;
    let argv = fs::read_to_string(harness.work.path().join(".fixture-argv")).unwrap();
    assert!(argv.lines().any(|arg| arg == "--NotebookApp.password"));
    assert!(argv
        .lines()
        .any(|arg| arg == "--PasswordIdentityProvider.hashed_password"));
    assert!(argv.lines().any(|arg| arg == "--IdentityProvider.token"));
    let mut cleanup = ServerCleanup {
        harness: &harness,
        handle,
        active: true,
    };

    fs::write(harness.mode_path(), "kernels-503").unwrap();
    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(status.status.success());
    assert!(stdout(&status).contains("unreachable"));
    assert!(stdout(&status).contains("503"));
    let list = harness.run(&["jupyter", "list"]);
    assert!(list.status.success());
    assert!(stdout(&list).contains(&format!("handle {handle}: unreachable")));
    assert_record_present(&harness, handle, instance_id);

    fs::write(harness.mode_path(), "kernels-403").unwrap();
    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(status.status.success());
    assert!(stdout(&status).contains("403"));
    let list = harness.run(&["jupyter", "list"]);
    assert!(list.status.success());
    assert!(stdout(&list).contains(&format!("handle {handle}: unreachable")));
    assert!(stdout(&list).contains("403"));
    assert_record_present(&harness, handle, instance_id);

    fs::write(harness.mode_path(), "kernels-malformed").unwrap();
    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(status.status.success());
    assert!(stdout(&status).contains("malformed JSON"));
    assert_record_present(&harness, handle, instance_id);

    fs::write(harness.mode_path(), "shutdown-403").unwrap();
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!stop.status.success(), "403 shutdown exited zero");
    assert!(stderr(&stop).contains("403"));
    assert_record_present(&harness, handle, instance_id);

    fs::write(harness.mode_path(), "shutdown-503").unwrap();
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!stop.status.success(), "503 shutdown exited zero");
    assert!(stderr(&stop).contains("503"));
    assert_record_present(&harness, handle, instance_id);

    fs::write(harness.mode_path(), "shutdown-no-exit").unwrap();
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!stop.status.success(), "ambiguous shutdown exited zero");
    assert!(stderr(&stop).contains("not confirmed"));
    assert_record_present(&harness, handle, instance_id);

    fs::remove_file(harness.mode_path()).unwrap();
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(
        stop.status.success(),
        "cleanup stop failed: {}",
        stderr(&stop)
    );
    cleanup.stopped();
    assert!(!harness.registry().servers.contains_key(&handle));
}

#[test]
fn semantic_start_failure_is_nonzero_and_redacts_token() {
    let harness = Harness::new();
    let port = free_port();
    let token = "do-not-print-this-token";
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        token,
        "--extra=--fixture-invalid-only",
    ]);
    assert!(!start.status.success());
    assert!(stderr(&start).contains("jupyter server failed"));
    assert!(!stderr(&start).contains(token));
    if harness.registry_path().exists() {
        assert!(harness.registry().servers.is_empty());
    }
    assert_listener_refused(port);
}

#[test]
fn malformed_kernels_readiness_is_rolled_back_without_registration() {
    let harness = Harness::new();
    let port = free_port();
    fs::write(harness.mode_path(), "kernels-malformed-exit").unwrap();
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "malformed-readiness-token",
    ]);
    assert!(!start.status.success(), "malformed readiness exited zero");
    assert!(
        stderr(&start).contains("jupyter server failed"),
        "unexpected readiness failure: {}",
        stderr(&start)
    );
    if harness.registry_path().exists() {
        assert!(harness.registry().servers.is_empty());
    }
    assert_listener_refused(port);
}
