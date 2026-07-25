#!/usr/bin/env bash
# migrate-from-upstream.sh — переход с jenkinsci/gitea-plugin на наш Rust+JNI fork
#
# Что делает:
#   1. Бэкапит текущий config.xml (GiteaServers settings)
#   2. Бэкапит текущий .jpi
#   3. Печатает diff между upstream и нашим fork (webhook URL и port меняются)
#   4. Генерирует новый webhook URL для регистрации в Gitea
#   5. Выводит чек-лист для operator
#
# НЕ устанавливает плагин сам — это делает operator через Jenkins UI или CLI
# (мы не хотим в автоматизации трогать $JENKINS_HOME/plugins/ без ревью).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

# Defaults — override via env
: "${JENKINS_HOME:?Must set JENKINS_HOME (e.g. /var/lib/jenkins)}"
: "${BACKUP_DIR:=$REPO_DIR/tools/_backup/$(date +%Y%m%d-%H%M%S)}"

PLUGIN_NAME="gitea"
OLD_JPI="$JENKINS_HOME/plugins/$PLUGIN_NAME.jpi"
OLD_CONFIG="$JENKINS_HOME/config.xml"
NEW_HPI="$REPO_DIR/target/gitea.hpi"

echo "=== Gitea plugin migration: upstream → Rust+JNI fork ==="
echo "Jenkins home: $JENKINS_HOME"
echo "Backup dir:   $BACKUP_DIR"
echo ""

mkdir -p "$BACKUP_DIR"

# 1. Backup existing config.xml + .jpi
if [ -f "$OLD_CONFIG" ]; then
    cp "$OLD_CONFIG" "$BACKUP_DIR/config.xml.bak"
    echo "✓ Backed up config.xml → $BACKUP_DIR/config.xml.bak"
    # Extract GiteaServers block for quick diff
    python3 -c "
import xml.etree.ElementTree as ET
import sys
try:
    tree = ET.parse('$BACKUP_DIR/config.xml.bak')
    for srv in tree.findall('.//org.jenkinsci.plugin.gitea.servers.GiteaServer'):
        url = srv.find('serverUrl')
        print(f'  Existing Gitea server: {url.text if url is not None else \"(no URL)\"}')
except Exception as e:
    print(f'  (could not parse config.xml: {e})', file=sys.stderr)
" || true
else
    echo "⚠ No existing config.xml at $OLD_CONFIG — fresh install?"
fi

if [ -f "$OLD_JPI" ]; then
    cp "$OLD_JPI" "$BACKUP_DIR/gitea.jpi.bak"
    echo "✓ Backed up old gitea.jpi → $BACKUP_DIR/gitea.jpi.bak"
    # Try to extract version from old plugin manifest
    if command -v unzip >/dev/null 2>&1; then
        OLD_VER=$(unzip -p "$OLD_JPI" META-INF/MANIFEST.MF 2>/dev/null \
            | grep -i "^Implementation-Version" | tr -d '\r' || echo "unknown")
        echo "  Old plugin version: $OLD_VER"
    fi
else
    echo "⚠ No existing gitea.jpi — first install"
fi

# 2. Build new .hpi if not already present
if [ ! -f "$NEW_HPI" ]; then
    echo ""
    echo "=== Building new gitea.hpi ==="
    cd "$REPO_DIR"
    if command -v cargo >/dev/null 2>&1 && command -v mvn >/dev/null 2>&1; then
        # Build Rust + Java locally
        (cd rust/gitea-client && cargo build --release)
        mvn -B clean package -DskipTests -Dban-junit4-imports.skip=true -Dexec.skip=true
    elif command -v docker >/dev/null 2>&1 && [ -f docker-compose.yml ]; then
        # Fall back to Docker build
        docker compose build
        CONTAINER_ID=$(docker compose ps -q jenkins 2>/dev/null || true)
        if [ -n "$CONTAINER_ID" ]; then
            docker compose cp jenkins:/var/jenkins_home/plugins/gitea.jpi "$NEW_HPI"
        fi
    else
        echo "✗ Cannot build .hpi — neither cargo+mvn nor docker available"
        exit 1
    fi
fi

if [ ! -f "$NEW_HPI" ]; then
    echo "✗ Build failed — $NEW_HPI not produced"
    exit 1
fi
echo "✓ New .hpi ready: $NEW_HPI"

# 3. Print migration checklist for operator
echo ""
echo "=== Migration checklist ==="
echo ""
echo "1. STOP Jenkins:  systemctl stop jenkins"
echo ""
echo "2. INSTALL new plugin (one of):"
echo "   a) UI:   Manage Jenkins → Plugins → Advanced → Upload Plugin →"
echo "            select $NEW_HPI"
echo "   b) CLI:  cp $NEW_HPI $JENKINS_HOME/plugins/gitea.jpi"
echo "            chown jenkins:jenkins $JENKINS_HOME/plugins/gitea.jpi"
echo ""
echo "3. START Jenkins: systemctl start jenkins"
echo ""
echo "4. CONFIGURE Gitea Servers:"
echo "   Manage Jenkins → System → Gitea Servers"
echo "   - Existing servers should be preserved (config.xml back-compat)"
echo "   - NEW fields to fill:"
echo "     • HMAC secret (random 32+ bytes)"
echo "     • Bearer token (optional, defence-in-depth)"
echo "     • Allowed CIDRs (e.g. 10.0.0.0/8)"
echo "     • Trusted PEM (if corp CA)"
echo "     • Webhook path prefix (default /gitea-webhook — change if reverse proxy needs)"
echo ""
echo "5. UPDATE Gitea webhook URL:"
echo "   In Gitea → repo Settings → Webhooks → edit existing webhook:"
echo "   OLD URL: http://<jenkins>/gitea-webhook/post"
echo "   NEW URL: http://<jenkins>:8081/gitea-webhook/post  (or custom port)"
echo ""
echo "   If Jenkins is behind reverse proxy, use the External webhook URL field"
echo "   in Gitea Servers config instead of editing Gitea."
echo ""
echo "6. VERIFY (run smoke test):"
echo "   $REPO_DIR/tools/smoke-test.sh http://<jenkins>:8081 <hmac-secret>"
echo ""
echo "Backup location: $BACKUP_DIR"
echo "Rollback procedure: $REPO_DIR/tools/rollback-to-upstream.sh"
