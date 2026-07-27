#[path = "../proof/authority_proof.rs"]
mod authority_proof;

use authority_proof::{
    expected_events, merge_cleanup, render_failure, render_process_result, render_success,
    run_authority_proof, CleanupEvidence, CleanupReport, ProofFailure, ProofOptions, ProofOutcome,
    ProofStage,
};

#[tokio::test]
async fn authority_proof_emits_expected_events() {
    let outcome = run_authority_proof(ProofOptions::default())
        .await
        .expect("real authority proof");

    assert_eq!(outcome.events, expected_events());
    assert!(outcome.cleanup.epoch_advanced);
    assert!(outcome.cleanup.issued_capability_rejected);
    assert_eq!(outcome.cleanup.active_connections, 0);
    assert_eq!(outcome.cleanup.remote_rpc_systems_awaited, 3);
    assert!(outcome.cleanup.server_host_awaited);
    assert!(outcome.cleanup.client_host_awaited);

    let output = render_process_result(Ok(outcome));
    assert_eq!(output.exit_code, 0);
    assert!(output.stderr.is_empty());
    assert!(output.stdout.ends_with("PASS\n"));
}

#[tokio::test]
async fn forced_failure_after_reader_authorization_still_cleans_up() {
    let failure = run_authority_proof(ProofOptions {
        fail_after_reader_authorization: true,
    })
    .await
    .expect_err("injected failure must be returned");

    assert_eq!(failure.stage, ProofStage::InjectedFailure);
    assert_eq!(
        failure.diagnostic,
        "forced failure after Reader authorization"
    );
    assert!(failure.cleanup_diagnostics.is_empty());
    assert!(failure.cleanup.epoch_advanced);
    assert!(failure.cleanup.issued_capability_rejected);
    assert_eq!(failure.cleanup.active_connections, 0);
    assert_eq!(failure.cleanup.remote_rpc_systems_awaited, 2);
    assert!(failure.cleanup.server_host_awaited);
    assert!(failure.cleanup.client_host_awaited);

    let output = render_process_result(Err(failure));
    assert_eq!(output.exit_code, 1);
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        "FAIL injectedFailure forced failure after Reader authorization\n"
    );
}

#[test]
fn cleanup_diagnostics_turn_a_successful_proof_into_a_cleanup_failure() {
    let cleanup = CleanupReport {
        evidence: CleanupEvidence {
            epoch_advanced: true,
            issued_capability_rejected: true,
            active_connections: 1,
            remote_rpc_systems_awaited: 3,
            server_host_awaited: true,
            client_host_awaited: true,
        },
        diagnostics: vec!["connection budget did not drain (active=1)".to_string()],
    };

    let failure = merge_cleanup(Ok(()), expected_events(), cleanup)
        .expect_err("cleanup diagnostics must block a PASS");
    assert_eq!(failure.stage, ProofStage::Cleanup);
    assert_eq!(failure.diagnostic, "observable cleanup failed");
    assert_eq!(failure.cleanup.active_connections, 1);

    let output = render_process_result(Err(failure));
    assert_eq!(output.exit_code, 1);
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        "FAIL cleanup observable cleanup failed; \
         cleanup: connection budget did not drain (active=1)\n"
    );
}

#[test]
fn cleanup_diagnostics_stay_secondary_to_the_original_failure() {
    let original = ProofFailure {
        stage: ProofStage::PlayerWrite,
        diagnostic: "applyMove failed: connection reset".to_string(),
        cleanup: CleanupEvidence::default(),
        cleanup_diagnostics: Vec::new(),
    };
    let cleanup = CleanupReport {
        evidence: CleanupEvidence {
            epoch_advanced: true,
            ..CleanupEvidence::default()
        },
        diagnostics: vec!["timed out awaiting server host task".to_string()],
    };

    let failure = merge_cleanup(Err(original), Vec::new(), cleanup)
        .expect_err("original failure must be returned");
    assert_eq!(failure.stage, ProofStage::PlayerWrite);
    assert_eq!(failure.diagnostic, "applyMove failed: connection reset");
    assert!(failure.cleanup.epoch_advanced);
    assert_eq!(
        failure.cleanup_diagnostics,
        vec!["timed out awaiting server host task".to_string()]
    );

    let output = render_process_result(Err(failure));
    assert_eq!(output.exit_code, 1);
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        "FAIL playerWrite applyMove failed: connection reset; \
         cleanup: timed out awaiting server host task\n"
    );
}

#[test]
fn renderer_requires_the_complete_ordered_evidence_vector() {
    let expected = expected_events();
    let transcript = render_success(&expected).expect("complete evidence renders");
    assert_eq!(
        transcript,
        format!(
            concat!(
                "WETWARE CHESS AUTHORITY PROOF\n",
                "Wetware {}\n",
                "\n",
                "IDENTITIES\n",
                "  Reader  66be7e332c7a…9fb6810c473a\n",
                "  Player  0b513ad9b492…da8eb6e39f2d\n",
                "  Unknown 91a28a0b7438…9b9eba9a4b3a\n",
                "\n",
                "POLICY\n",
                "  Reader  -> getState\n",
                "  Player  -> getState, applyMove\n",
                "  Unknown -> no profile\n",
                "\n",
                "OUTCOMES\n",
                "  Unknown login       DENIED\n",
                "  Reader getState     ALLOWED\n",
                "  Reader applyMove    DENIED: permissionDenied\n",
                "  Player applyMove    ALLOWED: e2e4\n",
                "  Reader getState     ALLOWED: shared board contains e2e4\n",
                "\n",
                "RESULT\n",
                "  Same remote service. Different issued authority.\n",
                "\n",
                "SCOPE\n",
                "This proof controls method calls made through the issued ChessEngine capability.\n",
                "It does not prove the executor lacks ambient credentials, shell access, network egress,\n",
                "alternate APIs, or other bypass paths. It does not enforce per-customer, per-side,\n",
                "per-move, per-argument, or per-resource policy.\n",
                "\n",
                "PASS\n",
            ),
            ww::VERSION
        )
    );
    assert!(!transcript.contains(ww::GIT_COMMIT));

    let partial = render_success(&expected[..6]).expect_err("partial evidence rejected");
    assert_eq!(partial.stage, ProofStage::Transcript);
    assert!(!render_failure(&partial).contains("PASS"));
    let partial_output = render_process_result(Ok(ProofOutcome {
        events: expected[..6].to_vec(),
        cleanup: CleanupEvidence::default(),
    }));
    assert_eq!(partial_output.exit_code, 1);
    assert!(partial_output.stdout.is_empty());
    assert_eq!(
        partial_output.stderr,
        "FAIL transcript incomplete or reordered evidence\n"
    );

    let mut reordered = expected;
    reordered.swap(4, 5);
    let reordered = render_success(&reordered).expect_err("reordered evidence must be rejected");
    assert_eq!(
        render_failure(&reordered),
        "FAIL transcript incomplete or reordered evidence"
    );
}
