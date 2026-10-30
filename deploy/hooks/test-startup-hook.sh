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
# The runtime clears the environment (runtime/src/hook.rs), so the test does
# too. The hook's stdout and stderr land in $OUT.
run_hook() {
    local listen=${LISTEN-127.0.0.1:8091}
    OUT=$ROOT/out
    if [[ -n $listen ]]; then
        env -i HOME="$HOME" PATH="$BIN:/usr/bin:/bin" OPENAB_PTY_TOOLS_LISTEN="$listen" bash "$HOOK" >"$OUT" 2>&1
    else
        env -i HOME="$HOME" PATH="$BIN:/usr/bin:/bin" bash "$HOOK" >"$OUT" 2>&1
    fi
}
mcp() { echo "$HOME/.kiro/settings/mcp.json"; }
agent() { echo "$HOME/.kiro/agents/openab-terminal.json"; }
cli() { echo "$HOME/.kiro/settings/cli.json"; }
default_agent() { jq -r '.["chat.defaultAgent"]' "$(cli)"; }
sum() { sha256sum "$1" | cut -d' ' -f1; }
# The entry the pre-#76 hook wrote into mcp.json.
OLD_ENTRY='{"url":"http://127.0.0.1:8091/mcp","headers":{"Authorization":"Bearer ${OPENAB_TOOLS_MCP_TOKEN}"}}'

LISTEN=127.0.0.1:8091

# --- detection -----------------------------------------------------------------
fresh_home
run_hook
check "no kiro-cli: nothing written" '[[ ! -e $HOME/.kiro ]]'

# --- fresh workspace -------------------------------------------------------------
fresh_home kiro
run_hook
check "fresh: terminal agent created" '[[ -f $(agent) ]]'
check "fresh: agent name" '[[ $(jq -r .name "$(agent)") == openab-terminal ]]'
check "fresh: agent includes mcp.json" '[[ $(jq -r .includeMcpJson "$(agent)") == true ]]'
check "fresh: agent tools [*]" '[[ $(jq -c .tools "$(agent)") == "[\"*\"]" ]]'
check "fresh: agent trusts computer" '[[ $(jq -c .allowedTools "$(agent)") == "[\"@computer/*\"]" ]]'
check "fresh: agent loads the user's skills" \
    '[[ $(jq -r ".resources | index(\"skill://$HOME/.kiro/skills/*/SKILL.md\") != null" "$(agent)") == true ]]'
check "fresh: constant url" \
    '[[ $(jq -r .mcpServers.computer.url "$(agent)") == "http://127.0.0.1:8091/mcp" ]]'
check "fresh: header is the literal env reference" \
    "[[ \$(jq -r .mcpServers.computer.headers.Authorization \"\$(agent)\") == 'Bearer \${OPENAB_TOOLS_MCP_TOKEN}' ]]"
check "fresh: no session name or key on disk" '! grep -rq "/mcp/" "$HOME/.kiro"'
check "fresh: mcp.json not created" '[[ ! -e $(mcp) ]]'
check "fresh: terminal agent is the default" '[[ $(default_agent) == openab-terminal ]]'
check "fresh: only the hook's own agent file" '[[ $(ls "$HOME/.kiro/agents") == openab-terminal.json ]]'
check "fresh: mode 644" '[[ $(stat -c %a "$(agent)") == 644 ]]'
before_agent=$(sum "$(agent)")
before_cli=$(sum "$(cli)")
run_hook
check "idempotent: agent byte-identical" '[[ $(sum "$(agent)") == "$before_agent" ]]'
check "idempotent: cli.json byte-identical" '[[ $(sum "$(cli)") == "$before_cli" ]]'
check "idempotent: second run writes nothing" '[[ ! -s $OUT ]]'
check "idempotent: still no mcp.json" '[[ ! -e $(mcp) ]]'

# --- upgrade from the mcp.json form ------------------------------------------------
fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
printf '{"zeta":1,"mcpServers":{"other":{"command":"x"},"computer":%s},"alpha":2}' "$OLD_ENTRY" >"$(mcp)"
chmod 600 "$(mcp)"
run_hook
check "upgrade: old computer entry removed" '[[ $(jq -c ".mcpServers | has(\"computer\")" "$(mcp)") == false ]]'
check "upgrade: user server kept" '[[ $(jq -r .mcpServers.other.command "$(mcp)") == x ]]'
check "upgrade: other settings kept" '[[ $(jq -c "[.zeta,.alpha]" "$(mcp)") == "[1,2]" ]]'
check "upgrade: key order kept" '[[ $(jq -c keys_unsorted "$(mcp)") == "[\"zeta\",\"mcpServers\",\"alpha\"]" ]]'
check "upgrade: mode kept" '[[ $(stat -c %a "$(mcp)") == 600 ]]'
check "upgrade: computer now in the terminal agent" \
    '[[ $(jq -r .mcpServers.computer.url "$(agent)") == "http://127.0.0.1:8091/mcp" ]]'
before=$(sum "$(mcp)")
run_hook
check "upgrade: second run leaves mcp.json byte-identical" '[[ $(sum "$(mcp)") == "$before" ]]'

# An old entry from a boot on another port is the hook's too.
fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo '{"mcpServers":{"computer":{"url":"http://127.0.0.1:7000/mcp","headers":{"Authorization":"Bearer ${OPENAB_TOOLS_MCP_TOKEN}"}}}}' >"$(mcp)"
run_hook
check "upgrade: old entry on another port removed" '[[ $(jq -c .mcpServers "$(mcp)") == "{}" ]]'

# Anything that is not exactly the hook's shape is the user's.
for entry in \
    '{"url":"http://127.0.0.1:8091/mcp/mac/deadbeef"}' \
    '{"url":"http://127.0.0.1:8091/mcp","headers":{"Authorization":"Bearer ${OPENAB_TOOLS_MCP_TOKEN}"},"disabled":true}' \
    '{"url":"http://127.0.0.1:8091/mcp","headers":{"Authorization":"Bearer mine"}}' \
    '{"command":"mine"}'; do
    fresh_home kiro
    mkdir -p "$HOME/.kiro/settings"
    printf '{"mcpServers":{"computer":%s}}' "$entry" >"$(mcp)"
    before=$(sum "$(mcp)")
    run_hook
    check "upgrade: user's computer entry kept: $entry" '[[ $(sum "$(mcp)") == "$before" ]]'
done

# --- other agents are not touched ---------------------------------------------------
fresh_home kiro
mkdir -p "$HOME/.kiro/agents"
echo '{"name":"a","allowedTools":["fs_read"],"tools":["fs_read"]}' >"$HOME/.kiro/agents/a.json"
echo '{"name":"b"}' >"$HOME/.kiro/agents/b.json"
# Trust an earlier hook appended looks exactly like a user's own: it stays.
echo '{"name":"c","allowedTools":["fs_read","@computer/*"]}' >"$HOME/.kiro/agents/c.json"
echo 'broken' >"$HOME/.kiro/agents/e.json"
for f in a b c e; do eval "before_$f=\$(sum \"\$HOME/.kiro/agents/$f.json\")"; done
run_hook
for f in a b c e; do
    eval "b=\$before_$f"
    check "other agent $f.json byte-identical" '[[ $(sum "$HOME/.kiro/agents/'"$f"'.json") == "$b" ]]'
done
check "other agents: no @computer/* added anywhere but openab-terminal" \
    '[[ -z $(grep -l "@computer" "$HOME"/.kiro/agents/*.json | grep -v -e openab-terminal.json -e /c.json) ]]'

# --- default agent --------------------------------------------------------------------
for user_default in mine kiro_default; do
    fresh_home kiro
    mkdir -p "$HOME/.kiro/settings"
    printf '{\n  "chat.defaultAgent": "%s"\n}\n' "$user_default" >"$(cli)"
    before=$(sum "$(cli)")
    run_hook
    check "user default $user_default: left unchanged" '[[ $(sum "$(cli)") == "$before" ]]'
    check "user default $user_default: one notice line" \
        '[[ $(grep -c "left unchanged" "$OUT") == 1 && $(grep "left unchanged" "$OUT") == *"--agent openab-terminal"* ]]'
    check "user default $user_default: terminal agent still written" '[[ -f $(agent) ]]'
done

fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo '{"chat.editMode":"vi","chat.defaultAgent":""}' >"$(cli)"
run_hook
check "cli.json: empty default set" '[[ $(default_agent) == openab-terminal ]]'
check "cli.json: other settings kept" '[[ $(jq -r ".[\"chat.editMode\"]" "$(cli)") == vi ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
printf 'not json' >"$(cli)"
before=$(sum "$(cli)")
run_hook
check "cli.json malformed: left byte-identical" '[[ $(sum "$(cli)") == "$before" ]]'

# --- the terminal agent file ---------------------------------------------------------
# A user's additions to the hook's agent are kept, includeMcpJson false included.
fresh_home kiro
mkdir -p "$HOME/.kiro/agents"
echo '{"name":"openab-terminal","prompt":"p","includeMcpJson":false,"resources":["file://X.md"],"allowedTools":["fs_read"],"mcpServers":{"mine":{"command":"m"}}}' >"$(agent)"
chmod 600 "$(agent)"
run_hook
check "agent: user keys kept" '[[ $(jq -c "[.prompt,.includeMcpJson,.resources,.mcpServers.mine.command]" "$(agent)") == "[\"p\",false,[\"file://X.md\"],\"m\"]" ]]'
check "agent: computer added" '[[ $(jq -r .mcpServers.computer.url "$(agent)") == "http://127.0.0.1:8091/mcp" ]]'
check "agent: trust appended" '[[ $(jq -c .allowedTools "$(agent)") == "[\"fs_read\",\"@computer/*\"]" ]]'
check "agent: mode kept" '[[ $(stat -c %a "$(agent)") == 600 ]]'

for body in \
    '{"name":"someone-else"}' \
    '{"name":"openab-terminal","mcpServers":{"computer":{"command":"mine"}}}' \
    '{"name":"openab-terminal","allowedTools":"*"}' \
    'not json' '[1,2]' '{} {}'; do
    fresh_home kiro
    mkdir -p "$HOME/.kiro/agents"
    printf '%s' "$body" >"$(agent)"
    before=$(sum "$(agent)")
    run_hook
    check "agent not ours, byte-identical: $body" '[[ $(sum "$(agent)") == "$before" ]]'
    check "agent not ours, default not set: $body" '[[ ! -e $(cli) ]]'
done

fresh_home kiro
LISTEN=127.0.0.1:7000 run_hook
run_hook
check "agent: our entry follows a changed port" '[[ $(jq -r .mcpServers.computer.url "$(agent)") == "http://127.0.0.1:8091/mcp" ]]'

fresh_home kiro
LISTEN='[::1]:8091' run_hook
check "ipv6 listener: bracketed url" '[[ $(jq -r .mcpServers.computer.url "$(agent)") == "http://[::1]:8091/mcp" ]]'

fresh_home kiro
mkdir -p "$(agent)"
run_hook
check "agent path is a directory: left alone, no stray temp" '[[ -d $(agent) && -z $(ls -A "$(agent)") && -z $(ls -A "$HOME/.kiro/agents" | grep -v openab-terminal.json) ]]'

# --- files left alone -------------------------------------------------------------
for body in 'not json' '[1,2]' '{"mcpServers":"oops"}' '{} {}'; do
    fresh_home kiro
    mkdir -p "$HOME/.kiro/settings"
    printf '%s' "$body" >"$(mcp)"
    before=$(sum "$(mcp)")
    run_hook
    check "mcp.json left byte-identical: $body" '[[ $(sum "$(mcp)") == "$before" ]]'
done

fresh_home kiro
mkdir -p "$(mcp)"
run_hook
check "mcp.json is a directory: left alone, no stray temp" '[[ -d $(mcp) && -z $(ls -A "$(mcp)") ]]'

# --- symlinks ---------------------------------------------------------------------
# Inside ~/.kiro: followed, link kept.
fresh_home kiro
mkdir -p "$HOME/.kiro/settings" "$HOME/.kiro/shared"
echo "{\"mcpServers\":{\"computer\":$OLD_ENTRY,\"u\":{\"command\":\"u\"}}}" >"$HOME/.kiro/shared/mcp.json"
ln -s ../shared/mcp.json "$(mcp)"
run_hook
check "symlink inside .kiro: link kept" '[[ -L $(mcp) ]]'
check "symlink inside .kiro: target cleaned" '[[ $(jq -c ".mcpServers | keys" "$HOME/.kiro/shared/mcp.json") == "[\"u\"]" ]]'

# Out of ~/.kiro: never followed.
fresh_home kiro
mkdir -p "$HOME/.kiro/settings" "$HOME/dotfiles"
echo "{\"mcpServers\":{\"computer\":$OLD_ENTRY}}" >"$HOME/dotfiles/mcp.json"
before=$(sum "$HOME/dotfiles/mcp.json")
ln -s "$HOME/dotfiles/mcp.json" "$(mcp)"
run_hook
check "symlink out of .kiro: link kept" '[[ -L $(mcp) ]]'
check "symlink out of .kiro: target byte-identical" '[[ $(sum "$HOME/dotfiles/mcp.json") == "$before" ]]'
check "symlink out of .kiro: warned" 'grep -q "not following it" "$OUT"'

fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
ln -s "$HOME/nowhere.json" "$(mcp)"
run_hook
check "dangling symlink: left alone" '[[ -L $(mcp) && ! -e $HOME/nowhere.json ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/agents" "$HOME/dotfiles"
echo '{"name":"openab-terminal"}' >"$HOME/dotfiles/t.json"
before=$(sum "$HOME/dotfiles/t.json")
ln -s "$HOME/dotfiles/t.json" "$(agent)"
run_hook
check "agent symlink out of .kiro: link kept, target byte-identical" \
    '[[ -L $(agent) && $(sum "$HOME/dotfiles/t.json") == "$before" ]]'
check "agent symlink out of .kiro: default not set" '[[ ! -e $(cli) ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/agents"
ln -s "$HOME/nowhere.json" "$(agent)"
run_hook
check "agent dangling symlink: left alone" '[[ -L $(agent) && ! -e $HOME/nowhere.json && ! -e $(cli) ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro" "$HOME/elsewhere"
ln -s "$HOME/elsewhere" "$HOME/.kiro/agents"
run_hook
check "agents dir linked out of .kiro: nothing written there" '[[ -z $(ls -A "$HOME/elsewhere") ]]'

fresh_home kiro
mkdir -p "$HOME/elsewhere"
ln -s "$HOME/elsewhere" "$HOME/.kiro"
run_hook
check ".kiro itself a symlink: nothing written through it" '[[ -z $(ls -A "$HOME/elsewhere") ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/settings" "$HOME/dotfiles"
echo '{}' >"$HOME/dotfiles/cli.json"
ln -s "$HOME/dotfiles/cli.json" "$(cli)"
run_hook
check "cli.json symlink out of .kiro: target byte-identical" '[[ -L $(cli) && $(cat "$HOME/dotfiles/cli.json") == "{}" ]]'

# --- tools plane off --------------------------------------------------------------
fresh_home kiro
run_hook
LISTEN='' run_hook
check "off: our entry withdrawn from the agent" '[[ $(jq -c ".mcpServers | has(\"computer\")" "$(agent)") == false ]]'
check "off: agent and default kept" '[[ $(jq -r .name "$(agent)") == openab-terminal && $(default_agent) == openab-terminal ]]'

fresh_home kiro
mkdir -p "$HOME/.kiro/settings"
echo "{\"mcpServers\":{\"computer\":$OLD_ENTRY}}" >"$(mcp)"
LISTEN='' run_hook
check "off: old mcp.json entry withdrawn" '[[ $(jq -c .mcpServers "$(mcp)") == "{}" ]]'

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
mkdir -p "$HOME/.kiro/agents"
chmod 555 "$HOME/.kiro/agents"
if run_hook; then
    ok "unwritable dir: exit 0"
else
    not_ok "unwritable dir: exit 0"
fi
check "unwritable dir: default not set" '[[ ! -e $(cli) ]]'
chmod 755 "$HOME/.kiro/agents"

# A planted ~/.jq must not change what the hook does (HOME is the shared workspace).
fresh_home kiro
printf 'def has(k): error("pwned");\n' >"$HOME/.jq"
run_hook
check "planted .jq: ignored, entry still written" '[[ $(jq -r .mcpServers.computer.url "$(agent)") == "http://127.0.0.1:8091/mcp" ]]'

fresh_home kiro
if env -i PATH="$BIN:/usr/bin:/bin" OPENAB_PTY_TOOLS_LISTEN=127.0.0.1:8091 bash "$HOOK" >/dev/null 2>&1; then
    ok "HOME unset: exit 0"
else
    not_ok "HOME unset: exit 0"
fi

echo "# $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
