#!/usr/bin/env bash
# Tests for deploy/hooks/startup-hook.sh. Linux (GNU coreutils) + jq, which is
# what the image has. Run: bash deploy/hooks/test-startup-hook.sh
# Assertions are single-quoted and eval'd by check(), so they see the values
# set just before them.
# shellcheck disable=SC2016,SC2034
set -euo pipefail

HOOK=$(cd "$(dirname "$0")" && pwd)/startup-hook.sh
PASS=0
FAIL=0
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT

ok() { PASS=$((PASS + 1)); echo "ok   - $1"; }
not_ok() { FAIL=$((FAIL + 1)); echo "FAIL - $1"; }
check() { if eval "$2"; then ok "$1"; else not_ok "$1"; fi; }

# fresh_home [with-kiro] -> sets HOME and BIN
fresh_home() {
    HOME=$(mktemp -d "$ROOT/home.XXXXXX")
    BIN=$(mktemp -d "$ROOT/bin.XXXXXX")
    if [[ ${1:-} == kiro ]]; then
        printf '#!/bin/sh\n' >"$BIN/kiro-cli"
        chmod 755 "$BIN/kiro-cli"
    fi
}
# The runtime clears the environment (runtime/src/hook.rs), so the test does too.
run_hook() {
    local listen=${LISTEN-127.0.0.1:8091}
    if [[ -n $listen ]]; then
        env -i HOME="$HOME" PATH="$BIN:/usr/bin:/bin" OPENAB_PTY_TOOLS_LISTEN="$listen" bash "$HOOK" >/dev/null 2>&1
    else
        env -i HOME="$HOME" PATH="$BIN:/usr/bin:/bin" bash "$HOOK" >/dev/null 2>&1
    fi
}
mcp() { echo "$HOME/.kiro/settings/mcp.json"; }
sum() { sha256sum "$1" | cut -d' ' -f1; }

LISTEN=127.0.0.1:8091

# --- detection -----------------------------------------------------------------
fresh_home
run_hook
check "no kiro-cli: nothing written" '[[ ! -e $HOME/.kiro ]]'

# --- fresh workspace -------------------------------------------------------------
fresh_home kiro
run_hook
check "fresh: mcp.json created" '[[ -f $(mcp) ]]'
check "fresh: constant url" \
    '[[ $(jq -r .mcpServers.computer.url "$(mcp)") == "http://127.0.0.1:8091/mcp" ]]'
check "fresh: header is the literal env reference" \
    "[[ \$(jq -r .mcpServers.computer.headers.Authorization \"\$(mcp)\") == 'Bearer \${OPENAB_TOOLS_MCP_TOKEN}' ]]"
check "fresh: no session name or key on disk" '! grep -q "/mcp/" "$(mcp)"'
check "fresh: no agent file created" '[[ ! -e $HOME/.kiro/agents ]]'
check "fresh: mode 644" '[[ $(stat -c %a "$(mcp)") == 644 ]]'
before=$(sum "$(mcp)")
run_hook
check "idempotent: second run is byte-identical" '[[ $(sum "$(mcp)") == "$before" ]]'

# --- preservation ----------------------------------------------------------------
fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
printf '{"zeta":1,"mcpServers":{"other":{"command":"x"}},"alpha":2}' >"$(mcp)"
chmod 600 "$(mcp)"
run_hook
check "preserve: other server kept" '[[ $(jq -r .mcpServers.other.command "$(mcp)") == x ]]'
check "preserve: other settings kept" '[[ $(jq -c "[.zeta,.alpha]" "$(mcp)") == "[1,2]" ]]'
check "preserve: key order kept" '[[ $(jq -c keys_unsorted "$(mcp)") == "[\"zeta\",\"mcpServers\",\"alpha\"]" ]]'
check "preserve: mode kept" '[[ $(stat -c %a "$(mcp)") == 600 ]]'

# A stale hand-written per-session entry is replaced by the session-independent one.
fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo '{"mcpServers":{"computer":{"url":"http://127.0.0.1:8091/mcp/mac/deadbeef"}}}' >"$(mcp)"
run_hook
check "stale per-session url replaced" '[[ $(jq -r .mcpServers.computer.url "$(mcp)") == "http://127.0.0.1:8091/mcp" ]]'

# --- files left alone -------------------------------------------------------------
for body in 'not json' '[1,2]' '{"mcpServers":"oops"}'; do
    fresh_home kiro
    mkdir -p "$HOME/.kiro/settings"
    printf '%s' "$body" >"$(mcp)"
    before=$(sum "$(mcp)")
    run_hook
    check "left byte-identical: $body" '[[ $(sum "$(mcp)") == "$before" ]]'
done

# --- symlinks ---------------------------------------------------------------------
fresh_home kiro
mkdir -p "$HOME/.kiro/settings" "$HOME/dotfiles"
echo '{}' >"$HOME/dotfiles/mcp.json"
ln -s "$HOME/dotfiles/mcp.json" "$(mcp)"
run_hook
check "symlink: link kept" '[[ -L $(mcp) ]]'
check "symlink: target written" '[[ $(jq -r .mcpServers.computer.url "$HOME/dotfiles/mcp.json") == "http://127.0.0.1:8091/mcp" ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
ln -s "$HOME/nowhere.json" "$(mcp)"
run_hook
check "dangling symlink: left alone" '[[ -L $(mcp) && ! -e $HOME/nowhere.json ]]'

# --- agent trust ------------------------------------------------------------------
fresh_home kiro
mkdir -p "$HOME/.kiro/agents"
echo '{"name":"a","allowedTools":["fs_read"],"tools":["fs_read"]}' >"$HOME/.kiro/agents/a.json"
echo '{"name":"b"}' >"$HOME/.kiro/agents/b.json"
echo '{"name":"c","allowedTools":["@computer"]}' >"$HOME/.kiro/agents/c.json"
echo '{"name":"d","allowedTools":"*"}' >"$HOME/.kiro/agents/d.json"
echo 'broken' >"$HOME/.kiro/agents/e.json"
c_before=$(sum "$HOME/.kiro/agents/c.json")
d_before=$(sum "$HOME/.kiro/agents/d.json")
e_before=$(sum "$HOME/.kiro/agents/e.json")
run_hook
check "agent: trust appended" '[[ $(jq -c .allowedTools "$HOME/.kiro/agents/a.json") == "[\"fs_read\",\"@computer/*\"]" ]]'
check "agent: restrictive tools list untouched" '[[ $(jq -c .tools "$HOME/.kiro/agents/a.json") == "[\"fs_read\"]" ]]'
check "agent: absent allowedTools gains trust" '[[ $(jq -c .allowedTools "$HOME/.kiro/agents/b.json") == "[\"@computer/*\"]" ]]'
check "agent: already trusted unchanged" '[[ $(sum "$HOME/.kiro/agents/c.json") == "$c_before" ]]'
check "agent: non-array allowedTools unchanged" '[[ $(sum "$HOME/.kiro/agents/d.json") == "$d_before" ]]'
check "agent: malformed unchanged" '[[ $(sum "$HOME/.kiro/agents/e.json") == "$e_before" ]]'

# --- tools plane off --------------------------------------------------------------
fresh_home kiro
run_hook
LISTEN='' run_hook
check "off: our entry withdrawn" '[[ $(jq -c ".mcpServers | has(\"computer\")" "$(mcp)") == false ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo '{"mcpServers":{"computer":{"command":"mine"}}}' >"$(mcp)"
before=$(sum "$(mcp)")
LISTEN='' run_hook
check "off: a user's own computer entry is kept" '[[ $(sum "$(mcp)") == "$before" ]]'

fresh_home kiro
LISTEN='' run_hook
check "off: nothing created" '[[ ! -e $HOME/.kiro ]]'

# --- never fails ------------------------------------------------------------------
fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo '{}' >"$(mcp)"
chmod 555 "$HOME/.kiro/settings"
if run_hook; then
    ok "unwritable dir: exit 0"
else
    not_ok "unwritable dir: exit 0"
fi
chmod 755 "$HOME/.kiro/settings"

# --- review round 1 ---------------------------------------------------------------
# A planted ~/.jq must not change what the hook does (HOME is the shared workspace).
fresh_home kiro
printf 'def has(k): error("pwned");\n' >"$HOME/.jq"
run_hook
check "planted .jq: ignored, entry still written" '[[ $(jq -r .mcpServers.computer.url "$(mcp)") == "http://127.0.0.1:8091/mcp" ]]'

# Tools on: a user's own `computer` server is left alone.
fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo '{"mcpServers":{"computer":{"command":"mine","disabled":false}}}' >"$(mcp)"
before=$(sum "$(mcp)")
run_hook
check "on: a user's own computer entry is kept" '[[ $(sum "$(mcp)") == "$before" ]]'

# ...and so is one on a different listener.
fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo '{"mcpServers":{"computer":{"url":"http://127.0.0.1:9999/mcp/x/y"}}}' >"$(mcp)"
before=$(sum "$(mcp)")
run_hook
check "on: a computer url on another listener is kept" '[[ $(sum "$(mcp)") == "$before" ]]'

# Our own entry from an earlier boot on a different port is refreshed.
fresh_home kiro
LISTEN=127.0.0.1:7000 run_hook
run_hook
check "on: our entry follows a changed port" '[[ $(jq -r .mcpServers.computer.url "$(mcp)") == "http://127.0.0.1:8091/mcp" ]]'

fresh_home kiro
LISTEN='[::1]:8091' run_hook
check "ipv6 listener: bracketed url" '[[ $(jq -r .mcpServers.computer.url "$(mcp)") == "http://[::1]:8091/mcp" ]]'

fresh_home kiro
mkdir -p "$(mcp)"
run_hook
check "mcp.json is a directory: left alone, no stray temp" '[[ -d $(mcp) && -z $(ls -A "$(mcp)") ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
printf '{} {}' >"$(mcp)"
before=$(sum "$(mcp)")
run_hook
check "multi-document file: left byte-identical" '[[ $(sum "$(mcp)") == "$before" ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/agents" "$HOME/dotfiles"
echo '{"name":"s"}' >"$HOME/dotfiles/s.json"
ln -s "$HOME/dotfiles/s.json" "$HOME/.kiro/agents/s.json"
echo '{"name":"star","allowedTools":["*"]}' >"$HOME/.kiro/agents/star.json"
star_before=$(sum "$HOME/.kiro/agents/star.json")
run_hook
check "agent symlink: link kept, target trusted" '[[ -L $HOME/.kiro/agents/s.json && $(jq -c .allowedTools "$HOME/dotfiles/s.json") == "[\"@computer/*\"]" ]]'
check "agent with allowedTools [*]: unchanged" '[[ $(sum "$HOME/.kiro/agents/star.json") == "$star_before" ]]'

fresh_home kiro
if env -i PATH="$BIN:/usr/bin:/bin" OPENAB_PTY_TOOLS_LISTEN=127.0.0.1:8091 bash "$HOOK" >/dev/null 2>&1; then
    ok "HOME unset: exit 0"
else
    not_ok "HOME unset: exit 0"
fi

echo "# $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
