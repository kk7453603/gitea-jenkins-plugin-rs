#!/usr/bin/env bash
# rollback-to-upstream.sh — откат на upstream jenkinsci/gitea-plugin
#
# Что делает:
#   1. Останавливает Jenkins (operator должен подтвердить)
#   2. Восстанавливает оригинальный .jpi из бэкапа
#   3. Восстанавливает config.xml (если upstream плагин не понимает новые поля)
#   4. Подсказывает обновить Gitea webhook URL обратно

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(dirname "$SCRIPT_DIR")"

: "${JENKINS_HOME:?Must set JENKINS_HOME}"
: "${BACKUP_DIR:?Must set BACKUP_DIR (the one created by migrate-from-upstream.sh)}"

PLUGIN_NAME="gitea"
PLUGIN_PATH="$JENKINS_HOME/plugins/$PLUGIN_NAME.jpi"
PINNED_PATH="$JENKINS_HOME/plugins/$PLUGIN_NAME.jpi.pinned"
CONFIG_PATH="$JENKINS_HOME/config.xml"

if [ ! -d "$BACKUP_DIR" ]; then
    echo "✗ Backup dir does not exist: $BACKUP_DIR"
    echo "  Pass the path created by migrate-from-upstream.sh"
    exit 1
fi

echo "=== Rollback: Rust+JNI fork → upstream ==="
echo "Backup dir: $BACKUP_DIR"
echo ""

# Safety prompt
read -r -p "Jenkins will be stopped and the plugin replaced. Continue? [y/N] " yn
if [ "$yn" != "y" ] && [ "$yn" != "Y" ]; then
    echo "Aborted."
    exit 0
fi

# 1. Stop Jenkins (best-effort — may need sudo)
echo ""
echo "=== Stopping Jenkins ==="
if command -v systemctl >/dev/null 2>&1; then
    sudo systemctl stop jenkins || echo "⚠ Could not stop via systemctl — stop manually"
elif [ -x /etc/init.d/jenkins ]; then
    sudo /etc/init.d/jenkins stop || echo "⚠ Could not stop via init.d"
else
    echo "⚠ No systemctl / init.d — stop Jenkins manually before continuing"
    read -r -p "Press Enter once Jenkins is stopped..."
fi

# 2. Replace plugin
echo ""
echo "=== Restoring upstream .jpi ==="
BACKED_UP_JPI="$BACKUP_DIR/gitea.jpi.bak"
if [ ! -f "$BACKED_UP_JPI" ]; then
    echo "✗ Backup $BACKED_UP_JPI missing"
    exit 1
fi

# Remove pinned file (our fork pins itself)
if [ -f "$PINNED_PATH" ]; then
    sudo rm -f "$PINNED_PATH"
    echo "✓ Removed pinned marker"
fi

sudo cp "$BACKED_UP_JPI" "$PLUGIN_PATH"
sudo chown jenkins:jenkins "$PLUGIN_PATH" 2>/dev/null || \
    sudo chown "$(stat -c '%u:%g' "$JENKINS_HOME")" "$PLUGIN_PATH"
echo "✓ Restored $PLUGIN_PATH"

# 3. Restore config.xml (drop our extra fields — upstream ignores unknown
# fields due to XStream, but rollback to be safe if user added weird values)
echo ""
echo "=== Restoring config.xml ==="
BACKED_UP_CONFIG="$BACKUP_DIR/config.xml.bak"
if [ -f "$BACKED_UP_CONFIG" ]; then
    sudo cp "$BACKED_UP_CONFIG" "$CONFIG_PATH"
    sudo chown jenkins:jenkins "$CONFIG_PATH" 2>/dev/null || \
        sudo chown "$(stat -c '%u:%g' "$JENKINS_HOME")" "$CONFIG_PATH"
    echo "✓ Restored $CONFIG_PATH"
else
    echo "⚠ No config.xml backup — keeping current (upstream may tolerate extra fields)"
fi

# 4. Restart Jenkins
echo ""
echo "=== Starting Jenkins ==="
if command -v systemctl >/dev/null 2>&1; then
    sudo systemctl start jenkins
elif [ -x /etc/init.d/jenkins ]; then
    sudo /etc/init.d/jenkins start
else
    echo "⚠ Start Jenkins manually"
fi

# 5. Tell operator to revert Gitea webhook URL
echo ""
echo "=== Post-rollback actions ==="
echo ""
echo "1. Update Gitea webhook URLs back to upstream format:"
echo "   NEW (was): http://<jenkins>:8081/gitea-webhook/post"
echo "   RESTORED:  http://<jenkins>/gitea-webhook/post"
echo ""
echo "2. Verify in Gitea → repo Settings → Webhooks — Test Delivery button"
echo ""
echo "3. Confirm Jenkins UI shows plugin version matching upstream"
echo "   Manage Jenkins → Plugins → Installed"
echo ""
echo "Rollback complete."
