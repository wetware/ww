# Auth/session schema: Signer, Identity, and Terminal definitions.
#
# Split from stem.capnp to separate authentication/session concerns from
# epoch/provenance and membrane transport metadata.

@0xd0e3f9c78a4b21f1;

interface Signer @0xafafaf9468b6a274 {
  sign @0 (nonce :UInt64, epochSeq :UInt64) -> (sig :Data);
  # Sign a challenge binding (nonce, epochSeq). The signed payload is
  # nonce.to_be_bytes() || epochSeq.to_be_bytes() (16 bytes total).
}

interface Identity @0xa7c200e5b4726d89 {
  # Returns a Signer scoped to the requested signing domain.
  signer @0 (domain :Text) -> (signer :Signer);

  verify @1 (data :Data, signature :Data, pubkey :Data) -> (valid :Bool);
  # Verify an Ed25519 signature against an arbitrary public key.
  # Stateless -- does not use the node's private key.
  # The pubkey is the 32-byte Ed25519 verifying key.
  # The signature is the 64-byte Ed25519 signature.
}

interface Terminal @0xeae8840b2a898ba9 (Session) {
  login @0 (signer :Signer) -> (session :Session);
  # Authenticate via epoch-bound challenge-response. The Terminal generates
  # a random nonce + current epoch seq, the Signer signs both, and the
  # Terminal verifies the signature, nonce, epoch freshness, and auth policy.
  # Having a Terminal reference does NOT grant access -- the caller must prove
  # identity by signing the challenge with the expected key.
}
