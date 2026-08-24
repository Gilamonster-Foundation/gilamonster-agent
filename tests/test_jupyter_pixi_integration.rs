// Integration tests for Pixi-first Jupyter support
#![cfg(feature = "jupyter")]

use gilamonster_agent::gila_pixi;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_pixi_manifest_detection() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path();

    // No pixi.toml yet
    assert!(!gila_pixi::has_pixi_manifest(path));

    // Create pixi.toml
    let manifest_path = path.join("pixi.toml");
    fs::write(
        manifest_path,
        r#"
[project]
name = "test-project"
version = "0.1.0"

[tasks]
test = "pytest"
"#,
    )
    .expect("Failed to write pixi.toml");

    assert!(gila_pixi::has_pixi_manifest(path));
}

#[test]
fn test_manifest_loading_and_parsing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path();
    let manifest_path = path.join("pixi.toml");

    fs::write(
        manifest_path,
        r#"
[project]
name = "test-project"
version = "0.1.0"

[tasks]
jupyter = "jupyter lab"
test = "pytest"
"#,
    )
    .expect("Failed to write pixi.toml");

    let manifest = gila_pixi::load_manifest(path).expect("Failed to load manifest");

    // Check project table exists
    assert!(manifest.project.is_some());
    assert!(manifest.workspace.is_none());

    // Check tasks are parsed
    assert!(manifest.tasks.is_some());
    let tasks = manifest.tasks.unwrap();
    assert!(tasks.contains_key("jupyter"));
    assert!(tasks.contains_key("test"));
}

#[test]
fn test_legacy_manifest_detection() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path();
    let manifest_path = path.join("pixi.toml");

    // Legacy format with [project]
    fs::write(
        manifest_path,
        r#"
[project]
name = "test-project"
"#,
    )
    .expect("Failed to write pixi.toml");

    let manifest = gila_pixi::load_manifest(path).expect("Failed to load manifest");
    assert!(gila_pixi::is_legacy_manifest(&manifest));
}

#[test]
fn test_find_jupyter_task_conventional_names() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path();
    let manifest_path = path.join("pixi.toml");

    fs::write(
        manifest_path,
        r#"
[project]
name = "test-project"

[tasks]
lab-local = "jupyter lab"
other = "pytest"
"#,
    )
    .expect("Failed to write pixi.toml");

    let manifest = gila_pixi::load_manifest(path).expect("Failed to load manifest");

    // Should find lab-local by convention
    assert_eq!(
        gila_pixi::find_jupyter_task(&manifest, None),
        Some("lab-local".to_string())
    );
}

#[test]
fn test_find_jupyter_task_explicit() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path();
    let manifest_path = path.join("pixi.toml");

    fs::write(
        manifest_path,
        r#"
[project]
name = "test-project"

[tasks]
my-jupyter = "jupyter notebook"
"#,
    )
    .expect("Failed to write pixi.toml");

    let manifest = gila_pixi::load_manifest(path).expect("Failed to load manifest");

    // With explicit task name
    assert_eq!(
        gila_pixi::find_jupyter_task(&manifest, Some("my-jupyter")),
        Some("my-jupyter".to_string())
    );

    // Should fail if task doesn't exist
    assert_eq!(
        gila_pixi::find_jupyter_task(&manifest, Some("nonexistent")),
        None
    );
}

#[test]
fn test_modernize_manifest() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path();
    let manifest_path = path.join("pixi.toml");

    let legacy_content = r#"
[project]
name = "test-project"
version = "0.1.0"

[tasks]
test = "pytest"
"#;

    fs::write(&manifest_path, legacy_content).expect("Failed to write pixi.toml");

    // Modernize the manifest
    gila_pixi::modernize_manifest(path).expect("Failed to modernize");

    // Read back and verify
    let content = fs::read_to_string(&manifest_path).expect("Failed to read pixi.toml");
    assert!(content.contains("[workspace]"));
    assert!(!content.contains("[project]"));
}

#[test]
fn test_preview_modernization() {
    let legacy = gilamonster_agent::gila_pixi::PixiManifest {
        project: Some(gilamonster_agent::gila_pixi::ProjectTable {
            name: Some("test".to_string()),
            rest: Default::default(),
        }),
        workspace: None,
        tasks: None,
    };

    let preview = gila_pixi::preview_modernization(&legacy);
    assert!(preview.contains("[project] → [workspace]"));

    let modern = gilamonster_agent::gila_pixi::PixiManifest {
        project: None,
        workspace: Some(gilamonster_agent::gila_pixi::WorkspaceTable {
            members: None,
            rest: Default::default(),
        }),
        tasks: None,
    };

    let preview = gila_pixi::preview_modernization(&modern);
    assert!(preview.contains("Already using modern"));
}
