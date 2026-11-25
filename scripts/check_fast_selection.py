"""Check the Fast CI selection against Cargo metadata and libtest discovery."""

import argparse
import json
import subprocess
import sys


# Cargo runs all default test targets in each package selected by Fast Rust.
# For the other packages, enumerate the reviewed targets so additions require
# a tier decision. Qualify the target name with its Cargo kind.
DEFERRED_TARGETS = {
    # Chain integration uses Anvil; the library shares its dependency graph.
    "atom": {
        "lib:atom", "test:integration", "test:membrane_integration", "test:reorg_naive",
    },
    # Fast runs the two fixture suites in cell's library. Full supplies the
    # other generated components and Kubo for the complete suite.
    "cell": {"lib:cell", "test:composer_kubo"},
    # Native chess tests link the Host. Full also checks the proof transcript.
    "chess": {"cdylib,rlib:chess", "test:authority_proof"},
    # All of these link the Host. Full supplies fixtures, Kubo and the FHS;
    # libtest leaves the explicitly ignored measurement probes opt-in.
    "ww": {
        "lib:ww", "bin:ww",
        *("test:" + name for name in """
            canonical_package child_authority_confinement cli_daemon_integration
            cli_image_contract cli_image_update cli_lifecycle_contract
            discovery_integration executor_liveness executor_overhead finite_wagi_output
            fuel_overhead gate2b_cell_sessions image_check installation_mutation
            ipfs_integration ipns_kubo_interop kad_integration kernel_preparation
            lifecycle_observed lifecycle_overhead lifecycle_service logging_contract
            logging_output logging_volume metrics_http observability_incident pid0_e2e
            runtime_spike_test status_cell_e2e status_cell_http_listener_e2e
            stdin_shutdown_integration stream_listener_integration worker_liveness_service_overhead
        """.split()),
    },
}

# Standalone generated guests under tests/fixtures. Full builds every fixture.
NATIVE = "p3::tests::p3_"
SESSION = "p3::session_tests::p3_guest_session_"
FIXTURE_TIERS = {
    "native-p3": NATIVE,
    "guest-session-cancel/parent": SESSION,
    "guest-session-cancel/child": SESSION,
    "authority-probe": "full",
    "gate2b-root": "full",
    "routing-key-probe": "full",
    "stream-lifecycle-probe": "full",
}

# These prefixes select every matching ignored case, including future additions.
P3_CASES = {
    NATIVE: {
        "filesystem_matches_cid_tree_policy_and_cleans_up",
        "run_concurrent_owner_abort_drops_transport_without_hang",
        "transport_backpressure_is_bounded_and_ordered",
        "transport_completion_waits_for_underlying_flush",
        "transport_failure_and_single_open_are_structured",
        "transport_ordering_final_write_and_half_close",
    },
    SESSION: {
        "extracted_fields_do_not_detach",
        "run_cancellation",
        "serve_cancellation",
        "unpolled_raw_owner_cancellation",
    },
}

# Requires the packaged kernel and an isolated compiled-component cache.
MANUAL_CELL_CASES = {"proc::tests::measure_packaged_kernel_validation"}


def check_members(label, actual, expected):
    missing = sorted(expected - actual)
    unclassified = sorted(actual - expected)
    if missing or unclassified:
        raise ValueError(f"{label}: missing {missing}, unclassified {unclassified}")


def check_targets(metadata, fast_packages):
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]}
    check_members("workspace packages", set(packages), fast_packages | DEFERRED_TARGETS.keys())
    if fast_packages & DEFERRED_TARGETS.keys():
        raise ValueError("Fast and deferred package classifications overlap")
    for name, package in packages.items():
        targets = [t for t in package["targets"] if t["test"]]
        if name in DEFERRED_TARGETS:
            ids = {f"{','.join(sorted(t['kind']))}:{t['name']}" for t in targets}
            check_members(f"{name} test targets", ids, DEFERRED_TARGETS[name])
        else:
            for target in targets:
                if target.get("required-features"):
                    raise ValueError(f"{name}/{target['name']}: classify required test features")


def check_fixtures(paths):
    check_members("fixture tiers", set(FIXTURE_TIERS.values()), P3_CASES.keys() | {"full"})
    expected = {f"tests/fixtures/{fixture}/Cargo.toml" for fixture in FIXTURE_TIERS}
    check_members("standalone guest manifests", set(paths), expected)


def fixture_selector(manifests):
    # The runner passes the actual manifests it built. Every selected guest
    # must belong to exactly one suite, including both halves of the composition.
    for selector in P3_CASES:
        inputs = {f"tests/fixtures/{name}/Cargo.toml" for name, tier in FIXTURE_TIERS.items() if tier == selector}
        if inputs and inputs == set(manifests):
            return selector
    raise ValueError(f"no Fast P3 selector for fixture manifests {sorted(manifests)}")


def check_p3(names, selector):
    classified = set(MANUAL_CELL_CASES)
    for prefix, required in P3_CASES.items():
        matches = {name for name in names if name.startswith(prefix)}
        if not matches:
            raise ValueError(f"empty libtest selector {prefix}")
        missing = {prefix + suffix for suffix in required} - matches
        if missing:
            raise ValueError(f"missing P3 tests: {sorted(missing)}")
        classified.update(matches)
    check_members("ignored cell library tests", names, classified)
    # libtest uses substring matching. Reject an ambiguous or stale selector.
    if not selector or selector not in P3_CASES or any(selector in n and not n.startswith(selector) for n in names):
        raise ValueError(f"unexpected libtest selector {selector!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("inventory").add_argument("packages", nargs="+")
    commands.add_parser("p3").add_argument("manifests", nargs="+")
    args = parser.parse_args()
    if args.command == "inventory":
        metadata = subprocess.check_output(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"])
        check_targets(json.loads(metadata), set(args.packages))
        fixtures = subprocess.check_output(["git", "ls-files", "-z", "tests/fixtures/*Cargo.toml"])
        check_fixtures(fixtures.decode().rstrip("\0").split("\0"))
    else:
        output = subprocess.check_output(
            ["cargo", "test", "--locked", "-p", "cell", "--lib", "--", "--ignored", "--list"],
            text=True,
        )
        selector = fixture_selector(args.manifests)
        check_p3({line[:-6] for line in output.splitlines() if line.endswith(": test")}, selector)
        print(selector)


if __name__ == "__main__":
    try:
        main()
    except ValueError as error:
        sys.exit(str(error))
