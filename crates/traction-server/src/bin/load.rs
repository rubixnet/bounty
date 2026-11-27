//! `load`: the Gate 1 benchmark (spec S3b, Q23).
//!
//! It drives the **real** `traction-server` binary and a real Postgres. It
//! (1) creates a fresh database, migrates it and enrolls `--devices` devices,
//! (2) preseeds a month of data (`--seed-buckets` Work Buckets, with the
//! Activity Rollups, Anchors and Timeline Marks that go with them),
//! (3) starts the server as a child process, (4) has `--peak-devices`
//! devices send heartbeats concurrently (realistic sizes with Timeline Marks;
//! a share of them first replay a chunked offline backlog), and (5) reports
//! the ingest p95, the server's peak RSS and the measured Postgres bytes per
//! bucket. It exits non-zero when a limit is exceeded or any request failed.
//!
//! ```text
//! load --database-url postgres://u@host/postgres [--server-bin path]
//!      [--devices 15488] [--seed-buckets 6600000] [--peak-devices 1548]
//!      [--rounds 10] [--burst-rounds 2] [--interval-ms 2000]
//!      [--backlog-chunks 4] [--backlog-percent 10]
//!      [--max-p95-ms 50] [--max-rss-mb 512] [--report out.json] [--keep-db]
//! ```
//!
//! Gate 1 sizing: 15,488 MAU at 109,759 hours a month is 153 devices working
//! at any moment on average; peak concurrency is 10x that (about 1,530).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Barrier;
use traction_core::sync::{ChainTip, HeartbeatRequest, ProjectMeta, SyncMode, TimelineMark};
use traction_core::{
    ActivityRollup, DeviceSigningKey, HarnessId, StruggleAxes, TelemetryTier, WorkBucket,
};
use traction_server::token::{hash_secret, random_hex};
use zeroize::Zeroizing;

const WINDOW_MS: u64 = 120_000;
const LINKS_PER_CHUNK: u64 = 30;
const THIRTY_DAYS_MIN: i64 = 30 * 24 * 60;

struct Opts {
    database_url: String,
    server_bin: Option<PathBuf>,
    devices: u64,
    seed_buckets: u64,
    peak_devices: u64,
    rounds: u64,
    burst_rounds: u64,
    interval_ms: u64,
    backlog_chunks: u64,
    backlog_percent: u64,
    max_p95_ms: f64,
    max_rss_mb: f64,
    report: Option<PathBuf>,
    keep_db: bool,
}

fn parse_opts() -> Result<Opts, String> {
    let mut o = Opts {
        database_url: String::new(),
        server_bin: None,
        devices: 15_488,
        seed_buckets: 6_600_000,
        peak_devices: 0,
        rounds: 10,
        burst_rounds: 2,
        interval_ms: 2000,
        backlog_chunks: 4,
        backlog_percent: 10,
        max_p95_ms: 50.0,
        max_rss_mb: 512.0,
        report: None,
        keep_db: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        let num = |v: String| {
            v.parse::<u64>()
                .map_err(|_| format!("bad number for {flag}"))
        };
        match flag.as_str() {
            "--database-url" => o.database_url = value()?,
            "--server-bin" => o.server_bin = Some(PathBuf::from(value()?)),
            "--devices" => o.devices = num(value()?)?,
            "--seed-buckets" => o.seed_buckets = num(value()?)?,
            "--peak-devices" => o.peak_devices = num(value()?)?,
            "--rounds" => o.rounds = num(value()?)?,
            "--burst-rounds" => o.burst_rounds = num(value()?)?,
            "--interval-ms" => o.interval_ms = num(value()?)?.max(1),
            "--backlog-chunks" => o.backlog_chunks = num(value()?)?,
            "--backlog-percent" => o.backlog_percent = num(value()?)?,
            "--max-p95-ms" => o.max_p95_ms = value()?.parse().map_err(|_| "bad --max-p95-ms")?,
            "--max-rss-mb" => o.max_rss_mb = value()?.parse().map_err(|_| "bad --max-rss-mb")?,
            "--report" => o.report = Some(PathBuf::from(value()?)),
            "--keep-db" => o.keep_db = true,
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    if o.database_url.is_empty() {
        return Err("--database-url is required".into());
    }
    if o.devices == 0 || o.seed_buckets == 0 {
        return Err("--devices and --seed-buckets must be above zero".into());
    }
    if o.peak_devices == 0 {
        // 10x the average concurrency: monthly hours / 720 h, devices scaled.
        let average = (o.devices as f64 * 109_759.0 / 15_488.0 / 720.0).ceil();
        o.peak_devices = ((10.0 * average) as u64).clamp(1, o.devices);
    }
    // The server's per-device token bucket (burst 20) must not be what we
    // measure: keep each device under it.
    if o.rounds + o.burst_rounds + o.backlog_chunks > 18 {
        return Err(
            "--rounds + --burst-rounds + --backlog-chunks must stay within the 20-request burst"
                .into(),
        );
    }
    Ok(o)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// The deterministic Device Signing Key of device `d`.
fn key_for(d: u64) -> DeviceSigningKey {
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&(d + 1).to_le_bytes());
    seed[8] = 0x1D;
    DeviceSigningKey::from_seed(Zeroizing::new(seed)).expect("a valid seed")
}

fn project_of(d: u64) -> String {
    format!("dir:{d:064x}")
}

fn token_of(d: u64) -> String {
    format!("{d:064x}")
}

async fn connect_pool(url: &str, n: u32) -> Result<PgPool, String> {
    PgPoolOptions::new()
        .max_connections(n)
        .connect(url)
        .await
        .map_err(|e| format!("cannot connect to the database: {e}"))
}

fn sql(e: sqlx::Error) -> String {
    format!("database error: {e}")
}

/// Enrolls every device: user, device (with its token hash), pinned Device
/// Signing Key and Project metadata.
async fn enroll(pool: &PgPool, devices_total: u64) -> Result<(), String> {
    for base in (0..devices_total).step_by(2000) {
        let ids: Vec<u64> = (base..(base + 2000).min(devices_total)).collect();
        let chunk: Vec<DeviceSigningKey> = ids.iter().map(|d| key_for(*d)).collect();
        let users: Vec<String> = ids.iter().map(|d| format!("lu{d}")).collect();
        let devices: Vec<String> = ids.iter().map(|d| format!("ld{d}")).collect();
        let hashes: Vec<Vec<u8>> = ids
            .iter()
            .map(|d| hash_secret(&token_of(*d)).to_vec())
            .collect();
        let key_ids: Vec<String> = chunk
            .iter()
            .map(|k| k.key_id().as_str().to_owned())
            .collect();
        let pubs: Vec<String> = chunk.iter().map(|k| k.verifying_key().to_hex()).collect();
        let projects: Vec<String> = ids.iter().map(|d| project_of(*d)).collect();
        sqlx::query(
            "INSERT INTO users (id, sub, display_name) SELECT u, u, u FROM UNNEST($1::text[]) u",
        )
        .bind(&users)
        .execute(pool)
        .await
        .map_err(sql)?;
        sqlx::query(
            "INSERT INTO devices (id, user_id, label, token_hash)
             SELECT * FROM UNNEST($1::text[], $2::text[], $1::text[], $3::bytea[])",
        )
        .bind(&devices)
        .bind(&users)
        .bind(&hashes)
        .execute(pool)
        .await
        .map_err(sql)?;
        sqlx::query(
            "INSERT INTO device_keys (device_id, key_id, public_key)
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[])",
        )
        .bind(&devices)
        .bind(&key_ids)
        .bind(&pubs)
        .execute(pool)
        .await
        .map_err(sql)?;
        sqlx::query(
            "INSERT INTO projects (user_id, project_id, project_name, root_path, os_username, updated_at_ms)
             SELECT u, p, 'load-project', '/home/load/project', 'load', 0
             FROM UNNEST($1::text[], $2::text[]) AS t(u, p)",
        )
        .bind(&users)
        .bind(&projects)
        .execute(pool)
        .await
        .map_err(sql)?;
    }
    Ok(())
}

/// Seeds a month of data per device, ending at `seed_end_ms`.
async fn seed(pool: &PgPool, o: &Opts, per: u64, seed_end_ms: u64) -> Result<(), String> {
    let stride = (THIRTY_DAYS_MIN / i64::try_from(per).unwrap_or(1)).max(2);
    let per_i = i32::try_from(per).map_err(|_| "too many buckets per device")?;
    let end = i64::try_from(seed_end_ms).map_err(|_| "bad time")?;
    for lo in (0..o.devices).step_by(500) {
        let hi = (lo + 499).min(o.devices - 1);
        let (lo, hi) = (
            i32::try_from(lo).unwrap_or(0),
            i32::try_from(hi).unwrap_or(0),
        );
        let minute = "($5::bigint - (($3::int - m) * $4::bigint * 60000))";
        let window = format!("({minute} - ({minute} % 120000))");
        let statements = [
            format!(
                "INSERT INTO work_buckets (user_id, device_id, project_id, minute_start_ms, ruleset_id,
                    agent_compute_sec, idle_autonomous_sec, human_active_sec, permission_wait_ms,
                    commits_human, commits_agent, injected_hid_detected, auto_approver_suspected,
                    telemetry_tier, tier_limited, axis_steering, axis_specificity, axis_friction,
                    axis_comfort_delta, verified_struggle, struggle_credit_sec)
                 SELECT 'lu'||d, 'ld'||d, 'dir:'||lpad(to_hex(d), 64, '0'), {minute}, 'traction-rules/1',
                        20, 5, 40, 100, 0, 0, false, false, 'full', false, 1, 2, 3, 4, 600, 20
                 FROM generate_series($1::int, $2::int) d CROSS JOIN generate_series(0, $3::int - 1) m"
            ),
            format!(
                "INSERT INTO timeline_marks (user_id, device_id, project_id, minute_start_ms,
                    mutations_manual, mutations_harness, turns_started, tool_failures, harnesses)
                 SELECT 'lu'||d, 'ld'||d, 'dir:'||lpad(to_hex(d), 64, '0'), {minute},
                        1, 2, 1, 0, ARRAY['claude-code']
                 FROM generate_series($1::int, $2::int) d CROSS JOIN generate_series(0, $3::int - 1) m"
            ),
            format!(
                "INSERT INTO activity_rollups (user_id, device_id, window_start_ms, ruleset_id,
                    minutes_observed, share_ide, share_terminal, share_browser, share_ai_desktop,
                    share_other, switch_rate_milli, mean_cadence_entropy_milli, injected_ratio_milli,
                    telemetry_tier)
                 SELECT 'lu'||d, 'ld'||d, {window}, 'traction-rules/1', 2, 400, 300, 100, 100, 100,
                        3, 900, NULL, 'full'
                 FROM generate_series($1::int, $2::int) d CROSS JOIN generate_series(0, $3::int - 1, 2) m
                 ON CONFLICT DO NOTHING"
            ),
            format!(
                "INSERT INTO anchors (device_id, key_id, seq, entry_hash, window_start_ms, sync_mode,
                    sent_at_ms, server_received_us, server_received_at)
                 SELECT 'ld'||d, k.key_id, m / 2, md5(d::text||m::text)||md5(m::text||d::text),
                        {window}, 'realtime', {window} + 120000,
                        ({window} + 150000) * 1000, to_timestamp(({window} + 150000) / 1000.0)
                 FROM generate_series($1::int, $2::int) d
                 JOIN device_keys k ON k.device_id = 'ld'||d
                 CROSS JOIN generate_series(0, $3::int - 1, 2) m"
            ),
        ];
        for statement in &statements {
            sqlx::query(sqlx::AssertSqlSafe(statement.clone()))
                .bind(lo)
                .bind(hi)
                .bind(per_i)
                .bind(stride)
                .bind(end)
                .execute(pool)
                .await
                .map_err(sql)?;
        }
        if lo % 5000 == 0 {
            eprintln!("load: seeded devices up to {hi}");
        }
    }
    Ok(())
}

fn bucket(project: &str, minute: u64) -> WorkBucket {
    WorkBucket {
        project_id: project.to_owned(),
        minute_start_ms: minute,
        ruleset_id: "traction-rules/1".into(),
        agent_compute_sec: 20,
        idle_autonomous_sec: 5,
        human_active_sec: 40,
        permission_wait_ms: 100,
        commits_human: 0,
        commits_agent: 0,
        injected_hid_detected: false,
        auto_approver_suspected: false,
        telemetry_tier: TelemetryTier::Full,
        tier_limited: false,
        struggle_axes: StruggleAxes {
            steering: 1,
            specificity: 2,
            friction: 3,
            comfort_delta: 4,
        },
        verified_struggle: 600,
        struggle_credit_sec: 20,
    }
}

/// A heartbeat of `links` windows starting at `first_window`: 2 Work Buckets,
/// 1 Activity Rollup and 2 Timeline Marks per window, as a real daemon sends.
fn heartbeat(
    key: &DeviceSigningKey,
    device: u64,
    seq: u64,
    first_window: u64,
    links: u64,
    mode: SyncMode,
) -> HeartbeatRequest {
    let project = project_of(device);
    let mut work_buckets = Vec::new();
    let mut activity_rollups = Vec::new();
    let mut timeline_marks = Vec::new();
    for i in 0..links {
        let window = first_window + i * WINDOW_MS;
        for minute in [window, window + 60_000] {
            work_buckets.push(bucket(&project, minute));
            timeline_marks.push(TimelineMark {
                project_id: project.clone(),
                minute_start_ms: minute,
                mutations_manual: 1,
                mutations_harness: 2,
                turns_started: 1,
                tool_failures: 0,
                harnesses: vec![HarnessId::ClaudeCode],
            });
        }
        activity_rollups.push(ActivityRollup {
            window_start_ms: window,
            ruleset_id: "traction-rules/1".into(),
            minutes_observed: 2,
            category_share_milli: Default::default(),
            switch_rate_milli: 3,
            mean_cadence_entropy_milli: Some(900),
            injected_ratio_milli: None,
            telemetry_tier: TelemetryTier::Full,
        });
    }
    HeartbeatRequest {
        tip: ChainTip {
            key_id: key.key_id(),
            seq,
            entry_hash: format!("{:064x}", u128::from(seq) * 7919 + u128::from(device)),
            window_start_ms: first_window + (links - 1) * WINDOW_MS,
        },
        sync_mode: mode,
        sent_at_ms: now_ms(),
        work_buckets,
        activity_rollups,
        projects: vec![ProjectMeta {
            project_id: project,
            project_name: "load-project".into(),
            project_root_path: "/home/load/project".into(),
            project_home_relative_path: Some("~/project".into()),
            os_username: "load".into(),
        }],
        timeline_marks,
        build: None,
    }
}

/// One persistent HTTP/1.1 connection (a real daemon keeps one too).
struct Conn {
    stream: BufReader<TcpStream>,
    host: String,
}

impl Conn {
    async fn open(addr: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream: BufReader::new(stream),
            host: addr.to_owned(),
        })
    }

    async fn post(&mut self, path: &str, token: &str, body: &[u8]) -> std::io::Result<u16> {
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            self.host,
            body.len()
        );
        let stream = self.stream.get_mut();
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        let mut line = String::new();
        self.stream.read_line(&mut line).await?;
        let status: u16 = line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other("bad status line"))?;
        let mut length = 0usize;
        loop {
            line.clear();
            self.stream.read_line(&mut line).await?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut sink = vec![0u8; length];
        self.stream.read_exact(&mut sink).await?;
        Ok(status)
    }
}

#[derive(Default)]
struct Samples {
    /// Paced realtime heartbeats (the Gate 1 population, with the backlog).
    realtime_us: Vec<u64>,
    backlog_us: Vec<u64>,
    /// Rounds where every device sends at the same instant (reported apart).
    burst_us: Vec<u64>,
    errors: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Paced,
    Backlog,
    Burst,
}

/// A deterministic pseudo-random moment inside `interval_ms`.
fn phase(device: u64, round: u64, interval_ms: u64) -> Duration {
    let mixed = device
        .wrapping_mul(2_654_435_761)
        .wrapping_add(round.wrapping_mul(40_503))
        .wrapping_mul(0x9E37_79B9);
    Duration::from_millis((mixed >> 7) % interval_ms)
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1] as f64 / 1000.0
}

#[derive(Clone, Copy)]
struct Plan {
    seq_base: u64,
    backlog_start: u64,
    realtime_start: u64,
    rounds: u64,
    burst_rounds: u64,
    interval_ms: u64,
    backlog_chunks: u64,
    backlog_percent: u64,
}

async fn device_task(
    addr: String,
    device: u64,
    key: Arc<DeviceSigningKey>,
    plan: Plan,
    backlog_barrier: Arc<Barrier>,
    round_barrier: Arc<Barrier>,
) -> Samples {
    let mut samples = Samples::default();
    let token = token_of(device);
    let mut conn = match Conn::open(&addr).await {
        Ok(c) => c,
        Err(e) => {
            *samples.errors.entry(format!("connect: {e}")).or_default() += 1;
            // Keep the barriers whole so the other devices are not stuck.
            backlog_barrier.wait().await;
            for _ in 0..plan.rounds + plan.burst_rounds {
                round_barrier.wait().await;
            }
            return samples;
        }
    };
    let record =
        |samples: &mut Samples, status: std::io::Result<u16>, micros: u64, kind: Kind| match status
        {
            Ok(200) => match kind {
                Kind::Backlog => samples.backlog_us.push(micros),
                Kind::Paced => samples.realtime_us.push(micros),
                Kind::Burst => samples.burst_us.push(micros),
            },
            Ok(code) => *samples.errors.entry(format!("status {code}")).or_default() += 1,
            Err(e) => *samples.errors.entry(format!("io: {e}")).or_default() += 1,
        };
    // Offline backlog: a share of the devices replays chunks of 30 links.
    let has_backlog = device % 100 < plan.backlog_percent;
    backlog_barrier.wait().await;
    if has_backlog {
        tokio::time::sleep(phase(device, 99, plan.interval_ms)).await;
        for chunk in 0..plan.backlog_chunks {
            let first = plan.backlog_start + chunk * LINKS_PER_CHUNK * WINDOW_MS;
            let seq = plan.seq_base + chunk * LINKS_PER_CHUNK + LINKS_PER_CHUNK - 1;
            let request = heartbeat(
                &key,
                device,
                seq,
                first,
                LINKS_PER_CHUNK,
                SyncMode::OfflineBuffered,
            );
            let body = serde_json::to_vec(&request).unwrap_or_default();
            let started = Instant::now();
            let status = conn.post("/v1/anchors", &token, &body).await;
            record(
                &mut samples,
                status,
                started.elapsed().as_micros() as u64,
                Kind::Backlog,
            );
        }
    }
    // Realtime heartbeats. The first rounds are paced: each device sends at
    // its own random moment inside the interval (daemons are not in step).
    // The last rounds are bursts: every device sends at the same instant.
    let rt_seq = plan.seq_base + plan.backlog_chunks * LINKS_PER_CHUNK;
    for round in 0..plan.rounds + plan.burst_rounds {
        round_barrier.wait().await;
        let kind = if round < plan.rounds {
            Kind::Paced
        } else {
            Kind::Burst
        };
        if kind == Kind::Paced {
            tokio::time::sleep(phase(device, round, plan.interval_ms)).await;
        }
        let first = plan.realtime_start + round * WINDOW_MS;
        let request = heartbeat(&key, device, rt_seq + round, first, 1, SyncMode::Realtime);
        let body = serde_json::to_vec(&request).unwrap_or_default();
        let started = Instant::now();
        let status = conn.post("/v1/anchors", &token, &body).await;
        record(
            &mut samples,
            status,
            started.elapsed().as_micros() as u64,
            kind,
        );
    }
    samples
}

fn rss_kb(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn start_server(o: &Opts, db_url: &str, port: u16, dir: &std::path::Path) -> Result<Child, String> {
    let bin = o.server_bin.clone().unwrap_or_else(|| {
        let mut p = std::env::current_exe().unwrap_or_default();
        p.set_file_name(if cfg!(windows) {
            "traction-server.exe"
        } else {
            "traction-server"
        });
        p
    });
    let config = format!(
        "[server]\nbind = \"127.0.0.1:{port}\"\npublic_url = \"http://127.0.0.1:{port}\"\n\
         database_url_env = \"TRACTION_DATABASE_URL\"\n[blob]\nlocal_dir = {:?}\n",
        dir.join("blobs").to_string_lossy()
    );
    let config_path = dir.join("traction-server.toml");
    std::fs::write(&config_path, config).map_err(|e| e.to_string())?;
    Command::new(&bin)
        .arg("--config")
        .arg(&config_path)
        .env("TRACTION_DATABASE_URL", db_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", bin.display()))
}

async fn table_bytes(pool: &PgPool, table: &str) -> Result<i64, String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(SUM(pg_total_relation_size(inhrelid)), 0)::bigint FROM pg_inherits
         WHERE inhparent = '{table}'::regclass"
    )))
    .fetch_one(pool)
    .await
    .map_err(sql)
}

async fn run(o: Opts) -> Result<bool, String> {
    let (prefix, _) = o
        .database_url
        .rsplit_once('/')
        .ok_or("the database URL must end with /<database>")?;
    let name = format!("traction_load_{}", random_hex(6));
    let admin = connect_pool(&o.database_url, 1).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .map_err(sql)?;
    let db_url = format!("{prefix}/{name}");
    let result = run_in(&o, &db_url).await;
    if !o.keep_db {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
    }
    result
}

async fn run_in(o: &Opts, db_url: &str) -> Result<bool, String> {
    let pool = connect_pool(db_url, 8).await?;
    traction_server::migrate(&pool)
        .await
        .map_err(|e| format!("migration: {e}"))?;
    let now = now_ms();
    traction_server::ensure_standard_partitions(&pool, now)
        .await
        .map_err(sql)?;

    // The timeline: seeded month, then the offline backlog, then the bursts.
    let per = o.seed_buckets.div_ceil(o.devices).max(2);
    let seed_anchors = per.div_ceil(2);
    let tail_windows = o.backlog_chunks * LINKS_PER_CHUNK + o.rounds + o.burst_rounds + 2;
    let now_window = now / WINDOW_MS * WINDOW_MS;
    let seed_end = now_window - tail_windows * WINDOW_MS;
    let plan = Plan {
        seq_base: seed_anchors,
        backlog_start: seed_end,
        realtime_start: seed_end + o.backlog_chunks * LINKS_PER_CHUNK * WINDOW_MS,
        rounds: o.rounds,
        burst_rounds: o.burst_rounds,
        interval_ms: o.interval_ms,
        backlog_chunks: o.backlog_chunks,
        backlog_percent: o.backlog_percent,
    };

    let started = Instant::now();
    eprintln!("load: enrolling {} devices", o.devices);
    enroll(&pool, o.devices).await?;
    eprintln!(
        "load: seeding about {} buckets ({per} per device)",
        per * o.devices
    );
    seed(&pool, o, per, seed_end).await?;
    let seeded_buckets: i64 = sqlx::query_scalar("SELECT count(*) FROM work_buckets")
        .fetch_one(&pool)
        .await
        .map_err(sql)?;
    sqlx::query("ANALYZE").execute(&pool).await.map_err(sql)?;
    let seed_secs = started.elapsed().as_secs_f64();

    let dir = tempfile_dir()?;
    let port = free_port()?;
    let mut child = start_server(o, db_url, port, &dir)?;
    let addr = format!("127.0.0.1:{port}");
    let ready = Instant::now();
    loop {
        if TcpStream::connect(&addr).await.is_ok() {
            break;
        }
        if ready.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            return Err("the server did not start".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = child.id();
    let idle_rss_kb = rss_kb(pid).unwrap_or(0);
    let sampler_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let peak_rss = Arc::new(std::sync::atomic::AtomicU64::new(idle_rss_kb));
    let sampler = {
        let (stop, peak) = (Arc::clone(&sampler_stop), Arc::clone(&peak_rss));
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(kb) = rss_kb(pid) {
                    peak.fetch_max(kb, std::sync::atomic::Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    };

    eprintln!(
        "load: {} devices send {} rounds of heartbeats ({}% also replay a backlog)",
        o.peak_devices, o.rounds, o.backlog_percent
    );
    let backlog_barrier = Arc::new(Barrier::new(o.peak_devices as usize));
    let round_barrier = Arc::new(Barrier::new(o.peak_devices as usize));
    let load_started = Instant::now();
    let mut tasks = Vec::new();
    for d in 0..o.peak_devices {
        tasks.push(tokio::spawn(device_task(
            addr.clone(),
            d,
            Arc::new(key_for(d)),
            plan,
            Arc::clone(&backlog_barrier),
            Arc::clone(&round_barrier),
        )));
    }
    let mut all = Samples::default();
    for task in tasks {
        let s = task.await.map_err(|e| e.to_string())?;
        all.realtime_us.extend(s.realtime_us);
        all.backlog_us.extend(s.backlog_us);
        all.burst_us.extend(s.burst_us);
        for (k, v) in s.errors {
            *all.errors.entry(k).or_default() += v;
        }
    }
    let load_secs = load_started.elapsed().as_secs_f64();
    sampler_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sampler.await;
    let rss_mb = peak_rss.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1024.0;
    let _ = child.kill();
    let _ = child.wait();

    let mut combined: Vec<u64> = all
        .realtime_us
        .iter()
        .chain(&all.backlog_us)
        .copied()
        .collect();
    combined.sort_unstable();
    all.realtime_us.sort_unstable();
    all.backlog_us.sort_unstable();
    all.burst_us.sort_unstable();
    let p95 = percentile(&combined, 0.95);
    let errors: u64 = all.errors.values().sum();

    // Bytes per bucket, measured over everything the buckets drag along.
    let mut tables = BTreeMap::new();
    for t in [
        "work_buckets",
        "activity_rollups",
        "anchors",
        "timeline_marks",
    ] {
        tables.insert(t, table_bytes(&pool, t).await?);
    }
    let total: i64 = tables.values().sum();
    let buckets: i64 = sqlx::query_scalar("SELECT count(*) FROM work_buckets")
        .fetch_one(&pool)
        .await
        .map_err(sql)?;
    let bytes_per_bucket = total as f64 / buckets.max(1) as f64;

    let pass = errors == 0 && p95 < o.max_p95_ms && rss_mb < o.max_rss_mb && !combined.is_empty();
    let report = serde_json::json!({
        "gate": "Gate 1",
        "pass": pass,
        "limits": { "max_p95_ms": o.max_p95_ms, "max_rss_mb": o.max_rss_mb },
        "scale": {
            "enrolled_devices": o.devices,
            "peak_concurrent_devices": o.peak_devices,
            "seeded_buckets": seeded_buckets,
            "paced_rounds": o.rounds,
            "burst_rounds": o.burst_rounds,
            "paced_interval_ms": o.interval_ms,
            "offered_paced_requests_per_sec": o.peak_devices as f64 * 1000.0 / o.interval_ms as f64,
            "real_world_peak_requests_per_sec": o.peak_devices as f64 / 120.0,
            "backlog_chunks_per_device": o.backlog_chunks,
            "backlog_device_percent": o.backlog_percent,
            "links_per_backlog_chunk": LINKS_PER_CHUNK,
        },
        "ingest_ms": {
            "requests": combined.len(),
            "p50": percentile(&combined, 0.50),
            "p95": p95,
            "p99": percentile(&combined, 0.99),
            "max": percentile(&combined, 1.0),
            "realtime_p95": percentile(&all.realtime_us, 0.95),
            "backlog_p95": percentile(&all.backlog_us, 0.95),
            "burst_requests": all.burst_us.len(),
            "burst_p95_informational": percentile(&all.burst_us, 0.95),
        },
        "errors": { "count": errors, "by_kind": all.errors },
        "server_rss_mb": { "idle": idle_rss_kb as f64 / 1024.0, "peak": rss_mb },
        "postgres_bytes": {
            "by_table": tables,
            "total": total,
            "buckets": buckets,
            "bytes_per_bucket_all_in": bytes_per_bucket,
        },
        "timing_sec": { "enroll_and_seed": seed_secs, "load": load_secs },
        "environment": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    println!("{text}");
    if let Some(path) = &o.report {
        std::fs::write(path, &text).map_err(|e| format!("cannot write the report: {e}"))?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(pass)
}

fn tempfile_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("traction-load-{}", random_hex(6)));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    let opts = match parse_opts() {
        Ok(o) => o,
        Err(message) => {
            eprintln!("load: {message}");
            return ExitCode::from(2);
        }
    };
    match run(opts).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("load: Gate 1 FAILED (see the report)");
            ExitCode::FAILURE
        }
        Err(message) => {
            eprintln!("load: {message}");
            ExitCode::FAILURE
        }
    }
}
