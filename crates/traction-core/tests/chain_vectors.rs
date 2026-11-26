//! Golden vectors: fixed seeds and a fixed two-Project, two-window payload
//! must give byte-identical hex on Windows, macOS and Linux. The server
//! (Step 4) recomputes these digests, so a change here is a format break.
//!
//! The first link's `payload_digest`, `entry_hash`, `key_id`, public key and
//! signature were cross-checked against an independent implementation
//! (Python `hashlib` + `cryptography`) when these constants were committed.

mod chain_fixture;

use chain_fixture::*;

/// Seed `[7; 32]`: windows 0 (alpha + beta) and 1 (alpha). Then a successor
/// chain from seed `[9; 32]` with `supersedes`, window 2.
const VECTORS: &str = "\
pk7 ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c
key_id7 ed25519:56b9ddbf936060e39c80ba8632696e39
key_id9 ed25519:ffd9e6003abadf97388c914695ba703b
c0.0 device_rows_digest 09308c8276459b6939b8d2f27641878326a69aa57795d58a6594e1eae9fa0ba2
c0.0 rows_digest git:alpha c569143f6dd998f524d0e02d5c64f4e3a5fbea9bd1f0ceec2895a1c4acbaa24e
c0.0 rows_digest git:beta d32c7a5b6cb5d98475522418124584ea1647efa9f1e2ce92838e09d211bd5cc7
c0.0 payload_digest e9f927446b3846e3f29991f250182dc7c7eeafa7df6d43d1808c2ac13905fefc
c0.0 entry_hash 1c1f636e8ce954920c3faaf1fccf361c618b3ac8e00de003682662c3c5f44f5e
c0.0 signature 66d4d52ad8dbdfc72179cdbfa6fcafb89c327b683bbc91915eaed6c676a74f9997d9d99fd227bee1d9e71a1213588ec327fbf858371251ee281350c6fd78b806
c0.1 device_rows_digest 2bf523d83359613a97be429c9c666b60a848408b366601dbfba72387120239be
c0.1 rows_digest git:alpha ca5c735001229471bbe9a9c9bebdb6b204c5b78b561e04bc77f0888bd89d11f6
c0.1 payload_digest a18cdd94cb215e9cc8f5bf0caad84211a575e8ae018b3c003277e038bd10964c
c0.1 entry_hash dc258888450ec6952e32f74c8ebfbbc6f7c8e8ce6683bd65f1033236319ca701
c0.1 signature 872fc2e4be2617affb3ab43de7dc3214c7e99531905c972b523c4a07c6ee7005483a103acae3894f68e3d54e4e61f22d9f23ffda2b81fd7150b556dac0eb2a08
c1.0 device_rows_digest 4a51be0a88a1729b72477a218565f7c52e49283fd96371fc7df055fb74cb76b3
c1.0 rows_digest git:alpha 928cc4c4f61b34d0034630fb9632b374c9ef9d65ffaa4ba93b2c7ca560f2ad8b
c1.0 rows_digest git:beta 40f99c53d165c6436a9928f88e8629144d5387cd0c4ee2e99167854c011ef741
c1.0 payload_digest d9cd9a32ab88c39998d081e11cd63152a8a8666c5f24262e1026de2a389dfd9a
c1.0 entry_hash 31d78963484ca2f2454ec04490cab32de9910d59d61e83753d7868830ac6fb78
c1.0 signature 4b5d02adf5582337ed570672a60caa2be4397276fbf57b02c3d48df245ac99d864bd1a3e6232430e10c54820dbc7c73d0b86ab32727e44dd232a0f7727158a03
";

fn vectors() -> String {
    let fix = build(&[(7, 2), (9, 1)]);
    let mut out = format!("pk7 {}\n", fix.keys[0].to_hex());
    out.push_str(&format!("key_id7 {}\n", fix.keys[0].key_id()));
    out.push_str(&format!("key_id9 {}\n", fix.keys[1].key_id()));
    for (c, chain) in fix.chains.iter().enumerate() {
        for link in chain {
            let n = link.header.seq;
            let p = &link.payload;
            out.push_str(&format!(
                "c{c}.{n} device_rows_digest {}\n",
                p.device_rows_digest
            ));
            for e in &p.projects {
                out.push_str(&format!(
                    "c{c}.{n} rows_digest {} {}\n",
                    e.project_id, e.rows_digest
                ));
            }
            out.push_str(&format!(
                "c{c}.{n} payload_digest {}\n",
                link.header.payload_digest
            ));
            out.push_str(&format!("c{c}.{n} entry_hash {}\n", link.entry_hash));
            out.push_str(&format!("c{c}.{n} signature {}\n", link.signature));
        }
    }
    out
}

#[test]
fn golden_vectors_are_byte_identical_on_every_os() {
    assert_eq!(vectors(), VECTORS);
}

#[test]
fn the_successor_genesis_commits_to_the_previous_tip() {
    let fix = build(&[(7, 2), (9, 1)]);
    let s = fix.chains[1][0]
        .payload
        .supersedes
        .as_ref()
        .expect("supersedes");
    assert_eq!(
        s.key_id.as_str(),
        "ed25519:56b9ddbf936060e39c80ba8632696e39"
    );
    assert_eq!(s.seq, 1);
    assert_eq!(
        s.entry_hash,
        "dc258888450ec6952e32f74c8ebfbbc6f7c8e8ce6683bd65f1033236319ca701"
    );
}
