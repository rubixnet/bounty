//! Release CI helper: `traction-sign-build <version> <git_commit> <target_triple>`
//! prints the signed build statement as one line of JSON. The CI Ed25519 seed
//! (64 hex digits) is read from `TRACTION_CI_SIGNING_SEED`, never from an
//! argument. `traction-sign-build --public-key` prints the matching public
//! key for `[provenance] ci_public_keys`. The JSON is then compiled into the
//! daemon through the `TRACTION_BUILD_STATEMENT` environment variable.

use std::process::ExitCode;

use traction_core::DeviceSigningKey;
use traction_core::sync::SignedBuildStatement;
use zeroize::Zeroizing;

fn seed_from_env() -> Result<Zeroizing<[u8; 32]>, String> {
    let text = std::env::var("TRACTION_CI_SIGNING_SEED")
        .map_err(|_| "TRACTION_CI_SIGNING_SEED is not set".to_owned())?;
    let text = Zeroizing::new(text);
    let bytes = text.trim().as_bytes();
    if bytes.len() != 64 {
        return Err("the seed must be 64 hex digits".into());
    }
    let mut seed = Zeroizing::new([0u8; 32]);
    for (i, pair) in bytes.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).map_err(|_| "the seed is not hex".to_owned())?;
        seed[i] = u8::from_str_radix(pair, 16).map_err(|_| "the seed is not hex".to_owned())?;
    }
    Ok(seed)
}

fn run() -> Result<String, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let key = DeviceSigningKey::from_seed(seed_from_env()?).map_err(|e| e.to_string())?;
    match args.as_slice() {
        [flag] if flag == "--public-key" => Ok(key.verifying_key().to_hex()),
        [version, commit, target] => {
            let statement = SignedBuildStatement::sign(&key, version, commit, target)
                .ok_or("a field is empty, too long or has unsupported characters")?;
            serde_json::to_string(&statement).map_err(|e| e.to_string())
        }
        _ => Err(
            "usage: traction-sign-build <version> <git_commit> <target_triple> | --public-key"
                .into(),
        ),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("traction-sign-build: {message}");
            ExitCode::FAILURE
        }
    }
}
