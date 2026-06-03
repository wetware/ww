//! JSON Farcaster Signatures (JFS) verification for Snap requests.
//!
//! JFS spec: https://github.com/farcasterxyz/protocol/discussions/208
//! Snap auth: https://docs.farcaster.xyz/snap/auth
//!
//! Wire format is JWT-style compact serialization:
//!
//!     BASE64URL(header) . BASE64URL(payload) . BASE64URL(signature)
//!
//! sent in the `X-Snap-Payload` HTTP header. The signing input is
//!
//!     ASCII(BASE64URL(header) || '.' || BASE64URL(payload))
//!
//! signed with EdDSA. The header carries `{fid, type, key}`; the payload
//! carries `{fid, inputs, audience, timestamp, user, surface}`. Servers
//! MUST reject expired (>5 min skew by default) or audience-mismatched
//! payloads. POST requests REQUIRE a valid header; GET treats it as
//! best-effort viewer identity.
//!
//! What this module does in v1.0:
//!   1. Parse the compact serialization (split on `.`, base64url-decode).
//!   2. Reconstruct the signing input and verify the EdDSA signature
//!      against the embedded `key` (32-byte Ed25519 pubkey, hex).
//!   3. Check the timestamp is within ±5 min of now (configurable).
//!   4. Check the audience matches the server's expected origin.
//!
//! What this module does NOT do in v1.0 (logged as v1.1 follow-up):
//!   - Query a Farcaster Hub to confirm the embedded key is currently
//!     registered to the claimed FID. Without that, an attacker can
//!     sign a payload claiming to be FID 1 with their own keypair and
//!     the signature verifies. The FID is therefore CLAIMED, not
//!     Hub-verified. Cells SHOULD treat the FID as untrusted identity
//!     until v1.1 ships Hub verification.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

/// Default replay window: ±5 minutes per spec.
pub const DEFAULT_TIMESTAMP_SKEW_SECS: i64 = 5 * 60;

/// JFS header (decoded from BASE64URL(header)). Carries the signing key
/// metadata the server uses to verify the signature.
#[derive(Debug, Deserialize)]
struct JfsHeader {
    /// Claimed Farcaster ID of the signer. NOT Hub-verified in v1.0.
    fid: u64,
    /// Key type. Snaps use `app_key` with EdDSA. We accept only this.
    #[serde(rename = "type")]
    key_type: String,
    /// Hex-encoded 32-byte Ed25519 public key.
    key: String,
}

/// JFS payload (decoded from BASE64URL(payload)). What the snap cell
/// receives as verified-context input.
///
/// Field naming matches the spec exactly. Using `serde_json::Value` for
/// `inputs` and `surface` lets us pass them through to cells without
/// constraining their shape.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct JfsPayload {
    /// FID claimed in the payload. MUST match the header's `fid`.
    pub fid: u64,
    /// Form inputs / button-press values from the user.
    #[serde(default)]
    pub inputs: serde_json::Value,
    /// Server origin this payload is intended for (scheme + host + port).
    pub audience: String,
    /// Unix epoch seconds when the payload was signed.
    pub timestamp: i64,
    /// Viewer identity. `{ "fid": <number> }`, possibly with extra fields.
    #[serde(default)]
    pub user: serde_json::Value,
    /// Render context (e.g. `{"type": "standalone"}`).
    #[serde(default)]
    pub surface: serde_json::Value,
}

/// Successful JFS verification result for the Snap example.
#[derive(Debug, Clone)]
pub struct VerifiedJfs {
    pub payload: JfsPayload,
}

/// Split a JFS compact serialization on `.` into its three parts.
///
/// Pure parsing — does no crypto. Separated for unit testability.
fn split_compact(s: &str) -> Result<(&str, &str, &str)> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        bail!(
            "JFS compact serialization must have exactly 3 dot-separated parts, got {}",
            parts.len()
        );
    }
    if parts.iter().any(|p| p.is_empty()) {
        bail!("JFS compact serialization parts must be non-empty");
    }
    Ok((parts[0], parts[1], parts[2]))
}

/// Decode a 32-byte Ed25519 public key from a hex string.
///
/// Accepts an optional `0x` prefix.
fn parse_pubkey(hex_str: &str) -> Result<VerifyingKey> {
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(hex_str).context("pubkey is not valid hex")?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("pubkey must be 32 bytes, got {}", bytes.len()))?;
    VerifyingKey::from_bytes(&arr).context("pubkey bytes are not a valid Ed25519 point")
}

/// Verify a JFS compact-serialized payload from `X-Snap-Payload`.
///
/// Returns the decoded payload + its base64url string for passthrough.
///
/// `expected_audience` is the server origin the request was intended for
/// (e.g. `"https://master.wetware.run"`); audience mismatch is rejected.
/// `now_unix_secs` is the current time; payloads outside `±skew_secs`
/// are rejected. Inject the clock to keep verification testable.
pub fn verify(
    compact: &str,
    expected_audience: &str,
    now_unix_secs: i64,
    skew_secs: i64,
) -> Result<VerifiedJfs> {
    // 1. Split into parts.
    let (header_b64, payload_b64, sig_b64) = split_compact(compact)?;

    // 2. Decode the header. Establishes the verifying key.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .context("header is not valid base64url")?;
    let header: JfsHeader =
        serde_json::from_slice(&header_bytes).context("header is not valid JSON")?;
    if header.key_type != "app_key" {
        bail!(
            "JFS key type must be 'app_key' for snap auth, got {:?}",
            header.key_type
        );
    }
    let pubkey = parse_pubkey(&header.key)?;

    // 3. Decode the signature.
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .context("signature is not valid base64url")?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signature must be 64 bytes, got {}", sig_bytes.len()))?;
    let signature = Signature::from_bytes(&sig_arr);

    // 4. Verify signature over `BASE64URL(header) || '.' || BASE64URL(payload)`.
    //    Per JWS-style compact serialization (and the JFS spec), the
    //    signing input is the ASCII bytes of the b64url header + `.` +
    //    the b64url payload — we DO NOT decode payload before verifying.
    let mut signing_input = Vec::with_capacity(header_b64.len() + 1 + payload_b64.len());
    signing_input.extend_from_slice(header_b64.as_bytes());
    signing_input.push(b'.');
    signing_input.extend_from_slice(payload_b64.as_bytes());
    pubkey
        .verify(&signing_input, &signature)
        .context("JFS signature verification failed")?;

    // 5. Decode the payload (after sig verify, so we don't waste work
    //    on tampered payloads).
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .context("payload is not valid base64url")?;
    let payload: JfsPayload =
        serde_json::from_slice(&payload_bytes).context("payload is not valid JSON")?;

    // 6. Header.fid MUST match payload.fid. Spec consistency check
    //    (also closes a confused-deputy hole where the header claims
    //    one FID but the payload carries another).
    if header.fid != payload.fid {
        bail!(
            "JFS header.fid ({}) does not match payload.fid ({})",
            header.fid,
            payload.fid
        );
    }

    // 7. Audience check: prevents a payload signed for snap A from
    //    being replayed against snap B. Spec MUST.
    if payload.audience != expected_audience {
        bail!(
            "audience mismatch: payload audience {:?} but server expected {:?}",
            payload.audience,
            expected_audience
        );
    }

    // 8. Timestamp window check: replay protection. Spec default 5 min.
    let age = now_unix_secs - payload.timestamp;
    if age.abs() > skew_secs {
        bail!(
            "timestamp outside skew window: payload age {}s exceeds limit {}s",
            age,
            skew_secs
        );
    }

    Ok(VerifiedJfs { payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn b64u(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Generate a fresh signing key for tests. Uses the OS CSPRNG via
    /// `try_fill_bytes` rather than `SigningKey::generate(&mut OsRng)`
    /// because the host workspace pulls multiple `rand_core` versions
    /// and `OsRng` doesn't implement ed25519-dalek's `CryptoRngCore`
    /// directly (same workaround as `src/keys.rs::generate`).
    fn generate_test_key() -> SigningKey {
        use rand::TryRngCore;
        let mut secret_bytes = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut secret_bytes)
            .expect("OS CSPRNG");
        SigningKey::from_bytes(&secret_bytes)
    }

    /// Build a valid JFS compact serialization for tests. Returns
    /// (compact, signing_key, fid, audience, timestamp).
    fn make_jfs(
        fid: u64,
        audience: &str,
        timestamp: i64,
        inputs: serde_json::Value,
    ) -> (String, SigningKey, u64, String, i64) {
        let signing_key = generate_test_key();
        let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());

        let header_json = serde_json::json!({
            "fid": fid,
            "type": "app_key",
            "key": pubkey_hex,
        });
        let payload_json = serde_json::json!({
            "fid": fid,
            "inputs": inputs,
            "audience": audience,
            "timestamp": timestamp,
            "user": { "fid": fid },
            "surface": { "type": "standalone" },
        });

        let header_b64 = b64u(serde_json::to_string(&header_json).unwrap().as_bytes());
        let payload_b64 = b64u(serde_json::to_string(&payload_json).unwrap().as_bytes());
        let signing_input = format!("{header_b64}.{payload_b64}");
        let sig = signing_key.sign(signing_input.as_bytes());
        let sig_b64 = b64u(&sig.to_bytes());

        (
            format!("{header_b64}.{payload_b64}.{sig_b64}"),
            signing_key,
            fid,
            audience.to_string(),
            timestamp,
        )
    }

    #[test]
    fn verify_happy_path() {
        let now = 1_700_000_000;
        let (compact, _sk, fid, audience, _ts) = make_jfs(
            12345,
            "https://snap.example.com",
            now,
            serde_json::json!({}),
        );
        let verified =
            verify(&compact, &audience, now, DEFAULT_TIMESTAMP_SKEW_SECS).expect("verify");
        assert_eq!(verified.payload.fid, fid);
        assert_eq!(verified.payload.audience, audience);
    }

    #[test]
    fn verify_inputs_passed_through() {
        let now = 1_700_000_000;
        let (compact, _, _, audience, _) = make_jfs(
            42,
            "https://x.example",
            now,
            serde_json::json!({"button": "yes", "name": "alice"}),
        );
        let verified = verify(&compact, &audience, now, DEFAULT_TIMESTAMP_SKEW_SECS).unwrap();
        assert_eq!(verified.payload.inputs["button"], "yes");
        assert_eq!(verified.payload.inputs["name"], "alice");
    }

    #[test]
    fn verify_split_rejects_wrong_part_count() {
        let err = verify(
            "onlyone",
            "https://x.example",
            1_700_000_000,
            DEFAULT_TIMESTAMP_SKEW_SECS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("3 dot-separated parts"));
    }

    #[test]
    fn verify_split_rejects_empty_parts() {
        let err = verify(
            "a..c",
            "https://x.example",
            1_700_000_000,
            DEFAULT_TIMESTAMP_SKEW_SECS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn verify_rejects_wrong_audience() {
        let now = 1_700_000_000;
        let (compact, _, _, _, _) =
            make_jfs(1, "https://right.example", now, serde_json::json!({}));
        let err = verify(
            &compact,
            "https://wrong.example",
            now,
            DEFAULT_TIMESTAMP_SKEW_SECS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("audience mismatch"));
    }

    #[test]
    fn verify_rejects_expired() {
        let signed_at = 1_700_000_000;
        let now = signed_at + 600; // 10 min later, > 5 min skew
        let (compact, _, _, audience, _) =
            make_jfs(1, "https://x.example", signed_at, serde_json::json!({}));
        let err = verify(&compact, &audience, now, DEFAULT_TIMESTAMP_SKEW_SECS).unwrap_err();
        assert!(err.to_string().contains("timestamp outside skew window"));
    }

    #[test]
    fn verify_rejects_future_timestamp_outside_skew() {
        // Symmetric: payload from the "future" outside skew is also rejected
        // (clock skew between client and server, or simply a forged
        // timestamp).
        let now = 1_700_000_000;
        let signed_at = now + 600; // 10 min ahead
        let (compact, _, _, audience, _) =
            make_jfs(1, "https://x.example", signed_at, serde_json::json!({}));
        let err = verify(&compact, &audience, now, DEFAULT_TIMESTAMP_SKEW_SECS).unwrap_err();
        assert!(err.to_string().contains("timestamp outside skew window"));
    }

    #[test]
    fn verify_accepts_within_skew_window() {
        let signed_at = 1_700_000_000;
        let now = signed_at + 60; // 1 min later, well within 5 min skew
        let (compact, _, _, audience, _) =
            make_jfs(1, "https://x.example", signed_at, serde_json::json!({}));
        verify(&compact, &audience, now, DEFAULT_TIMESTAMP_SKEW_SECS).expect("within skew");
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        // Build a valid JFS, then swap in a different payload (signed
        // for the original) — signature should fail to verify.
        let now = 1_700_000_000;
        let (compact, _, _, _, _) = make_jfs(1, "https://x.example", now, serde_json::json!({}));
        let parts: Vec<&str> = compact.split('.').collect();
        let evil_payload =
            b64u(br#"{"fid":1,"inputs":{},"audience":"https://x.example","timestamp":1700000000,"user":{"fid":1},"surface":{"type":"standalone"},"injected":"evil"}"#);
        let tampered = format!("{}.{}.{}", parts[0], evil_payload, parts[2]);
        let err = verify(
            &tampered,
            "https://x.example",
            now,
            DEFAULT_TIMESTAMP_SKEW_SECS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("signature verification failed"));
    }

    #[test]
    fn verify_rejects_tampered_header() {
        let now = 1_700_000_000;
        let (compact, _, _, _, _) = make_jfs(1, "https://x.example", now, serde_json::json!({}));
        let parts: Vec<&str> = compact.split('.').collect();
        // Re-encode a header with a different FID but same key.
        let header_decoded = URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
        let mut header: serde_json::Value = serde_json::from_slice(&header_decoded).unwrap();
        header["fid"] = serde_json::json!(99);
        let evil_header = b64u(serde_json::to_string(&header).unwrap().as_bytes());
        let tampered = format!("{}.{}.{}", evil_header, parts[1], parts[2]);
        let err = verify(
            &tampered,
            "https://x.example",
            now,
            DEFAULT_TIMESTAMP_SKEW_SECS,
        )
        .unwrap_err();
        // Tampering the header changes the signing input → signature fails.
        assert!(err.to_string().contains("signature verification failed"));
    }

    #[test]
    fn verify_rejects_header_payload_fid_mismatch() {
        // Forge a JFS where header.fid != payload.fid but signature is
        // valid (signed with our own key over the mismatched pair).
        // Should be rejected on the consistency check at step 6.
        let signing_key = generate_test_key();
        let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());

        let header_json = serde_json::json!({
            "fid": 1,
            "type": "app_key",
            "key": pubkey_hex,
        });
        let payload_json = serde_json::json!({
            "fid": 999,
            "inputs": {},
            "audience": "https://x.example",
            "timestamp": 1_700_000_000,
            "user": {"fid": 999},
            "surface": {"type": "standalone"},
        });
        let header_b64 = b64u(serde_json::to_string(&header_json).unwrap().as_bytes());
        let payload_b64 = b64u(serde_json::to_string(&payload_json).unwrap().as_bytes());
        let signing_input = format!("{header_b64}.{payload_b64}");
        let sig = signing_key.sign(signing_input.as_bytes());
        let sig_b64 = b64u(&sig.to_bytes());
        let compact = format!("{header_b64}.{payload_b64}.{sig_b64}");

        let err = verify(
            &compact,
            "https://x.example",
            1_700_000_000,
            DEFAULT_TIMESTAMP_SKEW_SECS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match payload.fid"));
    }

    #[test]
    fn verify_rejects_non_app_key_type() {
        // Build a header with type="custody" instead of "app_key".
        let signing_key = generate_test_key();
        let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());
        let header_json = serde_json::json!({
            "fid": 1,
            "type": "custody",
            "key": pubkey_hex,
        });
        let payload_json = serde_json::json!({
            "fid": 1, "inputs": {}, "audience": "https://x.example",
            "timestamp": 1_700_000_000, "user": {"fid":1},
            "surface": {"type":"standalone"},
        });
        let h = b64u(serde_json::to_string(&header_json).unwrap().as_bytes());
        let p = b64u(serde_json::to_string(&payload_json).unwrap().as_bytes());
        let sig = signing_key.sign(format!("{h}.{p}").as_bytes());
        let s = b64u(&sig.to_bytes());
        let compact = format!("{h}.{p}.{s}");

        let err = verify(
            &compact,
            "https://x.example",
            1_700_000_000,
            DEFAULT_TIMESTAMP_SKEW_SECS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be 'app_key'"));
    }

    #[test]
    fn parse_pubkey_accepts_0x_prefix() {
        let raw = generate_test_key().verifying_key();
        let with_prefix = format!("0x{}", hex::encode(raw.to_bytes()));
        let parsed = parse_pubkey(&with_prefix).unwrap();
        assert_eq!(parsed.to_bytes(), raw.to_bytes());
    }

    #[test]
    fn parse_pubkey_rejects_wrong_length() {
        let err = parse_pubkey("aabb").unwrap_err();
        assert!(err.to_string().contains("32 bytes"));
    }

    #[test]
    fn split_compact_three_parts() {
        let (a, b, c) = split_compact("aaa.bbb.ccc").unwrap();
        assert_eq!((a, b, c), ("aaa", "bbb", "ccc"));
    }
}
