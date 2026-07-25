#!/usr/bin/env bash
# smoke-test.sh — проверка что webhook endpoint работает после deploy
#
# Usage:
#   ./tools/smoke-test.sh http://jenkins-host:8081 [hmac-secret] [bearer-token]
#
# Запускает 5 тестов:
#   1. Health endpoint → 200 OK
#   2. Metrics endpoint → 200 + Prometheus format
#   3. POST без X-Gitea-Event → 400
#   4. POST с неверным HMAC (если secret задан) → 401
#   5. POST с корректным HMAC + push payload → 200 + Java dispatch

set -euo pipefail

WEBHOOK_BASE="${1:?Usage: $0 <base-url> [hmac-secret] [bearer-token]}"
HMAC_SECRET="${2:-}"
BEARER_TOKEN="${3:-}"

# Strip trailing slash
WEBHOOK_BASE="${WEBHOOK_BASE%/}"

PASS=0
FAIL=0
SKIP=0

ok()   { printf "  ✓ %s\n" "$1"; PASS=$((PASS+1)); }
bad()  { printf "  ✗ %s — %s\n" "$1" "$2"; FAIL=$((FAIL+1)); }
skip() { printf "  ⊘ %s — %s\n" "$1" "$2"; SKIP=$((SKIP+1)); }

echo "=== Smoke test: $WEBHOOK_BASE ==="
echo ""

# ---- 1. Health ----
echo "[1/5] Health endpoint"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$WEBHOOK_BASE/health" || echo "000")
if [ "$STATUS" = "200" ]; then
    ok "GET /health → 200"
else
    bad "GET /health" "expected 200, got $STATUS"
fi

# ---- 2. Metrics ----
echo "[2/5] Metrics endpoint"
METRICS_STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$WEBHOOK_BASE/metrics" || echo "000")
if [ "$METRICS_STATUS" = "200" ]; then
    # Check Prometheus format
    BODY=$(curl -s "$WEBHOOK_BASE/metrics")
    if echo "$BODY" | grep -q '^gitea_webhook_requests_total'; then
        ok "GET /metrics → 200 + gitea_webhook_requests_total present"
    else
        bad "GET /metrics" "200 but no gitea_webhook_requests_total in body"
    fi
else
    bad "GET /metrics" "expected 200, got $METRICS_STATUS"
fi

# ---- 3. POST without X-Gitea-Event ----
echo "[3/5] POST without X-Gitea-Event (expect 400 or 401 if auth)"
NO_EVENT_STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$WEBHOOK_BASE/post" \
    -H "Content-Type: application/json" \
    -d '{}' || echo "000")
if [ "$NO_EVENT_STATUS" = "400" ] || [ "$NO_EVENT_STATUS" = "401" ]; then
    ok "POST without X-Gitea-Event → $NO_EVENT_STATUS (rejected)"
else
    bad "POST without X-Gitea-Event" "expected 400 or 401, got $NO_EVENT_STATUS"
fi

# ---- 4. POST with wrong HMAC (only if secret configured) ----
echo "[4/5] POST with wrong HMAC signature"
if [ -n "$HMAC_SECRET" ]; then
    PAYLOAD='{"ref":"refs/heads/main","repository":{"name":"smoke","full_name":"test/smoke","html_url":"https://g/smoke","owner":{"login":"test"}},"sender":{"login":"smoke-test"}}'
    WRONG_SIG=$(echo -n "$PAYLOAD" | openssl dgst -sha256 -hmac "wrong-secret" -hex | awk '{print $NF}')
    AUTH_ARGS=(-H "X-Gitea-Signature: $WRONG_SIG")
    if [ -n "$BEARER_TOKEN" ]; then
        AUTH_ARGS+=(-H "Authorization: Bearer $BEARER_TOKEN")
    fi
    BAD_HMAC_STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
        -X POST "$WEBHOOK_BASE/post" \
        -H "Content-Type: application/json" \
        -H "X-Gitea-Event: push" \
        "${AUTH_ARGS[@]}" \
        -d "$PAYLOAD" || echo "000")
    if [ "$BAD_HMAC_STATUS" = "401" ]; then
        ok "POST with wrong HMAC → 401"
    else
        bad "POST with wrong HMAC" "expected 401, got $BAD_HMAC_STATUS"
    fi
else
    skip "POST with wrong HMAC" "no HMAC secret provided"
fi

# ---- 5. POST with valid HMAC + push payload ----
echo "[5/5] POST with valid HMAC + push event"
PAYLOAD='{"ref":"refs/heads/main","before":"0000000000000000000000000000000000000000","after":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef","repository":{"name":"smoke","full_name":"test/smoke","html_url":"https://gitea.test/test/smoke","owner":{"login":"test"}},"sender":{"login":"smoke-test"}}'

AUTH_ARGS=()
if [ -n "$HMAC_SECRET" ]; then
    SIG=$(echo -n "$PAYLOAD" | openssl dgst -sha256 -hmac "$HMAC_SECRET" -hex | awk '{print $NF}')
    AUTH_ARGS+=(-H "X-Gitea-Signature: $SIG")
fi
if [ -n "$BEARER_TOKEN" ]; then
    AUTH_ARGS+=(-H "Authorization: Bearer $BEARER_TOKEN")
fi

GOOD_STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "$WEBHOOK_BASE/post" \
    -H "Content-Type: application/json" \
    -H "X-Gitea-Event: push" \
    "${AUTH_ARGS[@]}" \
    -d "$PAYLOAD" || echo "000")

if [ "$GOOD_STATUS" = "200" ]; then
    ok "POST valid push event → 200"
else
    bad "POST valid push event" "expected 200, got $GOOD_STATUS"
fi

# ---- Summary ----
echo ""
echo "=== Summary ==="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
echo "  SKIP: $SKIP"
echo ""

if [ "$FAIL" -gt 0 ]; then
    echo "✗ Smoke test FAILED — check Jenkins System Log for details:"
    echo "  Manage Jenkins → System Log → 'Gitea plugin' recorder"
    exit 1
fi

echo "✓ Webhook endpoint healthy. Check Jenkins System Log for handleEvent dispatch."
exit 0
