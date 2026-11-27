//! Seam: the "no raw text" gate for everything Step 2 derives, with canary
//! text planted where real input carries it (a hook prompt, a file name and
//! its contents, a commit message) and run through a real daemon (hooks,
//! watcher, commit observer, presence sensor) and the finalizer. None of it
//! may appear in any byte of the data folder, WAL included.
//!
//! The daemon's log lines are `&'static str` by construction (`log.rs`), so
//! no input text can reach them; this test checks the stored bytes.

mod common;

use std::time::Duration;

use common::fixtures::{self, git};
use common::{TestDaemon, fast_watcher, path_str, poll_until, stored_mutations};
use serde_json::json;

const PROMPT: &str = "CANARY-PROMPT-b81e";
const FILE_STEM: &str = "canaryfile_7d2e";
const FILE_BODY: &str = "CANARY-FILE-BODY-4c90";
const COMMIT_MSG: &str = "CANARY-COMMIT-MSG-a23f";

fn count(db: &rusqlite::Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn data_folder_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            bytes.extend(std::fs::read(&path).unwrap_or_default());
        }
    }
    bytes
}

fn has(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn canary_text_never_reaches_the_derived_tables() {
    let mut daemon = TestDaemon::start_with(|config| {
        fast_watcher(config);
        config.presence.subslot = Duration::from_millis(60);
        config.commits.poll = Duration::from_millis(25);
        config.finalizer.enabled = false;
    });
    let repo = fixtures::repo(&daemon.home().join("code").join("alpha"), None);
    let response = daemon.caller().register_project(&repo);
    assert_eq!(response.status, 200, "{response:?}");
    // The commit observer takes its baseline when it handles the watch.
    std::thread::sleep(Duration::from_millis(300));

    // A hook prompt naming the file and carrying a stack trace.
    let prompt = json!({
        "session_id": "canary-session",
        "transcript_path": "/nowhere/canary.jsonl",
        "cwd": path_str(&repo),
        "prompt_id": "6cc0ac7e-854d-4139-8e0f-e0b52fd93a1d",
        "permission_mode": "acceptEdits",
        "hook_event_name": "UserPromptSubmit",
        "prompt": format!("fix {FILE_STEM}.rs {PROMPT}\n  at f (a.js:1:2)\n  at g (b.js:3:4)"),
    });
    let sent = daemon
        .caller()
        .request("POST", "/v1/hooks/claude-code")
        .body(prompt.to_string())
        .send();
    assert_eq!(sent.status, 200, "{}", sent.body);

    // A real file mutation and a real commit with a canary message.
    std::fs::write(repo.join(format!("{FILE_STEM}.rs")), FILE_BODY).unwrap();
    let live = daemon.live_db();
    poll_until(Duration::from_secs(30), "the file mutation", || {
        (!stored_mutations(&live).is_empty()).then_some(())
    });
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", COMMIT_MSG]);
    poll_until(Duration::from_secs(10), "the commit observation", || {
        (count(&live, "commit_observation") > 0).then_some(())
    });
    poll_until(Duration::from_secs(10), "a presence minute", || {
        (count(&live, "presence_minute") > 0).then_some(())
    });
    drop(live);
    daemon.shutdown();

    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let batches = traction_daemon::finalize_settled(daemon.data_dir(), now + 3_600_000).unwrap();
    assert!(batches > 0, "nothing was finalized");

    let db = daemon.live_db();
    for table in [
        "presence_minute",
        "commit_observation",
        "work_bucket",
        "chain_key",
        "chain_link",
    ] {
        assert!(count(&db, table) > 0, "{table} was not populated");
    }
    let project = common::registered_projects(&db)[0].project_id.clone();
    drop(db);

    // The Audit Bundle for the canary Project, exported from the sealed chain.
    let bundle_path = daemon.data_dir().join("canary.traction-bundle.json");
    traction_daemon::export_bundle(daemon.data_dir(), &project, &bundle_path, now).unwrap();
    let bundle = std::fs::read(&bundle_path).unwrap();
    assert!(!bundle.is_empty());
    let chain_text = chain_tables_text(daemon.data_dir());
    let public_key_file =
        std::fs::read(daemon.data_dir().join(traction_daemon::SIGNING_PUB_FILE)).unwrap();

    let bytes = data_folder_bytes(daemon.data_dir());
    for canary in [PROMPT, FILE_STEM, FILE_BODY, COMMIT_MSG] {
        assert!(!has(&bytes, canary), "{canary} reached the data folder");
        assert!(
            !chain_text.contains(canary),
            "{canary} reached chain_link or chain_key"
        );
        assert!(!has(&public_key_file, canary), "{canary} is in signing.pub");
        assert!(!has(&bundle, canary), "{canary} is in the Audit Bundle");
    }
    // The bundle names no path: not the Project Root, not the data folder.
    for path in [
        path_str(&repo).to_owned(),
        path_str(daemon.data_dir()).to_owned(),
    ] {
        assert!(!has(&bundle, &path), "{path} is in the Audit Bundle");
    }
}

/// Every column of every `chain_link` and `chain_key` row, as one text.
fn chain_tables_text(data_dir: &std::path::Path) -> String {
    let db = common::open_db_read_only(data_dir);
    let mut text = String::new();
    for (table, columns) in [
        ("chain_key", "key_id || public_key || created_at_ms"),
        (
            "chain_link",
            "key_id || seq || window_start_ms || ruleset_id || prev_hash || payload_digest \
             || entry_hash || sealed_at_ms || clock_regressed || payload_json || signature",
        ),
    ] {
        let mut stmt = db
            .prepare(&format!("SELECT {columns} FROM {table}"))
            .unwrap();
        for row in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
            text.push_str(&row.unwrap());
        }
    }
    text
}

/// Step 3b S1: Frame Records and the `capture-frame` child's stdout hold ids,
/// hashes, numbers and closed enums only. The data folder's path carries a
/// canary; neither the `frame` table nor the child's line may name it.
#[test]
fn frame_records_and_the_child_line_carry_no_text() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join(FILE_STEM);
    std::fs::create_dir_all(&dir).unwrap();
    let mut daemon = TestDaemon::start_with(|c| c.data_dir = dir.clone());
    daemon.shutdown();
    traction_daemon::enable_visual_proof(&dir).unwrap();
    let key = traction_daemon::VaultKey::load(&dir).unwrap().unwrap();

    // The post-grab pipeline on a real screen-sized image, then the writer.
    let image = traction_daemon::Bgra {
        width: 1920,
        height: 1080,
        pixels: (0..1920u32 * 1080 * 4).map(|i| (i % 251) as u8).collect(),
    };
    let report = traction_daemon::store_frame(&dir, &key, &image, None, 1_800_000_000_123, 15.0);
    let line = serde_json::to_string(&report).unwrap();
    assert!(!line.contains(FILE_STEM), "{line}");
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    let allowed = [
        "status",
        "frame_id",
        "frame_hash",
        "dhash",
        "captured_at_ms",
        "width",
        "height",
    ];
    for (k, v) in value.as_object().unwrap() {
        assert!(allowed.contains(&k.as_str()), "unexpected key {k}");
        let ok = v.is_u64()
            || v.as_str().is_some_and(|s| {
                s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                    || [
                        "written",
                        "duplicate",
                        "permission_denied",
                        "unsupported",
                        "focus_changed",
                        "error",
                    ]
                    .contains(&s)
            });
        assert!(ok, "{k}={v} is not an id, hash, number or status");
    }
    let traction_daemon::ChildReport::Written {
        frame_id,
        frame_hash,
        captured_at_ms,
        width,
        height,
        ..
    } = report
    else {
        panic!("{line}")
    };
    let row = traction_daemon::FrameRow {
        record: traction_core::FrameRecord {
            frame_id: frame_id.clone(),
            frame_hash,
            captured_at_ms,
            trigger: traction_core::FrameTrigger::Scheduled,
            scheduled_at_ms: Some(captured_at_ms - 5),
            nonce_id: None,
        },
        width,
        height,
        vault_file: traction_daemon::frame_path(&dir, &frame_id),
    };
    assert!(traction_daemon::record_frame(&dir, row).unwrap());

    let db = common::open_db_read_only(&dir);
    let mut stmt = db
        .prepare(
            "SELECT frame_id, window_start_ms, captured_at_ms, frame_hash, trigger,
                    scheduled_at_ms, nonce_id, width, height, pixels_state FROM frame",
        )
        .unwrap();
    let rows: Vec<Vec<rusqlite::types::Value>> = stmt
        .query_map([], |r| (0..10).map(|i| r.get(i)).collect())
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 1);
    for value in &rows[0] {
        use rusqlite::types::Value;
        match value {
            Value::Integer(_) | Value::Null => {}
            Value::Text(text) => {
                assert!(!text.contains(FILE_STEM));
                assert!(
                    traction_core::parse_frame_id(text).is_some()
                        || (text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()))
                        || ["scheduled", "present"].contains(&text.as_str()),
                    "{text} is not an id, hash or enum"
                );
            }
            other => panic!("unexpected column {other:?}"),
        }
    }
    // The Vault file is ciphertext: the canary path is not in it either.
    let bin = std::fs::read(traction_daemon::frame_path(&dir, &frame_id)).unwrap();
    assert!(!has(&bin, FILE_STEM));
}
