#!/usr/bin/env bash
# Build the Agent Runtime image and test it against the mock model, then with the model down.
set -euo pipefail

cd "$(dirname "$0")"
compose=(docker compose -f compose.yaml)

cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then
        echo "--- logs ---"
        "${compose[@]}" logs --no-color agent_runtime mock_model | tail -n 80 || true
    fi
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
    exit "$status"
}
trap cleanup EXIT

"${compose[@]}" up --detach --build --wait agent_runtime mock_model

echo "=== backend up"
"${compose[@]}" run --rm tester python test_stack.py up

echo "=== backend down"
"${compose[@]}" stop mock_model
"${compose[@]}" run --rm --no-deps tester python test_stack.py down
