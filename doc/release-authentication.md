# Authenticated release manifests

Status: implementation prepared for #412; production activation is **NO-GO**.
No release key has been provisioned by this change. IPFS publication remains
under the unconditional workflow hold. This document grants no operational authority.

## Trust and wire format

The release signing key is an Ed25519 key dedicated to release manifests.
It must be independent of the Kubo `ww-release` IPNS key and Host/RPC identities.
An IPNS publisher, gateway, or release-tree author cannot authorize a new binary
without this release key. IPNS and CIDs locate content; they do not establish
release authority.

`CHECKSUMS.txt.sig` contains exactly 64 raw signature bytes. The signer uses pure
Ed25519 over the exact published `CHECKSUMS.txt` bytes. It does not prehash the
manifest, encode the signature as text, or normalize newlines. There is no
unsigned fallback, including with `--release CID`.

`CHECKSUMS.txt` is at most 1 MiB. Its first line is exactly `# sha256` followed
by LF. Each remaining line contains 64 lowercase hexadecimal SHA-256 digits,
two spaces, a relative artifact path, and LF. At least one entry is required.
Paths contain only ASCII letters, digits, `.`, `_`, `-`, and `/`. Empty, `.` and
`..` components and duplicate paths are invalid. CRLF, blank lines, unknown
headers, mixed algorithms, and BLAKE3 entries are invalid. Signature verification
precedes checksum parsing. A missing required artifact entry is an error.

The independent public keyring contains one or two LF-terminated lines. Each
line is canonical standard base64 of a 44-byte Ed25519 SubjectPublicKeyInfo DER
value: prefix `302a300506032b6570032100`, then the 32-byte public key. Duplicate
keys, extra lines, invalid encodings, and small-order keys are rejected.
The second key exists only for a bounded operator-managed rotation.

The installer reads `/etc/wetware/release-keys` by default. An operator can use
`--trusted-release-keys /absolute/path` for a separately provisioned keyring.
The installer never fetches a keyring from IPFS and never trusts a public key
because it accompanies a signature. Do not point this option at downloaded
release content. A missing local keyring fails closed.

Trust bootstrap requires an independently obtained installer and keyring.
Fetching the installer itself from an attacker-controlled release tree defeats
this boundary. A trusted source checkout or authenticated distribution channel
must supply the installer. The installer requires OpenSSL 3.x or 4.x with
Ed25519 support. Unsupported OpenSSL/LibreSSL fails closed. On macOS, place a
trusted OpenSSL installation on `PATH`; the OS-provided tool might be LibreSSL.

## Consumers and producers

`scripts/install.sh` authenticates the manifest, independently binds it to the
requested release root, then verifies the selected Host, kernel, and status bytes
with SHA-256. All three payloads must pass before the
installer executes the candidate or changes installed state. Existing ownership,
component validation, and recovery checks still apply.

The shell fetches the raw root with `ipfs block get`, hashes it locally, and
compares its canonical CIDv1 DAG-PB SHA-256 identity with `--release`.
A bounded directory parser requires a unique direct `CHECKSUMS.txt` link whose
raw CID equals the authenticated manifest's SHA-256 identity. The current
package producer stores manifests of at most 1 MiB as single raw blocks.
The parser accepts plain directories of at most 2 MiB and 1024 sorted unique
ASCII names. It rejects unknown fields, noncanonical varints, ambiguous links,
CIDv0 roots, and wrapped or chunked manifests. Unsupported formats fail closed;
no downloaded verifier executes as a fallback. Manifest-only verification for
signing remains independent of a release root.

With `--recover`, the same binding check precedes execution of the signed Host.
The shell stages no FHS payloads. The native recovery
entry point independently authenticates the typed release and checks that its
own executable matches a recorded transaction generation before recovery.

`ww upgrade` authenticates descriptor selection and delegates to #739. Native
replacement still refuses until compatibility and startup-admission gates pass;
see [native upgrades](native-upgrades.md). `ww image update` authenticates the
selected FHS and never replaces the native Host or restarts services.

`ww oci import` refuses before download, stdout export, or container loading.
The legacy OCI archive is absent from current packages and had no authentication.
The hidden shell bootstrap handoff requires the staged manifest, signature, and
independent keyring. The native installer authenticates all three payloads again
before component validation. No unsigned fresh-install path is exposed. Exact
recorded-artifact repair does not authorize a different release. Container
registry distribution and deployment-head authority remain separate.

`ww::release::auth::TrustedKeys` accepts separately supplied keyring bytes.
`authenticate` returns an `AuthenticatedManifest` only after strict Ed25519
verification and manifest parsing. `verify_artifact` explicitly uses SHA-256.
#739 must obtain trust independently and use this verification before consuming
or replacing a Host. This change does not implement replacement or rollback.

`scripts/assemble_release.sh` still produces an unsigned staging tree for build
and package validation. A staging tree is not an authorized release.
`scripts/sign_release.sh MANIFEST SIGNATURE PRIVATE_KEY TRUSTED_KEYS` signs the
exact staged manifest and verifies the result against independent trusted keys.
It rejects an existing signature destination and a private key inside the release
tree. `ww-release-package prepare` constructs the binding before signing.
`finalize` authenticates the signed tree before producing the transport root.
The worker verifies against its independently provisioned keyring before local
commitment; the verifier comes from the installed executable, never the release tree.

Official signing admission now belongs to private `wetware/infra`. Its inactive
protected signing job validates the reviewed public artifact handoff before key
access. Public CI receives no release-signing secret. The reusable signing script
checks explicit files and independent trust; it does not allocate CI authority.
Environment protections and signing custody require separate approval.

Private key material exists only in an owner-accessible temporary directory
outside the artifact tree. The step disables tracing, uses `umask 077`, clears
the secret environment variable, and removes the directory on exit. The private
key is never copied to the VPS, Kubo, a release archive, or a build/test job.
Trusted master code and the signing runner can access the key during signing.
A compromised signing runner or authorized signing code remains in the threat
model's trusted computing base. Abrupt runner termination might bypass cleanup;
use ephemeral runners and restrict access to runner storage.

## Provisioning runbook — requires separate approval

1. Approve the release-key custodian, independent public-key distribution channel,
   protected environment, and cutover release. Keep the publication hold in place.
2. Generate a fresh key on the approved signing machine, outside the checkout and
   every artifact directory. Do not reuse an IPNS or Host key:

   ```sh
   umask 077
   openssl genpkey -algorithm ED25519 -out release-signing.pem
   openssl pkey -in release-signing.pem -pubout -outform DER -out release-public.der
   openssl base64 -A -in release-public.der > release-keys
   printf '\n' >> release-keys
   openssl dgst -sha256 release-public.der
   ```

3. Confirm the DER fingerprint through the approved independent channel. Use
   OpenSSL-generated keys, not arbitrary encoded points. Preserve a protected
   recovery copy according to the approved custody policy. Never send the private
   key to a release consumer, publisher host, issue, PR, chat, or build artifact.
4. Provision the public keyring on consumers and the publisher host as
   `/etc/wetware/release-keys`, owned by an administrator and not writable by
   untrusted users. For a user-managed installation, provision a protected local
   path and pass `--trusted-release-keys` explicitly. Releases cannot rotate keys.
5. After separate CI approval in `wetware/infra`, configure environment secret
   `WW_RELEASE_SIGNING_KEY` with the private PEM and environment variable
   `WW_RELEASE_TRUSTED_KEYS` with the public keyring. A final LF is optional in
   the CI variable; the workflow adds one if absent. Restrict `release-signing`
   to protected master and required reviewers. Do not duplicate the private
   secret into repository-wide build or PR contexts.
6. Validate a signed candidate by immutable CID without changing discovery.
   Verify all intended platforms and both fresh installation and recovery.
   Independently compare the published manifest and signature with the signed
   files. Record the source revision, CID, signer fingerprint, and test receipts.
7. Obtain separate approval for publication/cutover. This patch does not remove
   the IPFS hold, fence historical writers, or configure environment protections.
   Apply [the existing publication fence](legacy-publication-hold.md), and use
   #737/#738's publication owner when that integration is ready. Do not activate
   a competing owner or journal from this work.

For offline qualification, run `ww-release-package prepare`, then
`scripts/sign_release.sh MANIFEST SIGNATURE PRIVATE_KEY TRUSTED_KEYS`, followed
by `ww-release-package finalize`. Keep the private PEM outside the release
directory and use independently supplied public trust. These commands do not
announce a release or authorize production publication.

## Rotation, compromise, and unsigned releases

Normal rotation has three explicit states: old key only, old plus new keys,
then new key only. Distribute the new public key through the independent channel
before signing releases with the new private key. Set an operator-approved
cutover date and final old-key removal date. Keep at most two keys throughout.
The wire format permits overlap; it does not enforce calendar deadlines.
An operator who misses key distribution receives a verification error.

After the cutover, remove the old key from consumer keyrings, CI trust, and the
publisher keyring. Remove access to the old private key. A consumer that retains
an old key still accepts old-key signatures. Compromise requires immediate
independent removal and recovery; there is no online revocation service.
Do not let an old-key signature authorize a replacement trust anchor.

Use a producer-first cutover: prepare signed release content, provision trust,
qualify new consumers against its CID, then separately approve discovery and
consumer distribution. Existing SHA-256-only installers ignore the additional
signature file and remain vulnerable; retire them through the trusted installer
distribution channel. New installers reject unsigned historic releases even
when explicitly pinned. Rebuild or separately audit and re-authorize historic
bytes if recovery requires them. Do not add a temporary skip-verification flag.
Until the first signed release and trust distribution are ready, distributing
this new installer as the default would strand users. That cutover is NO-GO.

## #738 descriptor binding

The descriptor uses the existing #412 signer. It adds no signing authority.
The frozen binding order is:

1. Construct immutable artifact graphs and the canonical version-1 descriptor.
   The descriptor contains actual IPLD links and excludes its enclosing root,
   manifest, and signature.
2. Write `RELEASE-DESCRIPTOR` as canonical descriptor CID text plus exactly one
   LF. Add the binding's ordinary SHA-256 entry to `CHECKSUMS.txt`.
3. Sign the exact manifest bytes with the existing Ed25519 procedure.
4. Finalize the legacy-compatible UnixFS transport root. It reaches all payloads,
   manifest, signature, binding, and a direct `release-descriptor` IPLD link.
5. Begin the owner candidate for that final root. Retain complete bounded child
   stages and then the final root. Authenticate before authoritative local
   commitment. Announce through IPNS after that commitment.

`AuthenticatedRelease` verifies the signature, binding bytes, descriptor CID,
schema, and manifest artifact bindings. Selection checks platform and Host/PID0
compatibility. Artifact verification checks exact reconstructed bytes before use.
The shell installer keeps its signed-manifest staging path during migration;
it does not interpret descriptors or persist consumer freshness state. Automatic
discovery is disabled. Recovery delegates typed verification to the staged Host
only after independently verifying root-to-manifest membership and Host bytes.

The Rust consumer records raw signed IPNS evidence and the signed release proof
in one locked durable watermark. IPNS sequence and release sequence are separate
monotonic floors. Historical immutable selection never rewinds that watermark.
The watermark includes the public keyring used at acceptance. Reopening uses
that historical keyring only to verify the persisted floor. Each new discovery
must authenticate with the current independently provisioned keyring. A consumer
offline during key overlap can therefore preserve its floor after the old key
is removed, then accept a higher release signed by the new key. Historical keys
never authorize a new discovery. This relies on the documented private-state
integrity boundary; hostile filesystem writes and rollback are excluded.
A fresh consumer cannot prove latest release discovery through IPNS alone.
See [release-publication.md](release-publication.md) for local commitment,
uncertain delivery, recovery, and the approved divergence from #737's CAS claim.

## Limits and assumptions

Manifest authentication alone does not establish freshness or reproducibility.
The authenticated descriptor additionally binds channel identity and asserted
source provenance. Producer provenance is an authorized assertion, not a
reproducible-build proof. A malicious publisher can replay a previously signed release
or deny service. Rust descriptor consumers enforce the freshness policy above. `VERSION`,
`REVISION`, examples, and installer files are not implicitly authenticated by
payload-only checksum entries. Do not derive authority from those files.

The local OS, installer code, OpenSSL, hashing commands, trust file, and signing
machine are trusted. Concurrent hostile local mutation is outside this boundary.
Root and manifest sizes are checked after download; IPFS transport can consume
disk/time before rejection. Artifact transport retains its existing resource limits.
No claim is made about compromised signing keys, CI administrators, or runners.

OpenSSL performs shell Ed25519 verification. A shared small-order corpus closes
OpenSSL's weak-point acceptance, including signature R points. Rust additionally
rejects non-decodable public points while parsing the whole keyring. Provisioning
uses generated, independently checked keys; malformed non-curve local keyring
entries can cause stricter Rust refusal than OpenSSL during overlap. They do not
let an untrusted publisher authenticate without another valid trusted key.

The actual production environment protections, key custody, cross-platform native
installation, and discovery cutover require operator qualification. Local test
receipts and independent-review results accompany the proposed PR separately.

References: [OpenSSL Ed25519 verification](https://docs.openssl.org/3.0/man1/openssl-pkeyutl/#ed25519-and-ed448-algorithms),
[`ed25519-dalek` strict verification](https://docs.rs/ed25519-dalek/2.2.0/ed25519_dalek/struct.VerifyingKey.html#method.verify_strict).
