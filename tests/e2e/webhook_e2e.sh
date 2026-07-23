#!/usr/bin/env bash
# E2E test: simulate Gitea -> Rust webhook server -> Jenkins
#
# Requires: a running Jenkins controller with this plugin installed and the
# Rust webhook server started (i.e. the WebhookServerStarter has finished its
# first execute() pass). See docker/README.md for how to bring that up via
# `docker compose up`.
#
# Usage:
#   ./tests/e2e/webhook_e2e.sh [port] [secret]
#
# Env:
#   JENKINS_URL  (default http://localhost:8080)  used only for log messages
#
# Exit codes:
#   0  all assertions passed
#   1  one or more assertions failed (see FAIL lines on stderr)
#   2  prerequisite missing (curl / openssl not found)

set -euo pipefail

PORT="${1:-8081}"
SECRET="${2:-}"
JENKINS_URL="${JENKINS_URL:-http://localhost:8080}"

for dep in curl openssl awk; do
    if ! command -v "$dep" >/dev/null 2>&1; then
        echo "MISSING DEPENDENCY: $dep not on PATH" >&2
        exit 2
    fi
done

# Minimal but valid Gitea push payload (snake_case as Gitea sends it).
# `before` all-zero is the convention Gitea uses for branch creation.
read -r -d '' PUSH_PAYLOAD <<'EOF' || true
{
  "action": "push",
  "ref": "refs/heads/main",
  "before": "0000000000000000000000000000000000000000",
  "after": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
  "repository": {
    "name": "widget",
    "full_name": "acme/widget",
    "html_url": "https://gitea.test/acme/widget",
    "owner": {"login": "acme"}
  },
  "sender": {"login": "alice"}
}
EOF

WEBHOOK_URL="http://localhost:${PORT}/gitea-webhook/post"
PASS=0
FAIL=0

assert_status() {
    local label="$1" actual="$2" expected="$3"
    if [ "$actual" = "$expected" ]; then
        echo "  PASS  $label (HTTP $actual)"
        PASS=$((PASS + 1))
    else
        echo "  FAIL  $label — got $actual, expected $expected" >&2
        FAIL=$((FAIL + 1))
    fi
}

echo "Jenkins:        $JENKINS_URL"
echo "Webhook target: $WEBHOOK_URL"
echo "HMAC secret:    ${SECRET:+<set, ${#SECRET} chars>}${SECRET:-<empty — verification disabled>}"
echo ""

# --- Test 1 ----------------------------------------------------------------
# When the configured secret is empty, the Rust layer skips HMAC verification
# entirely and accepts unsigned requests. This is the developer-mode path.
echo "=== Test 1: push event without HMAC header (secret=${SECRET:+set}${SECRET:-empty}) ==="
if [ -z "$SECRET" ]; then
    RESPONSE=$(curl -s -o /dev/null -w "%{http_code}" \
        -X POST "$WEBHOOK_URL" \
        -H "Content-Type: application/json" \
        -H "X-Gitea-Event: push" \
        -d "$PUSH_PAYLOAD")
    assert_status "unsigned push accepted when secret empty" "$RESPONSE" "200"
else
    echo "  SKIP  secret is set — unsigned request not valid for this config"
fi

# --- Test 2 ----------------------------------------------------------------
# Valid HMAC signature must be accepted, regardless of whether a secret is set.
# When secret is empty the Rust side still computes HMAC against the empty
# key, so the test is meaningful either way.
echo "=== Test 2: push event with valid HMAC signature ==="
SIG=$(printf '%s' "$PUSH_PAYLOAD" \
    | openssl dgst -sha256 -hmac "$SECRET" -hex \
    | awk '{print $NF}')
RESPONSE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$WEBHOOK_URL" \
    -H "Content-Type: application/json" \
    -H "X-Gitea-Event: push" \
    -H "X-Gitea-Signature: $SIG" \
    -d "$PUSH_PAYLOAD")
assert_status "valid HMAC accepted" "$RESPONSE" "200"

# --- Test 3 ----------------------------------------------------------------
# When a secret is configured, a forged signature MUST be rejected with HTTP
# 401. Without this guarantee the webhook layer would be vulnerable to
# payload forgery.
echo "=== Test 3: forged HMAC signature rejected with HTTP 401 ==="
if [ -n "$SECRET" ]; then
    RESPONSE=$(curl -s -o /dev/null -w "%{http_code}" \
        -X POST "$WEBHOOK_URL" \
        -H "Content-Type: application/json" \
        -H "X-Gitea-Event: push" \
        -H "X-Gitea-Signature: deadbeef" \
        -d "$PUSH_PAYLOAD")
    assert_status "forged HMAC rejected" "$RESPONSE" "401"
else
    echo "  SKIP  secret empty — HMAC verification disabled, 401 not enforceable"
fi

# --- Test 4 ----------------------------------------------------------------
# Unknown event types are acknowledged with HTTP 200 so that Gitea does not
# retry them, but they must NOT trigger any Jenkins build. The dispatcher
# logs at FINE only.
echo "=== Test 4: unknown event type acknowledged, no Jenkins action ==="
RESPONSE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$WEBHOOK_URL" \
    -H "X-Gitea-Event: unknown_type" \
    -d '{}')
assert_status "unknown event returns 200 (ignored)" "$RESPONSE" "200"

# --- Test 5 ----------------------------------------------------------------
# A request with no X-Gitea-Event header cannot be dispatched and MUST be
# rejected with HTTP 400.
echo "=== Test 5: missing X-Gitea-Event header rejected with HTTP 400 ==="
RESPONSE=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$WEBHOOK_URL" \
    -d '{}')
assert_status "missing event header rejected" "$RESPONSE" "400"

# --- Summary ---------------------------------------------------------------
echo ""
echo "=== SUMMARY: $PASS passed, $FAIL failed ==="
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
echo "=== ALL E2E TESTS PASSED ==="
