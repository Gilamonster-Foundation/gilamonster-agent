//! Deterministic Jupyter lifecycle fixture.
//!
//! The normal mode accepts the same last-value-wins `--ip`, `--port`, and
//! `--NotebookApp.token` flags as Jupyter, announces its actual loopback URL,
//! and serves authenticated kernels/shutdown endpoints. Test-only behavior is
//! selected through command arguments or a `.fixture-mode` file in the cwd;
//! custom environment variables cannot be used because production deliberately
//! scrubs them.

use std::env;
use std::fs;
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
    base_path: String,
    default_url: String,
    display_query: Option<String>,
    break_registry: Option<PathBuf>,
    descendant_port: Option<u16>,
    spoof_candidates: bool,
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
    let listener = TcpListener::bind(SocketAddr::new(config.ip, config.port))
        .unwrap_or_else(|error| panic!("fixture failed to bind loopback listener: {error}"));
    let actual_addr = listener.local_addr().expect("fixture local address");

    if let Some(port) = config.descendant_port {
        spawn_descendant(&cwd, port);
    }
    if let Some(path) = &config.break_registry {
        fs::create_dir_all(path).expect("fixture failed to break registry path");
    }

    if config.spoof_candidates {
        println!("http://203.0.113.9:6553/lab?token={}", config.token);
        println!("http://127.0.0.1:6553/lab?token=wrong-token");
    }
    if config.invalid_only {
        println!("http://203.0.113.9:6553/lab?token={}", config.token);
        std::io::stdout().flush().ok();
        return;
    }

    let host = match actual_addr.ip() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    let mut announced = url::Url::parse(&format!(
        "http://{host}:{}{}{}",
        actual_addr.port(),
        config.base_path,
        config.default_url
    ))
    .expect("fixture announcement URL");
    // Match modern Jupyter Server: configured tokens are redacted unless the
    // caller supplies a custom display query. Production Gila supplies a
    // query-only custom URL so the actual host/port/base path remain intact.
    if let Some(query) = &config.display_query {
        announced.set_query(Some(query.trim_start_matches('?')));
    } else {
        announced.query_pairs_mut().append_pair("token", "...");
    }
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
    drop(listener);
    let _ = fs::write(cwd.join(".fixture-stopped"), b"stopped");
}

fn parse_config(args: &[String]) -> Config {
    let mut config = Config {
        ip: "127.0.0.1".parse().unwrap(),
        port: 8888,
        token: String::new(),
        base_path: String::new(),
        default_url: "/lab".to_string(),
        display_query: None,
        break_registry: None,
        descendant_port: None,
        spoof_candidates: false,
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
            "--NotebookApp.token" | "--IdentityProvider.token" => {
                index += 1;
                config.token = args[index].clone();
            }
            "--fixture-base-path" => {
                index += 1;
                config.base_path = normalize_base_path(&args[index]);
            }
            "--NotebookApp.default_url"
            | "--ServerApp.default_url"
            | "--JupyterNotebookApp.default_url" => {
                index += 1;
                config.default_url = normalize_default_url(&args[index]);
            }
            "--ServerApp.custom_display_url" => {
                index += 1;
                config.display_query = Some(args[index].clone());
            }
            "--fixture-break-registry" => {
                index += 1;
                config.break_registry = Some(PathBuf::from(&args[index]));
            }
            "--fixture-descendant-port" => {
                index += 1;
                config.descendant_port = Some(args[index].parse().expect("descendant port"));
            }
            "--fixture-spoof-candidates" => config.spoof_candidates = true,
            "--fixture-invalid-only" => config.invalid_only = true,
            _ => {}
        }
        index += 1;
    }
    config
}

fn normalize_base_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        String::new()
    } else {
        format!("/{}", path.trim_matches('/'))
    }
}

fn normalize_default_url(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        format!("/{}", path.trim_matches('/'))
    }
}

fn record_invocation(cwd: &Path, args: &[String]) {
    let _ = fs::write(cwd.join(".fixture-argv"), args.join("\n"));
    let mut environment: Vec<_> = env::vars().collect();
    environment.sort_by(|left, right| left.0.cmp(&right.0));
    let encoded = environment
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
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
    fs::write(ready, b"ready").expect("descendant ready file");
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
    let kernels_path = format!("{base_path}/api/kernels");
    let shutdown_path = format!("{base_path}/api/shutdown");

    if method == "GET" && path == kernels_path {
        if !authorized || mode.trim() == "kernels-403" {
            write_response(stream, "403 Forbidden", b"");
        } else if mode.trim() == "kernels-503" {
            write_response(stream, "503 Service Unavailable", b"");
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
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}
