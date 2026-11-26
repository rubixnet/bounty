//! Seam: `traction-server` behaviour that needs no database, so it runs on
//! all 3 OSes: the runtime-SQLx rule and refusing to start on an inline
//! secret.

use std::path::Path;
use std::process::Command;

fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// No `query!`-family macro anywhere in the crate: the build must need no
/// `.sqlx` cache and no live database. Banned names are assembled from
/// pieces so this file does not trip its own scan.
#[test]
fn no_compile_time_checked_sqlx_macro_is_used() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let banned: Vec<String> = [
        "query",
        "query_as",
        "query_scalar",
        "query_file",
        "query_unchecked",
    ]
    .iter()
    .map(|name| format!("{name}{}", "!"))
    .collect();
    let mut files = Vec::new();
    rust_sources(&root.join("src"), &mut files);
    rust_sources(&root.join("tests"), &mut files);
    assert!(files.len() > 5);
    for file in files {
        if file.file_name().is_some_and(|n| n == "no_database.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for name in &banned {
            for needle in [format!("sqlx::{name}"), format!(" {name}(")] {
                assert!(
                    !text.contains(&needle),
                    "{} uses the compile-time checked macro `{name}`",
                    file.display()
                );
            }
        }
    }
    assert!(!root.join(".sqlx").exists(), "a .sqlx cache must not exist");
    assert!(!root.join("..").join("..").join(".sqlx").exists());
}

#[test]
fn migrations_are_embedded_and_there_is_no_default_partition() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(source.contains("sqlx::migrate!"));
    for entry in std::fs::read_dir(root.join("migrations")).unwrap() {
        let sql = std::fs::read_to_string(entry.unwrap().path())
            .unwrap()
            .to_ascii_uppercase();
        assert!(
            !sql.contains("PARTITION OF"),
            "partitions are created by the server"
        );
        assert!(!sql.contains("DEFAULT PARTITION"));
    }
}

#[test]
fn the_server_binary_refuses_to_start_with_a_secret_written_inline() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("traction-server.toml");
    std::fs::write(
        &config,
        "[server]\ndatabase_url = \"postgres://u:hunter2@localhost/db\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_traction-server"))
        .arg("--config")
        .arg(&config)
        .env_remove("TRACTION_DATABASE_URL")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("server.database_url"), "{stderr}");
    assert!(stderr.contains("inline"), "{stderr}");
    assert!(
        !stderr.contains("hunter2"),
        "the secret must never be echoed"
    );
}

#[test]
fn the_server_binary_needs_its_database_url_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("traction-server.toml");
    std::fs::write(&config, "").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_traction-server"))
        .arg("--config")
        .arg(&config)
        .env_remove("TRACTION_DATABASE_URL")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TRACTION_DATABASE_URL"));
}
