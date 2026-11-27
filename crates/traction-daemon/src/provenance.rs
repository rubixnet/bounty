//! The CI-signed build statement compiled into release binaries
//! (spec S3b, Q18). Release CI exports `TRACTION_BUILD_STATEMENT` (the JSON
//! printed by `traction-sign-build`) while compiling; a build without it is a
//! self-built binary and sends nothing. The statement is soft provenance: it
//! can be copied into a fork, so the server only tags the device with it.

use traction_core::sync::SignedBuildStatement;

/// Parses the statement text compiled in at build time. A malformed value
/// is treated as absent: a daemon must never fail to start over provenance.
pub(crate) fn parse(text: Option<&str>) -> Option<SignedBuildStatement> {
    serde_json::from_str(text?).ok()
}

/// The statement compiled into this binary, if any.
pub(crate) fn compiled() -> Option<SignedBuildStatement> {
    parse(option_env!("TRACTION_BUILD_STATEMENT"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compiled_statement_parses_and_a_missing_or_malformed_one_is_absent() {
        let text = r#"{"version":"0.1.0","git_commit":"abc","target_triple":"t","signature":"s"}"#;
        let parsed = parse(Some(text)).expect("parses");
        assert_eq!(parsed.version, "0.1.0");
        assert!(parse(None).is_none());
        assert!(parse(Some("not json")).is_none());
        assert!(parse(Some(r#"{"version":"1"}"#)).is_none());
    }
}
