# Release rehearsal ownership

Operational rehearsal and consumer qualification moved with the single
`ww-release-package` executable to private `wetware/infra`. The private test suite
retains preparation, signing, finalization, CAR transfer, native consumer and
crash/recovery qualification using disposable keys and a pinned public revision.

Public `release` library tests continue to exercise descriptor authentication,
package integrity and bounded CAR/tree transport. The public installer tests
independently verify OpenSSL signatures, root/manifest binding, platform payloads,
trust rotation and failure before candidate execution. Existing Host installation
and upgrade functionality remains public. External integration harnesses that
accept an explicit `--package-tool` can use the privately built executable;
public CI never fetches private source or credentials.

No rehearsal grants production key provisioning, publication or service authority.
