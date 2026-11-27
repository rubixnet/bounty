//! Soft build provenance (spec S3b, Q18): the server checks a daemon's CI-signed
//! build statement against `[provenance] ci_public_keys` and tags the device
//! `ci_signed` or `self_built`. `self_built` sends a submission to a standard
//! spot-check (Yellow), never Red.
//!
//! The tag is soft: a statement can be copied verbatim into a fork's binary,
//! so it is a hint for a human reviewer, not proof of what the daemon runs.

use sqlx::PgConnection;
use traction_core::VerifyingKeyHex;
use traction_core::sync::{BuildTag, SignedBuildStatement, tag_build};

/// What the server records about a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    pub tag: BuildTag,
    pub version: Option<String>,
    pub git_commit: Option<String>,
    pub target: Option<String>,
}

/// Tags `statement` against the CI keys. Only a statement that verifies has
/// its fields recorded: an unverified claim is not stored as if it were true.
pub fn classify(statement: Option<&SignedBuildStatement>, keys: &[VerifyingKeyHex]) -> BuildInfo {
    let tag = tag_build(statement, keys);
    match (tag, statement) {
        (BuildTag::CiSigned, Some(s)) => BuildInfo {
            tag,
            version: Some(s.version.clone()),
            git_commit: Some(s.git_commit.clone()),
            target: Some(s.target_triple.clone()),
        },
        _ => BuildInfo {
            tag,
            version: None,
            git_commit: None,
            target: None,
        },
    }
}

/// Records a device's build (a heartbeat that carried a statement).
pub async fn record_device_build(
    conn: &mut PgConnection,
    device_id: &str,
    info: &BuildInfo,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE devices SET build_tag = $1, build_version = $2, build_git_commit = $3,
                build_target = $4 WHERE id = $5",
    )
    .bind(info.tag.as_str())
    .bind(&info.version)
    .bind(&info.git_commit)
    .bind(&info.target)
    .bind(device_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Whether the device is tagged `self_built`.
pub async fn is_self_built(
    pool: &sqlx::PgPool,
    device_id: &str,
) -> Result<bool, sqlx::Error> {
    let tag: String = sqlx::query_scalar("SELECT build_tag FROM devices WHERE id = $1")
        .bind(device_id)
        .fetch_one(pool)
        .await?;
    Ok(tag == BuildTag::SelfBuilt.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use traction_core::DeviceSigningKey;
    use zeroize::Zeroizing;

    fn key(byte: u8) -> DeviceSigningKey {
        DeviceSigningKey::from_seed(Zeroizing::new([byte; 32])).unwrap()
    }

    #[test]
    fn a_verified_statement_records_its_fields_and_an_unverified_one_records_nothing() {
        let ci = key(7);
        let statement =
            SignedBuildStatement::sign(&ci, "0.4.0", "abc123", "x86_64-unknown-linux-gnu").unwrap();
        let good = classify(Some(&statement), &[ci.verifying_key().clone()]);
        assert_eq!(good.tag, BuildTag::CiSigned);
        assert_eq!(good.version.as_deref(), Some("0.4.0"));
        let copied = classify(Some(&statement), &[key(8).verifying_key().clone()]);
        assert_eq!(copied.tag, BuildTag::SelfBuilt);
        assert_eq!(copied.version, None);
        assert_eq!(classify(None, &[]).tag, BuildTag::SelfBuilt);
    }
}
