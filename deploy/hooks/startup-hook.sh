#!/usr/bin/env bash
# openab-pty startup hook: wire the tools MCP into the image's coding CLI.
#
# Run by the runtime once, after seeding and before serving
# (`openab-pty --startup-hook`, see runtime/src/hook.rs). The runtime knows no
# CLI's config format; this script is where that knowledge lives, one function
# per CLI, so a new variant is a new function here and no runtime change.
#
# What it writes is session-independent (CLIENT-CONTRACT §9.3, preferred form):
# a constant URL plus an Authorization header that each CLI process expands from
# its own session's environment. No session name and no key is ever written to
# disk, so one shared workspace config serves every session and survives every
# key rotation without a rewrite.
#
# Contract with the runtime: best-effort. Anything unexpected is a warning and
# exit 0 — a CLI this script cannot wire still works through the manual path.
#
# Based on the merge rules from openabdev/openab-pty#45 by @Reese-max: existing
# servers and settings are preserved, files that are not what we expect are
# left byte-identical, agent files are never created, dangling symlinks are
# left alone, writes are atomic and keep the file's mode.
# jq filters are single-quoted on purpose; their $vars are jq's, not the shell's.
# shellcheck disable=SC2016
set -uo pipefail

SERVER=computer
TRUST='@computer/*'
# Literal on purpose: the CLI expands it per process, from the session's env.
AUTH_HEADER='Bearer ${OPENAB_TOOLS_MCP_TOKEN}'

warn() { echo "openab-pty-startup-hook: $*" >&2; }
info() { echo "openab-pty-startup-hook: $*"; }

# merge_json FILE JQ_FILTER [jq args...]
#
# Apply FILTER to FILE's JSON object (a missing or blank file is `{}`) and
# write the result atomically — only if it differs semantically, so a spawn
# never churns the file. The filter may `error("skip: …")` to leave the file
# untouched. Returns 0 always; prints what it did.
merge_json() {
    local path=$1
    shift
    local real=$path
    if [[ -L $path ]]; then
        real=$(realpath -e -- "$path" 2>/dev/null) || {
            warn "skip $path: symlink target does not exist"
            return 0
        }
    fi
    local input='{}'
    if [[ -e $real ]]; then
        if grep -q '[^[:space:]]' -- "$real" 2>/dev/null; then
            if ! jq -e 'type == "object"' -- "$real" >/dev/null 2>&1; then
                warn "skip $real: not a JSON object"
                return 0
            fi
            input=$(cat -- "$real")
        fi
    fi
    local output
    if ! output=$(jq --indent 2 "$@" <<<"$input" 2>&1); then
        warn "skip $real: ${output#jq: error (at <stdin>:*): }"
        return 0
    fi
    if [[ $(jq -cS . <<<"$input") == "$(jq -cS . <<<"$output")" ]]; then
        return 0
    fi
    local dir
    dir=$(dirname -- "$real")
    mkdir -p -- "$dir" || { warn "skip $real: cannot create $dir"; return 0; }
    local tmp
    tmp=$(mktemp "$dir/.$(basename -- "$real").XXXXXX") || {
        warn "skip $real: cannot create a temp file in $dir"
        return 0
    }
    printf '%s\n' "$output" >"$tmp"
    if [[ -e $real ]]; then
        chmod --reference="$real" -- "$tmp" 2>/dev/null || true
    else
        chmod 644 -- "$tmp"
    fi
    if mv -f -- "$tmp" "$real"; then
        info "wrote $real"
    else
        rm -f -- "$tmp"
        warn "skip $real: rename failed"
    fi
}

# --- kiro-cli ---------------------------------------------------------------
#
# ~/.kiro/settings/mcp.json gets `mcpServers.computer`; every existing
# ~/.kiro/agents/*.json gets `@computer/*` in `allowedTools`. kiro expands
# ${VAR} inside `headers` but not inside `url` (verified, #44), which is why the
# key travels in the header.
wire_kiro() {
    local mcp=$HOME/.kiro/settings/mcp.json
    if [[ -n ${PTY_TOOLS_LISTEN:-} ]]; then
        merge_json "$mcp" \
            --arg name "$SERVER" --arg url "http://${PTY_TOOLS_LISTEN}/mcp" --arg auth "$AUTH_HEADER" '
            if has("mcpServers") and (.mcpServers | type) != "object"
            then error("skip: mcpServers is not an object") else . end
            | .mcpServers[$name] = {url: $url, headers: {Authorization: $auth}}'

        local agent
        for agent in "$HOME"/.kiro/agents/*.json; do
            [[ -f $agent ]] || continue
            merge_json "$agent" --arg trust "$TRUST" '
                if has("allowedTools") and (.allowedTools | type) != "array"
                then error("skip: allowedTools is not an array") else . end
                | if ((.allowedTools // []) | any(. == $trust or . == "@computer"))
                  then . else .allowedTools = ((.allowedTools // []) + [$trust]) end'
        done
    else
        # Tools plane off: withdraw only the entry this hook wrote, recognised
        # by its header, so a persistent workspace does not keep a server that
        # can only answer 401. Anything else named `computer` is the user's.
        [[ -e $mcp ]] || return 0
        merge_json "$mcp" --arg name "$SERVER" --arg auth "$AUTH_HEADER" '
            if (.mcpServers | type) == "object"
               and ((try .mcpServers[$name].headers.Authorization catch null) == $auth)
            then del(.mcpServers[$name]) else . end'
    fi
}

main() {
    command -v jq >/dev/null 2>&1 || {
        warn "jq not found; tools MCP not wired (manual path: CLIENT-CONTRACT §9.3)"
        return 0
    }
    if command -v kiro-cli >/dev/null 2>&1; then
        wire_kiro
    fi
    # Other CLIs: not wired yet; the env vars and §9.3's manual path apply.
    return 0
}

main "$@"
exit 0
