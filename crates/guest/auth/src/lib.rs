//! Shared protocol types for wetware host and guest.
//!
//! Zero-dependency crate usable from both the host binary and WASM guests.

/// Domain separator for challenge-response signing.
///
/// Each signing context gets its own domain so that a signature produced
/// for one purpose (e.g. Terminal login for a Membrane) cannot be replayed
/// in another context (e.g. Terminal login for a Wallet).
///
/// Well-known domains are available via factory methods. User-defined
/// domains can be created with [`SigningDomain::new`].
///
/// # Wire format
///
/// The domain string is carried over Cap'n Proto RPC as `Text` (UTF-8).
/// Use [`SigningDomain::as_str`] to get the wire form.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SigningDomain {
    domain: String,
    payload_type: String,
}

impl SigningDomain {
    /// Create a signing domain.
    ///
    /// `domain` is the libp2p signed-envelope domain string (e.g. `"ww-terminal-membrane"`).
    /// The payload type is derived deterministically as `"/{domain}/challenge"`.
    ///
    /// # Panics
    ///
    /// Panics if `domain` is empty.
    pub fn new(domain: impl Into<String>) -> Self {
        let domain = domain.into();
        assert!(!domain.is_empty(), "signing domain must not be empty");
        let payload_type = format!("/{domain}/challenge");
        Self {
            domain,
            payload_type,
        }
    }

    /// Terminal login guarding a Membrane capability.
    pub fn terminal_membrane() -> Self {
        Self::new("ww-terminal-membrane")
    }

    /// Legacy domain for direct Membrane graft signing (pre-Terminal).
    ///
    /// This wire identifier intentionally retains its historical namespace
    /// across Rust crate renames.
    pub fn membrane_graft() -> Self {
        Self::new("ww-membrane-graft")
    }

    /// The domain string for wire transmission.
    pub fn as_str(&self) -> &str {
        &self.domain
    }

    /// The payload type bytes for the signed envelope.
    pub fn payload_type(&self) -> &[u8] {
        self.payload_type.as_bytes()
    }

    /// Construct the domain-separated signing buffer for the given payload.
    ///
    /// Format follows libp2p signed-envelope (RFC 0002):
    ///
    /// ```text
    /// varint(domain_len) domain varint(payload_type_len) payload_type varint(payload_len) payload
    /// ```
    ///
    /// Both the kernel signer and the host verifier must produce identical
    /// buffers for the same `(domain, payload)` pair.
    pub fn signing_buffer(&self, payload: &[u8]) -> Vec<u8> {
        let domain = self.domain.as_bytes();
        let payload_type = self.payload_type.as_bytes();
        let mut buf = Vec::with_capacity(
            varint_len(domain.len())
                + domain.len()
                + varint_len(payload_type.len())
                + payload_type.len()
                + varint_len(payload.len())
                + payload.len(),
        );
        push_varint(domain.len(), &mut buf);
        buf.extend_from_slice(domain);
        push_varint(payload_type.len(), &mut buf);
        buf.extend_from_slice(payload_type);
        push_varint(payload.len(), &mut buf);
        buf.extend_from_slice(payload);
        buf
    }
}

/// Encode an unsigned integer as a protobuf-style varint (LEB128).
fn push_varint(mut value: usize, buf: &mut Vec<u8>) {
    loop {
        if value < 0x80 {
            buf.push(value as u8);
            break;
        }
        buf.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
}

/// Number of bytes needed for a varint-encoded value.
fn varint_len(value: usize) -> usize {
    let mut v = value;
    let mut len = 1;
    while v >= 0x80 {
        v >>= 7;
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_buffer_terminal_membrane_structure() {
        let nonce: u64 = 0x0102030405060708;
        let buf = SigningDomain::terminal_membrane().signing_buffer(&nonce.to_be_bytes());

        let domain = b"ww-terminal-membrane";
        let payload_type = b"/ww-terminal-membrane/challenge";

        let mut expected = Vec::new();
        push_varint(domain.len(), &mut expected);
        expected.extend_from_slice(domain);
        push_varint(payload_type.len(), &mut expected);
        expected.extend_from_slice(payload_type);
        push_varint(8, &mut expected);
        expected.extend_from_slice(&nonce.to_be_bytes());

        assert_eq!(buf, expected);
    }

    #[test]
    fn signing_buffer_deterministic() {
        let payload = b"test-payload";
        let domain = SigningDomain::terminal_membrane();
        let a = domain.signing_buffer(payload);
        let b = domain.signing_buffer(payload);
        assert_eq!(a, b, "same inputs must produce identical buffers");
    }

    #[test]
    fn different_domains_produce_different_buffers() {
        let payload = b"test-payload";
        let a = SigningDomain::terminal_membrane().signing_buffer(payload);
        let b = SigningDomain::membrane_graft().signing_buffer(payload);
        assert_ne!(a, b, "different domains must produce different buffers");
    }

    #[test]
    fn custom_domain() {
        let domain = SigningDomain::new("ww-terminal-wallet");
        assert_eq!(domain.as_str(), "ww-terminal-wallet");
        assert_eq!(domain.payload_type(), b"/ww-terminal-wallet/challenge");
    }

    #[test]
    #[should_panic(expected = "signing domain must not be empty")]
    fn empty_domain_panics() {
        SigningDomain::new("");
    }

    #[test]
    fn varint_single_byte() {
        let mut buf = Vec::new();
        push_varint(0, &mut buf);
        assert_eq!(buf, vec![0]);

        buf.clear();
        push_varint(127, &mut buf);
        assert_eq!(buf, vec![127]);
    }

    #[test]
    fn varint_multi_byte() {
        let mut buf = Vec::new();
        push_varint(128, &mut buf);
        assert_eq!(buf, vec![0x80, 0x01]);

        buf.clear();
        push_varint(300, &mut buf);
        // 300 = 0b100101100 → 0b0101100 | 0x80, 0b10 → [0xAC, 0x02]
        assert_eq!(buf, vec![0xAC, 0x02]);
    }
}
