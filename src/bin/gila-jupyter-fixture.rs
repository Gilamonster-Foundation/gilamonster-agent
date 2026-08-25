//! Deterministic Jupyter lifecycle fixture.
//!
//! The normal mode accepts the same last-value-wins `--ip`, `--port`, and
//! `--NotebookApp.token` flags as Jupyter, announces its actual loopback URL,
//! and serves authenticated kernels/shutdown endpoints. Test-only behavior is
//! selected through command arguments or a `.fixture-mode` file in the cwd;
//! custom environment variables cannot be used because production deliberately
//! scrubs them.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Config {
    ip: IpAddr,
    port: u16,
    token: String,
    token_file: PathBuf,
    base_path: String,
    default_url: String,
    runtime_dir: PathBuf,
    break_registry: Option<PathBuf>,
    descendant_port: Option<u16>,
    invalid_only: bool,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|arg| arg == "--fixture-listener-only") {
        listener_only(&args);
        return;
    }

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    record_invocation(&cwd, &args);
    if args.get(1).is_some_and(|arg| arg == "nbconvert") {
        let _ = fs::write(cwd.join(".fixture-cwd"), cwd.to_string_lossy().as_bytes());
        return;
    }

    let config = parse_config(&args);
    if config.invalid_only {
        eprintln!("fixture exited before publishing runtime metadata");
        return;
    }
    let listener = TcpListener::bind(SocketAddr::new(config.ip, config.port))
        .unwrap_or_else(|error| panic!("fixture failed to bind loopback listener: {error}"));
    let actual_addr = listener.local_addr().expect("fixture local address");
    fs::write(
        cwd.join(".fixture-listener-ready"),
        actual_addr.port().to_string(),
    )
    .expect("fixture listener readiness marker");
    #[cfg(windows)]
    fs::write(cwd.join(".fixture-winsock-ready"), b"ready")
        .expect("fixture WinSock readiness marker");

    if let Some(port) = config.descendant_port {
        spawn_descendant(&cwd, port);
    }
    if let Some(path) = &config.break_registry {
        fs::create_dir_all(path).expect("fixture failed to break registry path");
    }

    let host = match actual_addr.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    let base_url = format!("http://{host}:{}{}", actual_addr.port(), config.base_path);
    let runtime_file = write_runtime_metadata(&config, actual_addr, &base_url);
    if fs::read_to_string(cwd.join(".fixture-mode"))
        .is_ok_and(|mode| mode.trim() == "replace-instance-directory")
    {
        let instance_dir = config
            .token_file
            .parent()
            .expect("fixture token has instance parent");
        #[cfg(not(windows))]
        fs::remove_dir_all(instance_dir).expect("fixture remove owned instance directory");
        #[cfg(windows)]
        {
            // Gila deliberately retains handles to the secret files on
            // Windows. Removing the tree would leave delete-pending directory
            // entries until those handles close, so model an attacker using
            // the operation Windows can perform with delete sharing enabled:
            // rename the exact tree out of the controlled path, then replace
            // that path with an unrelated directory.
            let name = instance_dir
                .file_name()
                .expect("fixture instance directory name")
                .to_string_lossy();
            let retained = instance_dir.with_file_name(format!("{name}.fixture-retained"));
            fs::rename(instance_dir, &retained)
                .expect("fixture retain exact owned instance directory");
        }
        fs::create_dir(instance_dir).expect("fixture create unowned replacement directory");
        fs::write(instance_dir.join("replacement-sentinel"), b"preserve")
            .expect("fixture replacement sentinel");
    }
    if fs::read_to_string(cwd.join(".fixture-mode"))
        .is_ok_and(|mode| mode.trim() == "cleanup-token-dir")
    {
        #[cfg(not(windows))]
        fs::remove_file(&config.token_file).expect("fixture replace token file");
        #[cfg(windows)]
        fs::rename(
            &config.token_file,
            config.token_file.with_extension("fixture-retained"),
        )
        .expect("fixture retain exact token file");
        fs::create_dir(&config.token_file).expect("fixture create token cleanup obstacle");
    }
    let announced = format!("{}{}", base_url, config.default_url.trim_start_matches('/'));
    println!("[I 2026-08-24 12:00:00.000 ServerApp] Jupyter Server is running at:");
    println!("[I 2026-08-24 12:00:00.000 ServerApp] {announced}");
    std::io::stdout().flush().ok();

    thread::spawn(|| {
        thread::sleep(Duration::from_millis(350));
        println!("[I 2026-08-24 12:00:00.350 ServerApp] fixture heartbeat");
        std::io::stdout().flush().ok();
    });

    listener
        .set_nonblocking(true)
        .expect("fixture nonblocking listener");
    let mode_path = cwd.join(".fixture-mode");
    let mut shutdown = false;
    while !shutdown {
        match listener.accept() {
            Ok((mut stream, _)) => {
                shutdown =
                    handle_request(&mut stream, &config.token, &config.base_path, &mode_path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
    if fs::read_to_string(&mode_path).is_ok_and(|mode| mode.trim() == "shutdown-barrier") {
        fs::write(cwd.join(".fixture-shutdown-received"), b"received")
            .expect("fixture shutdown marker");
        thread::sleep(Duration::from_millis(750));
    }
    drop(listener);
    let _ = fs::remove_file(runtime_file);
    let _ = fs::write(cwd.join(".fixture-stopped"), b"stopped");
}

fn parse_config(args: &[String]) -> Config {
    let mut config = Config {
        ip: "127.0.0.1".parse().unwrap(),
        port: 8888,
        token: String::new(),
        token_file: PathBuf::new(),
        base_path: "/".to_string(),
        default_url: "/lab".to_string(),
        runtime_dir: PathBuf::from(
            env::var_os("JUPYTER_RUNTIME_DIR").expect("fixture JUPYTER_RUNTIME_DIR"),
        ),
        break_registry: None,
        descendant_port: None,
        invalid_only: false,
    };
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--ip" => {
                index += 1;
                config.ip = args[index].parse().expect("fixture --ip");
            }
            "--port" => {
                index += 1;
                config.port = args[index].parse().expect("fixture --port");
            }
            "--NotebookApp.default_url"
            | "--ServerApp.default_url"
            | "--JupyterNotebookApp.default_url" => {
                index += 1;
                config.default_url = normalize_default_url(&args[index]);
            }
            "--fixture-break-registry" => {
                index += 1;
                config.break_registry = Some(PathBuf::from(&args[index]));
            }
            "--fixture-descendant-port" => {
                index += 1;
                config.descendant_port = Some(args[index].parse().expect("descendant port"));
            }
            "--fixture-spoof-candidates" => {}
            "--fixture-invalid-only" => config.invalid_only = true,
            _ => {}
        }
        index += 1;
    }
    let token_path =
        PathBuf::from(env::var_os("JUPYTER_TOKEN_FILE").expect("fixture JUPYTER_TOKEN_FILE"));
    config.token = fs::read_to_string(&token_path).expect("fixture read token file");
    config.token_file = token_path;
    let config_path = PathBuf::from(value_after(args, "--config").expect("fixture --config"));
    let protected_config = fs::read_to_string(config_path).expect("fixture read protected config");
    config.base_path = config_string_value(&protected_config, "c.ServerApp.base_url")
        .expect("fixture ServerApp.base_url config");
    config
}

fn config_string_value(config: &str, key: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let value = line
            .trim()
            .strip_prefix(key)?
            .trim()
            .strip_prefix('=')?
            .trim();
        serde_json::from_str(value).ok()
    })
}

fn normalize_default_url(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        format!("/{}", path.trim_matches('/'))
    }
}

fn secure_create(path: &Path, contents: &[u8]) -> File {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).expect("fixture create runtime file");
    file.write_all(contents)
        .expect("fixture write runtime file");
    file.flush().expect("fixture flush runtime file");
    file
}

fn write_runtime_metadata(config: &Config, actual_addr: SocketAddr, base_url: &str) -> PathBuf {
    let pid = std::process::id();
    let mode = fs::read_to_string(
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".fixture-mode"),
    )
    .unwrap_or_default();
    let mode = mode.trim();
    let mut runtime_token = config.token.clone();
    let mut runtime_base = config.base_path.clone();
    let mut runtime_url = base_url.to_string();
    let mut runtime_port = u64::from(actual_addr.port());
    let mut runtime_pid = u64::from(pid);
    match mode {
        "runtime-token-mismatch" => runtime_token = "wrong-runtime-token".to_string(),
        "runtime-base-mismatch" => runtime_base = "/wrong/".to_string(),
        "runtime-url-mismatch" => {
            runtime_url = format!("http://127.0.0.1:{}/wrong/", actual_addr.port())
        }
        "runtime-port-mismatch" => runtime_port = runtime_port.saturating_add(1),
        "runtime-pid-mismatch" => runtime_pid = runtime_pid.saturating_add(1),
        _ => {}
    }
    let encoded = serde_json::to_vec(&serde_json::json!({
        "base_url": runtime_base,
        "hostname": actual_addr.ip().to_string(),
        "password": false,
        "pid": runtime_pid,
        "port": runtime_port,
        "root_dir": env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        "secure": false,
        "sock": "",
        "token": runtime_token,
        "url": runtime_url,
        "version": "fixture"
    }))
    .expect("fixture encode runtime JSON");
    let runtime_file = config.runtime_dir.join(format!("jpserver-{pid}.json"));

    if mode == "runtime-flood" {
        for index in 0..300usize {
            let noise = config.runtime_dir.join(format!("noise-{index:04}"));
            secure_create(&noise, b"noise");
        }
    }

    match mode {
        "runtime-symlink" => {
            let target = config.runtime_dir.join("runtime-target");
            #[cfg(unix)]
            secure_create(&target, &encoded);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &runtime_file)
                .expect("fixture create runtime symlink");
            #[cfg(windows)]
            {
                fs::create_dir(&target).expect("fixture create junction target");
                let system_root = env::var_os("SYSTEMROOT").expect("fixture SYSTEMROOT");
                let output = Command::new(PathBuf::from(system_root).join("System32/cmd.exe"))
                    .arg("/D")
                    .arg("/C")
                    .arg("mklink")
                    .arg("/J")
                    .arg(&runtime_file)
                    .arg(&target)
                    .output()
                    .expect("fixture launch mklink junction");
                assert!(
                    output.status.success(),
                    "fixture create runtime junction failed: stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::write(
                    config.runtime_dir.join(".fixture-runtime-reparse-created"),
                    b"ready",
                )
                .expect("fixture runtime reparse marker");
            }
        }
        "runtime-oversized" => {
            secure_create(&runtime_file, &vec![b'x'; 65 * 1024]);
        }
        "runtime-partial" => {
            secure_create(&runtime_file, b"{");
            thread::sleep(Duration::from_millis(250));
            let mut file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&runtime_file)
                .expect("fixture reopen partial runtime file");
            file.write_all(&encoded)
                .expect("fixture complete runtime JSON");
            file.flush().expect("fixture flush completed runtime JSON");
        }
        _ => {
            secure_create(&runtime_file, &encoded);
        }
    }

    if mode == "runtime-multiple" {
        let second = config
            .runtime_dir
            .join(format!("nbserver-{}.json", pid.saturating_add(1)));
        secure_create(&second, &encoded);
    }
    runtime_file
}

fn record_invocation(cwd: &Path, args: &[String]) {
    let _ = fs::write(cwd.join(".fixture-argv"), args.join("\n"));
    // `vars()` panics when a deliberately allowed OS value is not Unicode.
    // Capture lossily only at this diagnostic fixture boundary so the real
    // child-environment code remains fully OsString-safe.
    let mut environment: Vec<_> = env::vars_os().collect();
    environment.sort_by(|left, right| left.0.cmp(&right.0));
    let encoded = environment
        .into_iter()
        .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = fs::write(cwd.join(".fixture-env"), encoded);
}

fn spawn_descendant(cwd: &Path, port: u16) {
    let executable = env::current_exe().expect("fixture executable");
    let ready_path = cwd.join(".fixture-descendant-ready");
    let mut child = Command::new(executable)
        .arg("--fixture-listener-only")
        .arg("--port")
        .arg(port.to_string())
        .arg("--fixture-ready-file")
        .arg(&ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fixture descendant");

    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready_path.exists() {
        if let Some(status) = child.try_wait().expect("inspect fixture descendant") {
            panic!("fixture descendant exited before ready: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "fixture descendant did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // The process-group/job owner, not this fixture, owns descendant cleanup.
    drop(child);
}

fn listener_only(args: &[String]) {
    let port = value_after(args, "--port")
        .expect("listener-only port")
        .parse::<u16>()
        .expect("listener-only numeric port");
    let ready = PathBuf::from(value_after(args, "--fixture-ready-file").expect("ready file"));
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("descendant bind");
    let actual_port = listener.local_addr().expect("descendant address").port();
    fs::write(ready, actual_port.to_string()).expect("descendant ready file");
    loop {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.write_all(b"fixture descendant\n");
        }
    }
}

fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .rposition(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn handle_request(stream: &mut TcpStream, token: &str, base_path: &str, mode_path: &Path) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut request_bytes = Vec::with_capacity(1_024);
    let mut chunk = [0u8; 1_024];
    while request_bytes.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(bytes_read) => {
                request_bytes.extend_from_slice(&chunk[..bytes_read]);
                if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let request = String::from_utf8_lossy(&request_bytes);
    let mut request_lines = request.lines();
    let request_line = request_lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_ascii_whitespace();
    let method = request_parts.next().unwrap_or_default();
    let path = request_parts.next().unwrap_or_default();
    let authorized = request_lines
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value.trim() == format!("token {token}")
        });
    let mode = fs::read_to_string(mode_path).unwrap_or_default();
    if let Some(directory) = mode_path.parent() {
        let request_log = directory.join(".fixture-requests");
        if let Ok(mut log) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(request_log)
        {
            let _ = writeln!(log, "{method} {path} authorization={authorized}");
        }
    }
    let kernels_path = format!("{}/api/kernels", base_path.trim_end_matches('/'));
    let shutdown_path = format!("{}/api/shutdown", base_path.trim_end_matches('/'));

    if method == "GET" && path == kernels_path {
        if mode.trim() == "kernels-delay" {
            thread::sleep(Duration::from_millis(1_200));
        }
        if !authorized || mode.trim() == "kernels-403" {
            write_response(stream, "403 Forbidden", b"");
        } else if mode.trim() == "kernels-503" {
            write_response(stream, "503 Service Unavailable", b"");
        } else if mode.trim() == "kernels-oversized" {
            // Production must reject this response from its declared length,
            // before buffering or decoding any body bytes. Sending only the
            // header keeps the regression deterministic under instrumented,
            // parallel coverage runs instead of making a 500 ms readiness
            // request wait for a deliberately irrelevant 128 KiB write.
            write_response_headers(stream, "200 OK", 128 * 1024);
        } else if matches!(mode.trim(), "kernels-malformed" | "kernels-malformed-exit") {
            write_response(stream, "200 OK", b"not-json");
        } else {
            write_response(stream, "200 OK", b"[]");
        }
        return mode.trim() == "kernels-malformed-exit";
    }

    if method == "POST" && path == shutdown_path {
        if !authorized || mode.trim() == "shutdown-403" {
            write_response(stream, "403 Forbidden", b"");
            return false;
        }
        if mode.trim() == "shutdown-503" {
            write_response(stream, "503 Service Unavailable", b"");
            return false;
        }
        if mode.trim() == "shutdown-no-exit" {
            write_response(stream, "204 No Content", b"");
            return false;
        }
        write_response(stream, "204 No Content", b"");
        return true;
    }

    write_response(stream, "404 Not Found", b"");
    false
}

fn write_response(stream: &mut TcpStream, status: &str, body: &[u8]) {
    write_response_headers(stream, status, body.len());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn write_response_headers(stream: &mut TcpStream, status: &str, content_length: usize) {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content_length
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.flush();
}
