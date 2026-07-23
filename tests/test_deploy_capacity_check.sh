#!/usr/bin/env bash
set -euo pipefail

script="scripts/check_deploy_capacity.sh"
fixture_dir="$(mktemp -d)"
trap 'rm -rf "$fixture_dir"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

write_kubectl() {
  cat > "$fixture_dir/kubectl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
scenario="\${KUBECTL_SCENARIO:?}"
case "\$scenario:\$*" in
  healthy:get\ nodes\ -o\ json|percent_low:get\ nodes\ -o\ json|disk_pressure:get\ nodes\ -o\ json|disk_unknown:get\ nodes\ -o\ json|malformed:get\ nodes\ -o\ json|capacity_zero:get\ nodes\ -o\ json|imagefs_low:get\ nodes\ -o\ json)
    printf '%s\\n' '{"items":[{"metadata":{"name":"epiphyte"}},{"metadata":{"name":"cordoned"},"spec":{"unschedulable":true}},{"metadata":{"name":"tainted"},"spec":{"taints":[{"key":"maintenance","effect":"NoSchedule"}]}}]}'
    ;;
  multi_later_bad:get\ nodes\ -o\ json)
    printf '%s\\n' '{"items":[{"metadata":{"name":"epiphyte"}},{"metadata":{"name":"later"}}]}'
    ;;
  no_candidates:get\ nodes\ -o\ json)
    printf '%s\\n' '{"items":[{"metadata":{"name":"cordoned"},"spec":{"unschedulable":true}}]}'
    ;;
  kubectl_error:get\ nodes\ -o\ json)
    echo "simulated kubectl failure" >&2
    exit 17
    ;;
  *:get\ node\ epiphyte\ -o\ json)
    case "\$scenario" in
      disk_pressure) printf '%s\\n' '{"status":{"conditions":[{"type":"DiskPressure","status":"True"}]}}' ;;
      disk_unknown) printf '%s\\n' '{"status":{"conditions":[{"type":"Ready","status":"True"}]}}' ;;
      *) printf '%s\\n' '{"status":{"conditions":[{"type":"DiskPressure","status":"False"}]}}' ;;
    esac
    ;;
  multi_later_bad:get\ node\ later\ -o\ json)
    printf '%s\\n' '{"status":{"conditions":[{"type":"DiskPressure","status":"False"}]}}'
    ;;
  *:get\ --raw\ /api/v1/nodes/epiphyte/proxy/stats/summary)
    case "\$scenario" in
      percent_low) printf '%s\\n' '{"node":{"fs":{"availableBytes":11811160064,"capacityBytes":64424509440},"runtime":{"imageFs":{"availableBytes":11811160064,"capacityBytes":64424509440}}}}' ;;
      malformed) printf '%s\\n' '{"node":{"fs":{"availableBytes":"many","capacityBytes":42949672960},"runtime":{"imageFs":{"availableBytes":12884901888,"capacityBytes":42949672960}}}}' ;;
      capacity_zero) printf '%s\\n' '{"node":{"fs":{"availableBytes":12884901888,"capacityBytes":0},"runtime":{"imageFs":{"availableBytes":12884901888,"capacityBytes":42949672960}}}}' ;;
      imagefs_low) printf '%s\\n' '{"node":{"fs":{"availableBytes":12884901888,"capacityBytes":42949672960},"runtime":{"imageFs":{"availableBytes":9663676416,"capacityBytes":42949672960}}}}' ;;
      *) printf '%s\\n' '{"node":{"fs":{"availableBytes":12884901888,"capacityBytes":42949672960},"runtime":{"imageFs":{"availableBytes":12884901888,"capacityBytes":42949672960}}}}' ;;
    esac
    ;;
  multi_later_bad:get\ --raw\ /api/v1/nodes/later/proxy/stats/summary)
    printf '%s\\n' '{"node":{"fs":{"availableBytes":9663676416,"capacityBytes":42949672960},"runtime":{"imageFs":{"availableBytes":12884901888,"capacityBytes":42949672960}}}}'
    ;;
  *)
    echo "unexpected kubectl arguments for \$scenario: \$*" >&2
    exit 2
    ;;
esac
EOF
  chmod +x "$fixture_dir/kubectl"
}

expect_pass() {
  local scenario="$1"
  local output
  if ! output="$(KUBECTL_SCENARIO="$scenario" PATH="$fixture_dir:$PATH" bash "$script" 2>&1)"; then
    fail "$scenario should pass capacity check: $output"
  fi
  grep -Fxq "rollout capacity check passed" <<<"$output" \
    || fail "$scenario did not emit completion sentinel"
}

expect_fail() {
  local scenario="$1"
  local expected="$2"
  local output
  if output="$(KUBECTL_SCENARIO="$scenario" PATH="$fixture_dir:$PATH" bash "$script" 2>&1)"; then
    fail "$scenario was accepted: $output"
  fi
  grep -Fq -- "$expected" <<<"$output" \
    || fail "$scenario did not report '$expected': $output"
}

write_kubectl
expect_pass healthy
expect_fail disk_pressure "DiskPressure=True"
expect_fail disk_unknown "DiskPressure=Unknown"
expect_fail percent_low "only 18% free space on node filesystem"
expect_fail malformed "did not return usable node filesystem capacity"
expect_fail capacity_zero "did not return usable node filesystem capacity"
expect_fail imagefs_low "only 9663676416 free bytes on image filesystem"
expect_fail multi_later_bad "node later has only 9663676416 free bytes"
expect_fail no_candidates "no schedulable nodes"
expect_fail kubectl_error "simulated kubectl failure"

echo "PASS: rollout capacity guard rejects unsafe candidate nodes and bad kubelet data"
