#![cfg(feature = "jupyter-test-fixture")]

//! Black-box durable Jupyter lifecycle coverage.
//!
//! Every operation is a separate `gila` process sharing only an isolated
//! `GILA_HOME`. The fixture is copied into PATH as `jupyter`, so these tests hit
//! production command construction, durable logs, endpoint parsing, HTTP
//! probes, registry transactions, and process-tree rollback.

use gilamonster_agent::gila_jupyter::RegistryFile;
use std::collections::BTreeMap;
#[cfg(windows)]
use std::collections::BTreeSet;
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
            .env("PYTHONPATH", "must-not-reach-child")
            .env("PYTHONHOME", "must-not-reach-child")
            .env("PIP_INDEX_URL", "https://credential.invalid/simple")
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
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            // Prove production and fixture environment handling never falls
            // back to the panicking Unicode-only `vars()` API.
            command.env("TERM", OsString::from_wide(&[b'g' as u16, 0xd800]));
        }
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

fn read_port_marker(path: &Path) -> u16 {
    fs::read_to_string(path)
        .expect("read fixture port marker")
        .trim()
        .parse()
        .expect("numeric fixture port marker")
}

fn instance_hex(instance_id: [u8; 16]) -> String {
    instance_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn percent_encoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
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
    wait_for_text_for(path, needle, Duration::from_secs(3));
}

fn wait_for_text_for(path: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
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

fn assert_listener_port_reclaimed(port: u16) {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        match TcpListener::bind(address) {
            Ok(listener) => {
                assert_eq!(listener.local_addr().unwrap().port(), port);
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            result => panic!("listener {port} did not release its bind: {result:?}"),
        }
    }
}

fn assert_no_instance_trees(harness: &Harness) {
    let instances = harness.gila_home.path().join("jupyter").join("instances");
    if !instances.exists() {
        return;
    }
    assert_eq!(
        fs::read_dir(instances).unwrap().count(),
        0,
        "rollback left private instance state behind"
    );
}

fn fixture_shutdown(port: u16, token: &str, base_path: &str) {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("fixture connection");
    write!(
        stream,
        "POST {}/api/shutdown HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: token {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        base_path.trim_end_matches('/')
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
    let password_hash = "argon2:$argon2id$v=19$m=65536,t=3,p=4$c2FsdA$aGFzaC8rPQ";
    let extra = "--fixture-spoof-candidates --ServerApp.default_url /voila";
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        token,
        "--password-hash",
        password_hash,
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
    assert!(record.identity_bound);
    let instance_hex = instance_hex(record.instance_id);
    let base_path = format!("/__gila/{instance_hex}/");
    let runtime_relative = format!("jupyter/instances/{instance_hex}/runtime");
    assert_eq!(record.url, format!("http://127.0.0.1:{port}{base_path}"));
    assert_eq!(
        record.runtime_path.as_deref(),
        Some(runtime_relative.as_str())
    );
    let log_path = record.log_path.as_deref().expect("durable log_path");
    assert!(log_path.starts_with("jupyter/logs/start-"));
    let absolute_log = harness.gila_home.path().join(log_path);
    assert!(absolute_log.is_file());
    wait_for_text(&absolute_log, "fixture heartbeat");
    let log = fs::read_to_string(&absolute_log).expect("read durable log");
    assert!(!log.contains(token));
    assert!(!log.contains(&percent_encoded(token)));
    assert!(!log.contains(password_hash));
    assert!(!log.contains(&percent_encoded(password_hash)));
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
    let environment: BTreeMap<String, String> = environment
        .lines()
        .map(|line| {
            let (key, value) = line.split_once('=').expect("captured environment pair");
            (key.to_ascii_uppercase(), value.to_string())
        })
        .collect();
    for secret in [
        "GILA_HOME",
        "GILA_AUTH_TOKEN",
        "GILA_OPERATOR_KEY",
        "NEWT_API_KEY",
        "OPENAI_API_KEY",
        "PYTHONPATH",
        "PYTHONHOME",
        "PIP_INDEX_URL",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ] {
        assert!(
            !environment.contains_key(secret),
            "child environment leaked {secret}"
        );
    }
    for required in ["PATH", "HOME", "JUPYTER_TOKEN_FILE", "JUPYTER_RUNTIME_DIR"] {
        assert!(
            environment.contains_key(required),
            "child missed {required}"
        );
    }
    assert!(!environment.values().any(|value| value.contains(token)));
    #[cfg(windows)]
    {
        assert!(harness.work.path().join(".fixture-winsock-ready").is_file());
        let mut expected_keys: BTreeSet<String> = [
            "PATH",
            "HOME",
            "JUPYTER_TOKEN_FILE",
            "JUPYTER_RUNTIME_DIR",
            "TERM",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        for optional in ["USER", "LANG", "LC_ALL", "LC_CTYPE"] {
            if std::env::var_os(optional).is_some() {
                expected_keys.insert(optional.to_string());
            }
        }
        for required in [
            "SYSTEMROOT",
            "WINDIR",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "TEMP",
            "TMP",
            "APPDATA",
            "LOCALAPPDATA",
            "PATHEXT",
        ] {
            if let Some(expected) = std::env::var_os(required) {
                expected_keys.insert(required.to_string());
                assert_eq!(
                    environment.get(required).map(String::as_str),
                    Some(expected.to_string_lossy().as_ref()),
                    "Windows child changed {required}"
                );
            }
        }
        assert_eq!(
            environment.keys().cloned().collect::<BTreeSet<_>>(),
            expected_keys,
            "Windows child environment must be exactly the case-insensitive allowlist"
        );
        assert!(!environment.contains_key("COMSPEC"));
    }

    let argv = fs::read_to_string(harness.work.path().join(".fixture-argv"))
        .expect("fixture argv capture");
    let args: Vec<_> = argv.lines().collect();
    let final_ip = args.iter().rposition(|arg| *arg == "--ip").unwrap();
    let final_port = args.iter().rposition(|arg| *arg == "--port").unwrap();
    let final_default_url = args
        .iter()
        .rposition(|arg| *arg == "--ServerApp.default_url")
        .unwrap();
    assert_eq!(args[final_ip + 1], "127.0.0.1");
    assert_eq!(args[final_port + 1], port.to_string());
    assert_eq!(args[final_default_url + 1], "/voila");
    assert!(!argv.contains(token));
    assert!(!argv.contains(&percent_encoded(token)));
    assert!(!argv.contains(password_hash));
    assert!(!argv.contains(&percent_encoded(password_hash)));
    assert!(!args.iter().any(|arg| arg.contains("token")));
    let instance_dir = harness
        .gila_home
        .path()
        .join("jupyter")
        .join("instances")
        .join(&instance_hex);
    assert!(!instance_dir.join("token").exists());
    assert!(!instance_dir.join("jupyter_config.py").exists());
    let runtime_entries: Vec<_> = fs::read_dir(instance_dir.join("runtime"))
        .unwrap()
        .collect();
    assert_eq!(runtime_entries.len(), 1);

    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(
        status.status.success(),
        "status failed: {}",
        stderr(&status)
    );
    assert!(stdout(&status).contains("running"));
    assert!(stdout(&status).contains(&base_path));
    let list = harness.run(&["jupyter", "list"]);
    assert!(list.status.success(), "list failed: {}", stderr(&list));
    assert!(stdout(&list).contains("registered server"));
    assert!(stdout(&list).contains(&format!("handle {handle}: running")));

    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
    assert!(stdout(&stop).contains("stopped"));
    cleanup.stopped();
    wait_for_text_for(
        &harness.work.path().join(".fixture-stopped"),
        "stopped",
        Duration::from_secs(6),
    );
    assert_listener_port_reclaimed(port);
    assert!(!harness.registry().servers.contains_key(&handle));
    assert!(!instance_dir.exists());

    let second_stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(second_stop.status.success());
    assert!(stdout(&second_stop).contains("not running"));
}

#[test]
fn headless_start_requires_explicit_auth_but_browser_and_password_flows_work() {
    let headless = Harness::new();
    let rejected = headless.run(&["jupyter", "start"]);
    assert!(
        !rejected.status.success(),
        "authless headless start exited zero"
    );
    assert!(stderr(&rejected).contains("requires --token, --password, or --password-hash"));
    assert!(!headless.work.path().join(".fixture-argv").exists());
    assert!(!headless.registry_path().exists());

    let help = headless.run(&["jupyter", "start", "--help"]);
    assert!(
        help.status.success(),
        "start help failed: {}",
        stderr(&help)
    );
    let help_text = stdout(&help);
    assert!(help_text.contains("Headless use requires"));
    assert!(help_text.contains("--open-browser may generate"));

    for (label, auth) in [
        ("browser", vec!["--open-browser"]),
        ("password", vec!["--password", "fixture-password"]),
    ] {
        let harness = Harness::new();
        let port = free_port();
        let mut args = vec!["jupyter", "start", "--port", ""];
        let port_text = port.to_string();
        args[3] = &port_text;
        args.extend(auth);
        let start = harness.run(&args);
        assert!(
            start.status.success(),
            "{label} start failed: {}",
            stderr(&start)
        );
        let handle = parse_handle(&start);
        let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
        assert!(
            stop.status.success(),
            "{label} stop failed: {}",
            stderr(&stop)
        );
        assert_no_instance_trees(&harness);
    }
}

#[test]
fn persistence_failure_kills_listener_and_descendant_tree() {
    let harness = Harness::new();
    let broken_registry = harness.registry_path();
    let extra = format!(
        "--fixture-break-registry {} --fixture-descendant-port 0",
        broken_registry.display()
    );
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        "0",
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
    let port = read_port_marker(&harness.work.path().join(".fixture-listener-ready"));
    let descendant_port = read_port_marker(&harness.work.path().join(".fixture-descendant-ready"));
    assert_listener_port_reclaimed(port);
    assert_listener_port_reclaimed(descendant_port);
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
    let record = harness.registry().servers[&handle].clone();
    let instance_id = record.instance_id;
    let mut cleanup = ServerCleanup {
        harness: &harness,
        handle,
        active: true,
    };

    let base_path = url::Url::parse(&record.url).unwrap().path().to_string();
    fixture_shutdown(port, token, &base_path);
    assert_listener_port_reclaimed(port);
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
fn registered_cleanup_preserves_unowned_replacement_until_owner_tree_is_restored() {
    let harness = Harness::new();
    let port = free_port();
    let token = "registered-replacement-token";
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
    let record = harness.registry().servers[&handle].clone();
    let base_path = url::Url::parse(&record.url).unwrap().path().to_string();
    fixture_shutdown(port, token, &base_path);
    assert_listener_port_reclaimed(port);

    let instance_dir = harness
        .gila_home
        .path()
        .join("jupyter/instances")
        .join(instance_hex(record.instance_id));
    let retained_owner = harness.work.path().join("retained-owned-instance");
    fs::rename(&instance_dir, &retained_owner).expect("retain owned instance tree");
    fs::create_dir(&instance_dir).expect("unowned replacement directory");
    fs::write(instance_dir.join("replacement-sentinel"), b"preserve").unwrap();

    let rejected = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!rejected.status.success(), "unsafe cleanup exited zero");
    assert!(instance_dir.join("replacement-sentinel").is_file());
    assert!(harness.registry().servers.contains_key(&handle));

    fs::remove_dir_all(&instance_dir).expect("remove test replacement");
    fs::rename(&retained_owner, &instance_dir).expect("restore owned instance tree");
    let cleanup = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(
        cleanup.status.success(),
        "cleanup failed: {}",
        stderr(&cleanup)
    );
    assert!(stdout(&cleanup).contains("not running"));
    assert!(!harness.registry().servers.contains_key(&handle));
    assert!(!instance_dir.exists());
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
    assert_invalid_record_preserved(&harness, handle, &mismatched_token, "token");

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
    assert_listener_port_reclaimed(port);
    assert!(!harness.registry().servers.contains_key(&handle));
}

#[test]
fn oversized_registry_token_is_rejected_without_an_authenticated_request() {
    let harness = Harness::new();
    let port = free_port();
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "bounded-registry-token",
    ]);
    assert!(start.status.success(), "start failed: {}", stderr(&start));
    let handle = parse_handle(&start);
    let original = harness.registry();
    let mut cleanup = RegistryRestoringCleanup {
        harness: &harness,
        handle,
        original: original.clone(),
        active: true,
    };
    let mut oversized = original.clone();
    oversized.servers.get_mut(&handle).unwrap().token = "x".repeat(5_000);
    harness.write_registry(&oversized);
    let requests = harness.work.path().join(".fixture-requests");
    fs::remove_file(&requests).expect("clear startup requests");

    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(
        status.status.success(),
        "status failed: {}",
        stderr(&status)
    );
    assert!(stdout(&status).contains("unreachable"));
    assert!(stdout(&status).contains("exceeds"));
    let list = harness.run(&["jupyter", "list"]);
    assert!(list.status.success(), "list failed: {}", stderr(&list));
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!stop.status.success(), "oversized-token stop exited zero");
    assert!(
        !requests.exists(),
        "invalid durable token reached the fixture"
    );
    assert_eq!(harness.registry(), oversized);

    harness.write_registry(&original);
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(stop.status.success(), "cleanup failed: {}", stderr(&stop));
    cleanup.stopped();
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
    assert!(!argv.contains("fixture-password"));
    assert!(!argv.contains(&percent_encoded("fixture-password")));
    assert!(!argv.contains("NotebookApp.password"));
    assert!(!argv.contains("PasswordIdentityProvider.hashed_password"));
    assert!(!argv.contains("IdentityProvider.token"));
    let log_path = harness.registry().servers[&handle]
        .log_path
        .as_deref()
        .unwrap()
        .to_string();
    let log = fs::read_to_string(harness.gila_home.path().join(log_path)).unwrap();
    assert!(!log.contains("fixture-password"));
    assert!(!log.contains(&percent_encoded("fixture-password")));
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
    assert_listener_port_reclaimed(port);
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
    assert_listener_port_reclaimed(port);
    assert_no_instance_trees(&harness);
}

#[test]
fn runtime_metadata_rejections_rollback_process_and_private_state() {
    for mode in [
        "runtime-symlink",
        "runtime-multiple",
        "runtime-flood",
        "runtime-oversized",
        "runtime-token-mismatch",
        "runtime-base-mismatch",
        "runtime-url-mismatch",
        "runtime-port-mismatch",
        "runtime-pid-mismatch",
    ] {
        let harness = Harness::new();
        let port = free_port();
        fs::write(harness.mode_path(), mode).unwrap();
        let start = harness.run(&[
            "jupyter",
            "start",
            "--port",
            &port.to_string(),
            "--token",
            "runtime-validation-token",
        ]);
        assert!(!start.status.success(), "{mode} unexpectedly started");
        assert!(
            stderr(&start).contains("runtime"),
            "{mode} missing runtime diagnostic: {}",
            stderr(&start)
        );
        if harness.registry_path().exists() {
            assert!(harness.registry().servers.is_empty(), "{mode} registered");
        }
        assert_listener_port_reclaimed(port);
        if mode == "runtime-symlink" {
            let instances = harness.gila_home.path().join("jupyter/instances");
            let retained: Vec<_> = fs::read_dir(&instances)
                .expect("retained unsafe runtime tree")
                .map(|entry| entry.expect("retained entry").path())
                .collect();
            assert_eq!(retained.len(), 1);
            #[cfg(windows)]
            {
                let runtime = retained[0].join("runtime");
                assert!(
                    runtime.join(".fixture-runtime-reparse-created").is_file(),
                    "fixture must exercise a real Windows reparse point"
                );
                let junction = fs::read_dir(&runtime)
                    .expect("runtime entries")
                    .map(|entry| entry.expect("runtime entry").path())
                    .find(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("jpserver-"))
                    })
                    .expect("runtime junction");
                fs::remove_dir(&junction).expect("remove test junction without traversal");
            }
            fs::remove_dir_all(&retained[0]).expect("test-owned unsafe runtime cleanup");
        } else {
            assert_no_instance_trees(&harness);
        }
    }
}

#[test]
fn startup_rollback_preserves_an_unowned_instance_directory_replacement() {
    let harness = Harness::new();
    let port = free_port();
    fs::write(harness.mode_path(), "replace-instance-directory").unwrap();
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "replacement-rollback-token",
    ]);
    assert!(!start.status.success(), "replacement start exited zero");
    assert!(
        stderr(&start).contains("replacement preserved")
            || stderr(&start).contains("ownership marker"),
        "missing safe-cleanup diagnostic: {}",
        stderr(&start)
    );
    assert_listener_port_reclaimed(port);
    if harness.registry_path().exists() {
        assert!(harness.registry().servers.is_empty());
    }
    let instances = harness.gila_home.path().join("jupyter/instances");
    let replacements: Vec<_> = fs::read_dir(&instances)
        .expect("replacement instances")
        .map(|entry| entry.expect("replacement entry").path())
        .collect();
    #[cfg(not(windows))]
    assert_eq!(replacements.len(), 1);
    #[cfg(windows)]
    assert_eq!(
        replacements.len(),
        2,
        "Windows retains the renamed exact owner alongside the unrelated replacement"
    );
    let replacement = replacements
        .iter()
        .find(|path| path.join("replacement-sentinel").is_file())
        .expect("unowned replacement directory");
    assert_eq!(
        fs::read(replacement.join("replacement-sentinel")).unwrap(),
        b"preserve"
    );
    for path in replacements {
        fs::remove_dir_all(path).expect("test-owned replacement cleanup");
    }
}

#[test]
fn partial_runtime_json_is_retried_and_cleanup_failure_is_surfaced() {
    let partial = Harness::new();
    let partial_port = free_port();
    fs::write(partial.mode_path(), "runtime-partial").unwrap();
    let start = partial.run(&[
        "jupyter",
        "start",
        "--port",
        &partial_port.to_string(),
        "--token",
        "partial-runtime-token",
    ]);
    assert!(
        start.status.success(),
        "partial JSON failed: {}",
        stderr(&start)
    );
    let handle = parse_handle(&start);
    let stop = partial.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(
        stop.status.success(),
        "partial cleanup failed: {}",
        stderr(&stop)
    );
    assert_no_instance_trees(&partial);

    let obstacle = Harness::new();
    let obstacle_port = free_port();
    fs::write(obstacle.mode_path(), "cleanup-token-dir").unwrap();
    let start = obstacle.run(&[
        "jupyter",
        "start",
        "--port",
        &obstacle_port.to_string(),
        "--token",
        "cleanup-obstacle-token",
    ]);
    assert!(!start.status.success(), "cleanup obstacle exited zero");
    assert!(
        stderr(&start).contains("cleanup") || stderr(&start).contains("startup secrets"),
        "cleanup failure was hidden: {}",
        stderr(&start)
    );
    assert_listener_port_reclaimed(obstacle_port);
    assert!(obstacle.registry().servers.is_empty());
    let instances = obstacle.gila_home.path().join("jupyter/instances");
    let retained: Vec<_> = fs::read_dir(&instances)
        .expect("retained replacement tree")
        .map(|entry| entry.expect("retained entry").path())
        .collect();
    assert_eq!(retained.len(), 1);
    assert!(retained[0].join("token").is_dir());
    #[cfg(windows)]
    assert_eq!(
        fs::metadata(retained[0].join("token.fixture-retained"))
            .expect("retained exact token")
            .len(),
        0,
        "Gila must erase the exact retained token handle before preserving the replacement"
    );
    fs::remove_dir_all(&retained[0]).expect("test-owned obstacle cleanup");
}

#[test]
fn stale_port_rebind_never_receives_authenticated_lifecycle_requests() {
    let harness = Harness::new();
    let port = free_port();
    let token = "stale-rebind-token";
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
    let record = harness.registry().servers[&handle].clone();
    let base_path = url::Url::parse(&record.url).unwrap().path().to_string();
    fixture_shutdown(port, token, &base_path);
    assert_listener_port_reclaimed(port);

    let mut unrelated = spawn_unrelated_listener(&harness, port);
    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(status.status.success());
    assert!(stdout(&status).contains("runtime file is missing"));
    let list = harness.run(&["jupyter", "list"]);
    assert!(list.status.success());
    assert!(stdout(&list).contains("unreachable"));
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!stop.status.success(), "stale stop exited zero");
    assert!(stderr(&stop).contains("runtime identity"));
    assert!(unrelated.0.try_wait().unwrap().is_none());
    assert_record_present(&harness, handle, record.instance_id);

    let _ = unrelated.0.kill();
    let _ = unrelated.0.wait();
    assert_listener_port_reclaimed(port);
    let cleanup = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(
        cleanup.status.success(),
        "cleanup failed: {}",
        stderr(&cleanup)
    );
    assert!(!harness.registry().servers.contains_key(&handle));
    assert_no_instance_trees(&harness);
}

#[test]
fn unbound_legacy_record_never_sends_stored_credentials_or_shutdown_while_accepting() {
    let harness = Harness::new();
    let port = free_port();
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "legacy-boundary-token",
    ]);
    assert!(start.status.success(), "start failed: {}", stderr(&start));
    let handle = parse_handle(&start);
    let original = harness.registry();
    let mut legacy = original.clone();
    legacy.servers.get_mut(&handle).unwrap().identity_bound = false;
    legacy.servers.get_mut(&handle).unwrap().runtime_path = None;
    harness.write_registry(&legacy);
    let request_log = harness.work.path().join(".fixture-requests");
    fs::remove_file(&request_log).expect("clear startup request log");

    let status = harness.run(&["jupyter", "status", &handle.to_string()]);
    assert!(status.status.success());
    assert!(stdout(&status).contains("unreachable"));
    assert!(stdout(&status).contains("status was not probed"));
    let list = harness.run(&["jupyter", "list"]);
    assert!(list.status.success());
    assert!(stdout(&list).contains("unreachable"));
    assert!(
        !request_log.exists(),
        "legacy status/list contacted the accepting endpoint"
    );
    let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(!stop.status.success(), "unbound accepting stop exited zero");
    assert!(stderr(&stop).contains("predates instance-bound shutdown"));
    let requests = fs::read_to_string(&request_log).unwrap_or_default();
    assert!(!requests.contains("POST "));
    assert!(!requests.contains("authorization=true"));
    assert_record_present(&harness, handle, legacy.servers[&handle].instance_id);
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());

    harness.write_registry(&original);
    let cleanup = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(
        cleanup.status.success(),
        "cleanup failed: {}",
        stderr(&cleanup)
    );
    assert_no_instance_trees(&harness);
}

#[test]
fn oversized_startup_and_parallel_list_bodies_are_rejected_at_the_shared_cap() {
    let startup = Harness::new();
    let startup_port = free_port();
    fs::write(startup.mode_path(), "kernels-oversized").unwrap();
    let failed = startup.run(&[
        "jupyter",
        "start",
        "--port",
        &startup_port.to_string(),
        "--token",
        "oversized-startup-token",
    ]);
    assert!(!failed.status.success(), "oversized readiness body started");
    assert!(
        stderr(&failed).contains("65536-byte lifecycle body limit"),
        "unexpected oversized readiness failure: {}",
        stderr(&failed)
    );
    assert_listener_port_reclaimed(startup_port);
    assert_no_instance_trees(&startup);

    let list = Harness::new();
    let mut started = start_concurrently(&list, 8);
    fs::write(list.mode_path(), "kernels-oversized").unwrap();
    let began = Instant::now();
    let output = list.run(&["jupyter", "list"]);
    assert!(
        output.status.success(),
        "oversized list failed: {}",
        stderr(&output)
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "3-second aggregate deadline exceeded reasonable scheduling slack"
    );
    let rendered = stdout(&output);
    assert_eq!(
        rendered.matches("65536-byte lifecycle body limit").count(),
        8,
        "every bounded worker must reject its own oversized body: {rendered}"
    );

    fs::remove_file(list.mode_path()).unwrap();
    let handles: Vec<_> = started.entries.iter().map(|(handle, _)| *handle).collect();
    for handle in handles {
        let stop = list.run(&["jupyter", "stop", &handle.to_string()]);
        assert!(stop.status.success(), "cleanup failed: {}", stderr(&stop));
        started.mark_stopped(handle);
    }
    assert_no_instance_trees(&list);
}

struct ConcurrentServers<'a> {
    harness: &'a Harness,
    entries: Vec<(u64, u16)>,
}

impl ConcurrentServers<'_> {
    fn mark_stopped(&mut self, handle: u64) {
        self.entries.retain(|(active, _)| *active != handle);
    }
}

impl Drop for ConcurrentServers<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.harness.mode_path());
        for (handle, _) in self.entries.drain(..) {
            let output = self.harness.run(&["jupyter", "stop", &handle.to_string()]);
            if !output.status.success() {
                eprintln!(
                    "ERROR: concurrent-server cleanup failed for handle {handle}: {}",
                    stderr(&output)
                );
            }
        }
    }
}

fn start_concurrently(harness: &Harness, count: usize) -> ConcurrentServers<'_> {
    let mut children = Vec::new();
    for index in 0..count {
        // Let each fixture bind port zero itself; this removes the otherwise
        // unavoidable free-port handoff race across parallel test processes.
        let token = format!("concurrent-token-{index}");
        let mut command = harness.command();
        command.args(["jupyter", "start", "--port", "0", "--token", &token]);
        children.push(command.spawn().expect("spawn concurrent start"));
    }
    let mut entries = Vec::with_capacity(count);
    let mut failures = Vec::new();
    for child in children {
        match child.wait_with_output() {
            Ok(output) if output.status.success() => {
                let handle = parse_handle(&output);
                let port = harness.registry().servers[&handle].port;
                entries.push((handle, port));
            }
            Ok(output) => failures.push(stderr(&output)),
            Err(error) => failures.push(format!("failed to wait for concurrent start: {error}")),
        }
    }
    let servers = ConcurrentServers { harness, entries };
    assert!(
        failures.is_empty(),
        "concurrent starts failed: {failures:?}"
    );
    servers
}

#[test]
fn concurrent_starts_have_unique_handles_and_duplicate_stops_are_idempotent() {
    let harness = Harness::new();
    let mut started = start_concurrently(&harness, 4);
    let handles: std::collections::HashSet<_> =
        started.entries.iter().map(|(handle, _)| *handle).collect();
    assert_eq!(handles.len(), 4);
    let registry = harness.registry();
    assert_eq!(registry.servers.len(), 4);
    assert!(handles
        .iter()
        .all(|handle| registry.servers.contains_key(handle)));

    let duplicate_handle = started.entries[0].0;
    fs::write(harness.mode_path(), "shutdown-barrier").unwrap();
    let mut duplicate_stops = Vec::new();
    for _ in 0..2 {
        let mut command = harness.command();
        command.args(["jupyter", "stop", &duplicate_handle.to_string()]);
        duplicate_stops.push(command.spawn().expect("spawn duplicate stop"));
    }
    let mut outcomes = Vec::new();
    for child in duplicate_stops {
        let output = child.wait_with_output().expect("wait duplicate stop");
        assert!(
            output.status.success(),
            "duplicate stop failed: {}",
            stderr(&output)
        );
        outcomes.push(stdout(&output));
    }
    assert_eq!(
        outcomes
            .iter()
            .filter(|output| output.contains(&format!("handle {duplicate_handle}: stopped")))
            .count(),
        1,
        "exactly one racing stop owns the successful CAS: {outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|output| output.contains(&format!("handle {duplicate_handle}: not running")))
            .count(),
        1,
        "the CAS loser must report idempotent absence: {outcomes:?}"
    );
    started.mark_stopped(duplicate_handle);
    fs::remove_file(harness.mode_path()).unwrap();

    let remaining: Vec<_> = started.entries.iter().map(|(handle, _)| *handle).collect();
    for handle in remaining {
        let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
        assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
        started.mark_stopped(handle);
    }
    assert!(harness.registry().servers.is_empty());
    assert_no_instance_trees(&harness);
}

#[test]
fn concurrent_stop_preserves_a_different_instance_that_wins_the_cas_race() {
    let harness = Harness::new();
    let port = free_port();
    let start = harness.run(&[
        "jupyter",
        "start",
        "--port",
        &port.to_string(),
        "--token",
        "cas-race-token",
    ]);
    assert!(start.status.success(), "start failed: {}", stderr(&start));
    let handle = parse_handle(&start);
    let mut replacement_registry = harness.registry();
    let mut replacement = replacement_registry.servers[&handle].clone();
    replacement.instance_id = [0x5a; 16];
    replacement.identity_bound = false;
    replacement.runtime_path = None;

    fs::write(harness.mode_path(), "shutdown-barrier").unwrap();
    let mut command = harness.command();
    command.args(["jupyter", "stop", &handle.to_string()]);
    let stop_child = command.spawn().expect("spawn racing stop");
    wait_for_text_for(
        &harness.work.path().join(".fixture-shutdown-received"),
        "received",
        Duration::from_secs(6),
    );
    replacement_registry
        .servers
        .insert(handle, replacement.clone());
    harness.write_registry(&replacement_registry);
    let stop = stop_child.wait_with_output().expect("wait racing stop");
    assert!(
        !stop.status.success(),
        "different-instance race exited zero"
    );
    assert!(stderr(&stop).contains("replacement was preserved"));
    assert_eq!(
        harness.registry().servers[&handle].instance_id,
        replacement.instance_id
    );

    let cleanup = harness.run(&["jupyter", "stop", &handle.to_string()]);
    assert!(
        cleanup.status.success(),
        "replacement cleanup failed: {}",
        stderr(&cleanup)
    );
    assert!(harness.registry().servers.is_empty());
}

#[test]
fn list_probes_have_a_bounded_aggregate_deadline() {
    let harness = Harness::new();
    let mut started = start_concurrently(&harness, 4);
    fs::write(harness.mode_path(), "kernels-delay").unwrap();
    let began = Instant::now();
    let list = harness.run(&["jupyter", "list"]);
    let elapsed = began.elapsed();
    assert!(list.status.success(), "list failed: {}", stderr(&list));
    assert!(
        elapsed < Duration::from_secs(5),
        "3-second list deadline exceeded reasonable scheduling slack: {elapsed:?}"
    );
    for (handle, _) in &started.entries {
        assert!(stdout(&list).contains(&format!("handle {handle}: running")));
    }

    fs::remove_file(harness.mode_path()).unwrap();
    let handles: Vec<_> = started.entries.iter().map(|(handle, _)| *handle).collect();
    for handle in handles {
        let stop = harness.run(&["jupyter", "stop", &handle.to_string()]);
        assert!(stop.status.success(), "stop failed: {}", stderr(&stop));
        started.mark_stopped(handle);
    }
    assert_no_instance_trees(&harness);
}

#[test]
fn real_traitlets_parser_demonstrates_deprecated_notebook_ip_alias_bypass() {
    let output = match Command::new("jupyter")
        .args([
            "notebook",
            "--NotebookApp.ip=0.0.0.0",
            "--ip=127.0.0.1",
            "--show-config-json",
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::env::var_os("GILA_REQUIRE_REAL_JUPYTER").is_some() {
                panic!("real Jupyter was required but unavailable");
            }
            eprintln!("skipping real parser regression: Jupyter is unavailable");
            return;
        }
        Err(error) => panic!("failed to run real Jupyter parser: {error}"),
    };
    if !output.status.success() {
        if std::env::var_os("GILA_REQUIRE_REAL_JUPYTER").is_none() {
            eprintln!(
                "skipping real parser regression: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        panic!(
            "required real Jupyter parser failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let config: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("real Jupyter config JSON");
    assert_eq!(
        config["ServerApp"]["ip"], "0.0.0.0",
        "Traitlets behavior changed; re-audit the exact extra-argument boundary"
    );
    assert!(
        gila_rejects_extra_arg("--NotebookApp.ip=0.0.0.0").is_ok(),
        "Gila must reject the demonstrated deprecated-alias bypass"
    );
}

fn gila_rejects_extra_arg(argument: &str) -> Result<(), String> {
    let harness = Harness::new();
    let output = harness.run(&["jupyter", "start", &format!("--extra={argument}")]);
    if output.status.success() {
        Err("unsafe argument unexpectedly started Jupyter".to_string())
    } else if stderr(&output).contains("exact allowlist") {
        Ok(())
    } else {
        Err(stderr(&output))
    }
}

struct RealServerCleanup {
    gila: PathBuf,
    home: PathBuf,
    gila_home: PathBuf,
    work: PathBuf,
    handle: u64,
    active: bool,
}

impl RealServerCleanup {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.gila);
        command
            .env("HOME", &self.home)
            .env("GILA_HOME", &self.gila_home)
            .current_dir(&self.work)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

impl Drop for RealServerCleanup {
    fn drop(&mut self) {
        if self.active {
            match self
                .command()
                .args(["jupyter", "stop", &self.handle.to_string()])
                .output()
            {
                Ok(output) if output.status.success() => {}
                Ok(output) => eprintln!(
                    "ERROR: real-Jupyter cleanup failed and may have left an orphan (handle {}): {}",
                    self.handle,
                    stderr(&output)
                ),
                Err(error) => eprintln!(
                    "ERROR: real-Jupyter cleanup could not run and may have left an orphan (handle {}): {error}",
                    self.handle
                ),
            }
        }
    }
}

#[test]
fn real_jupyter_lifecycle_uses_private_runtime_metadata() {
    if std::env::var_os("GILA_REQUIRE_REAL_JUPYTER_LIFECYCLE").is_none() {
        eprintln!("skipping real Jupyter lifecycle; opt-in environment is unset");
        return;
    }
    let home = TempDir::new().expect("real temporary HOME");
    let work = TempDir::new().expect("real temporary workdir");
    let gila_home = TempDir::new().expect("real temporary GILA_HOME");
    let gila = PathBuf::from(env!("CARGO_BIN_EXE_gila"));
    let token = "real token +/%!";
    let password = "real plaintext password";
    // Notebook 6 records literal port 0 instead of the post-bind port in its
    // nbserver metadata, so the cross-profile lane must reserve an explicit
    // port immediately before spawn rather than using port 0.
    let port = free_port().to_string();
    let mut start_command = Command::new(&gila);
    start_command
        .env("HOME", home.path())
        .env("GILA_HOME", gila_home.path())
        .current_dir(work.path())
        .args([
            "jupyter",
            "start",
            "--port",
            &port,
            "--token",
            token,
            "--password",
            password,
        ]);
    let start = start_command.output().expect("run real Jupyter start");
    assert!(
        start.status.success(),
        "real start failed: {}",
        stderr(&start)
    );
    let handle = parse_handle(&start);
    let mut cleanup = RealServerCleanup {
        gila,
        home: home.path().to_path_buf(),
        gila_home: gila_home.path().to_path_buf(),
        work: work.path().to_path_buf(),
        handle,
        active: true,
    };
    let registry: RegistryFile = serde_json::from_slice(
        &fs::read(gila_home.path().join("servers.json")).expect("real registry"),
    )
    .expect("parse real registry");
    let record = &registry.servers[&handle];
    assert!(record.identity_bound);
    let instance_dir = gila_home
        .path()
        .join("jupyter")
        .join("instances")
        .join(instance_hex(record.instance_id));
    assert!(!instance_dir.join("token").exists());
    assert!(!instance_dir.join("jupyter_config.py").exists());
    let runtime_entries: Vec<_> = fs::read_dir(instance_dir.join("runtime"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            (name.starts_with("jpserver-") || name.starts_with("nbserver-"))
                && name.ends_with(".json")
        })
        .collect();
    assert_eq!(runtime_entries.len(), 1);
    if let Some(expected) = std::env::var_os("GILA_EXPECT_RUNTIME_PREFIX") {
        assert!(
            runtime_entries[0].starts_with(expected.to_string_lossy().as_ref()),
            "unexpected runtime file: {:?}",
            runtime_entries
        );
    }
    let log = fs::read_to_string(
        gila_home
            .path()
            .join(record.log_path.as_deref().expect("real log path")),
    )
    .expect("real durable log");
    let encoded_token = percent_encoded(token);
    let encoded_password = percent_encoded(password);
    for secret in [token, password, &encoded_token, &encoded_password] {
        assert!(!log.contains(secret), "real durable log leaked a secret");
    }

    let status = cleanup
        .command()
        .args(["jupyter", "status", &handle.to_string()])
        .output()
        .expect("real status");
    assert!(
        status.status.success(),
        "real status failed: {}",
        stderr(&status)
    );
    assert!(stdout(&status).contains("running"));
    let stop = cleanup
        .command()
        .args(["jupyter", "stop", &handle.to_string()])
        .output()
        .expect("real stop");
    assert!(stop.status.success(), "real stop failed: {}", stderr(&stop));
    cleanup.active = false;
    assert!(!instance_dir.exists());
}
