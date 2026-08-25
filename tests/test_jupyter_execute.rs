#![cfg(feature = "jupyter-test-fixture")]

//! Black-box execution tests for notebook path resolution.
//! Uses a fake Jupyter fixture (nbconvert mode) to verify resolved paths are passed correctly.
//! Tests verify that relative paths are resolved to canonical absolute paths before execution.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// Get the fake Jupyter fixture binary
fn get_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gila-jupyter-fixture"))
}

/// Set up a temp directory with fake Jupyter in PATH
fn setup_fake_jupyter(temp_dir: &TempDir) -> String {
    let bin_dir = temp_dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("Failed to create bin dir");

    let fixture = get_fixture();
    #[cfg(unix)]
    let jupyter_path = bin_dir.join("jupyter");
    #[cfg(windows)]
    let jupyter_path = bin_dir.join("jupyter.exe");

    fs::copy(&fixture, &jupyter_path).expect("Failed to copy fixture");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&jupyter_path, fs::Permissions::from_mode(0o755))
            .expect("Failed to set executable permission");
    }

    format!(
        "{}{}{}",
        bin_dir.display(),
        if cfg!(windows) { ";" } else { ":" },
        std::env::var("PATH").unwrap_or_default()
    )
}

#[test]
fn test_execute_notebook_basename_resolves_correctly() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let work_dir = temp_dir.path();
    let notebook_path = work_dir.join("test.ipynb");
    fs::write(
        &notebook_path,
        r#"{"cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#,
    )
    .expect("Failed to write notebook");

    let path_env = setup_fake_jupyter(&temp_dir);
    let gila = PathBuf::from(env!("CARGO_BIN_EXE_gila"));

    let output = Command::new(&gila)
        .env("PATH", &path_env)
        .current_dir(work_dir)
        .arg("jupyter")
        .arg("execute")
        .arg("test.ipynb")
        .output()
        .expect("Failed to run gila jupyter execute");

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(output.status.success(), "execute should succeed");

    // Fixture writes to its cwd, which is work_dir (notebook parent when no --working-dir)
    let argv_file = work_dir.join(".fixture-argv");
    let argv = fs::read_to_string(&argv_file).expect("Fixture should write argv to child cwd");
    println!("Fixture argv:\n{}", argv);

    // Should contain the notebook path
    assert!(
        argv.contains(".ipynb"),
        "Fixture should receive notebook path"
    );
}

#[test]
fn test_execute_notebook_nested_without_working_dir() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let work_dir = temp_dir.path();
    let nested = work_dir.join("week1").join("labs");
    fs::create_dir_all(&nested).expect("Failed to create nested dirs");

    let notebook_path = nested.join("assignment.ipynb");
    fs::write(
        &notebook_path,
        r#"{"cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#,
    )
    .expect("Failed to write notebook");

    let path_env = setup_fake_jupyter(&temp_dir);
    let gila = PathBuf::from(env!("CARGO_BIN_EXE_gila"));

    let output = Command::new(&gila)
        .env("PATH", &path_env)
        .current_dir(work_dir)
        .arg("jupyter")
        .arg("execute")
        .arg("week1/labs/assignment.ipynb")
        .output()
        .expect("Failed to run gila jupyter execute");

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "execute should resolve nested path"
    );

    // Fixture writes to notebook's parent directory (nested = week1/labs/)
    let argv_file = nested.join(".fixture-argv");
    let argv = fs::read_to_string(&argv_file).expect("Fixture should write argv to child cwd");
    println!("Fixture argv for nested:\n{}", argv);
    assert!(argv.contains("assignment.ipynb"));
}

#[test]
fn test_execute_notebook_with_explicit_working_dir() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let work_subdir = temp_dir.path().join("work");
    fs::create_dir(&work_subdir).expect("Failed to create work dir");

    let labs_dir = work_subdir.join("labs");
    fs::create_dir(&labs_dir).expect("Failed to create labs dir");

    let notebook_path = labs_dir.join("exercise.ipynb");
    fs::write(
        &notebook_path,
        r#"{"cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#,
    )
    .expect("Failed to write notebook");

    let path_env = setup_fake_jupyter(&temp_dir);
    let gila = PathBuf::from(env!("CARGO_BIN_EXE_gila"));

    let output = Command::new(&gila)
        .env("PATH", &path_env)
        .current_dir(temp_dir.path())
        .arg("jupyter")
        .arg("execute")
        .arg("labs/exercise.ipynb")
        .arg("--working-dir")
        .arg(work_subdir.to_str().unwrap())
        .output()
        .expect("Failed to run gila jupyter execute");

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "execute with --working-dir should succeed"
    );

    // Fixture writes to work_subdir (the explicit --working-dir)
    let argv_file = work_subdir.join(".fixture-argv");
    let argv = fs::read_to_string(&argv_file).expect("Fixture should write argv to child cwd");
    println!("Fixture argv with --working-dir:\n{}", argv);
    assert!(argv.contains("exercise.ipynb"));
}

#[test]
fn test_execute_notebook_absolute_path() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let notebook_path = temp_dir.path().join("absolute.ipynb");
    fs::write(
        &notebook_path,
        r#"{"cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#,
    )
    .expect("Failed to write notebook");

    let path_env = setup_fake_jupyter(&temp_dir);
    let gila = PathBuf::from(env!("CARGO_BIN_EXE_gila"));

    let output = Command::new(&gila)
        .env("PATH", &path_env)
        .arg("jupyter")
        .arg("execute")
        .arg(notebook_path.to_str().unwrap())
        .output()
        .expect("Failed to run gila jupyter execute");

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "execute should handle absolute paths"
    );

    // Fixture writes to notebook parent (temp_dir.path())
    let argv_file = temp_dir.path().join(".fixture-argv");
    let argv = fs::read_to_string(&argv_file).expect("Fixture should write argv to child cwd");
    assert!(argv.contains("absolute.ipynb"));
}

#[test]
fn test_execute_notebook_with_spaces_in_path() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let work_dir = temp_dir.path();
    let dirs_with_spaces = work_dir.join("my assignments");
    fs::create_dir(&dirs_with_spaces).expect("Failed to create dir with spaces");

    let notebook_path = dirs_with_spaces.join("lab with spaces.ipynb");
    fs::write(
        &notebook_path,
        r#"{"cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#,
    )
    .expect("Failed to write notebook");

    let path_env = setup_fake_jupyter(&temp_dir);
    let gila = PathBuf::from(env!("CARGO_BIN_EXE_gila"));

    let output = Command::new(&gila)
        .env("PATH", &path_env)
        .current_dir(work_dir)
        .arg("jupyter")
        .arg("execute")
        .arg("my assignments/lab with spaces.ipynb")
        .output()
        .expect("Failed to run gila jupyter execute");

    if !output.status.success() {
        eprintln!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "execute should handle paths with spaces"
    );

    // Fixture writes to notebook's parent directory (my assignments/)
    let notebook_parent = dirs_with_spaces.clone();
    let argv_file = notebook_parent.join(".fixture-argv");
    let argv = fs::read_to_string(&argv_file).expect("Fixture should write argv to child cwd");
    assert!(argv.contains("spaces.ipynb"));
}

#[test]
fn test_execute_notebook_nonexistent_fails() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path_env = setup_fake_jupyter(&temp_dir);
    let gila = PathBuf::from(env!("CARGO_BIN_EXE_gila"));

    let output = Command::new(&gila)
        .env("PATH", &path_env)
        .current_dir(temp_dir.path())
        .arg("jupyter")
        .arg("execute")
        .arg("/nonexistent/notebook.ipynb")
        .output()
        .expect("Failed to run gila jupyter execute");

    assert!(
        !output.status.success(),
        "execute should fail for nonexistent notebook"
    );
}
