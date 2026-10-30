#!/usr/bin/env bash
# openab-pty startup hook: wire the tools MCP into the image's coding CLI.
#
# Run by the runtime once, after seeding and binding and before serving
# (`openab-pty --startup-hook`, see runtime/src/hook.rs). The runtime knows no
# CLI's config format; this script is where that knowledge lives, one function
# per CLI, so a new variant is a new function here and no runtime change.
#
# Inputs, both from the runtime and nothing else (it clears the environment):
#   HOME                     the shared workspace
#   OPENAB_PTY_TOOLS_LISTEN  the bound tools listener (host:port, IPv6
#                            bracketed); unset when the tools plane is off
# plus the session env allowlist (PATH, LANG, LC_*, ...).
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
# left byte-identical, the only file created is the hook's own, dangling
# symlinks are left alone, symlinks out of ~/.kiro are never followed, writes
# are atomic and keep the file's mode.

# jq filters are single-quoted on purpose; their $vars are jq's, not the shell's.
# shellcheck disable=SC2016
set -uo pipefail

SERVER=computer
TRUST='@computer/*'
# The terminal-only kiro agent that carries the `computer` server (#76).
AGENT=openab-terminal
# Literal on purpose: the CLI expands it per process, from the session's env.
AUTH_HEADER='Bearer ${OPENAB_TOOLS_MCP_TOKEN}'

# The exact server entry this hook writes (and, before #76, wrote into
# ~/.kiro/settings/mcp.json): nothing but this url shape and this header. An
# entry a user has edited no longer matches, and is theirs.
JQ_OURS='def ours($auth):
    type == "object" and keys == ["headers", "url"]
    and .headers == {Authorization: $auth}
    and (.url | type == "string" and test("^http://[^/]+/mcp$"));
'

warn() { echo "openab-pty-startup-hook: $*" >&2; }
info() { echo "openab-pty-startup-hook: $*"; }

# jq loads ~/.jq, and HOME is the workspace every session can write to: a
# planted ~/.jq could redefine the builtins the filters below rely on. Run it
# with a HOME that has nothing in it.
jq() { HOME=/nonexistent command jq "$@"; }

# phys PATH: PATH with every symlink in it resolved. A missing tail is kept as
# written (it cannot hold a symlink). Fails on a dangling symlink.
phys() {
    local p=$1 tail=''
    while [[ ! -e $p && ! -L $p ]]; do
        tail=/$(basename -- "$p")$tail
        p=$(dirname -- "$p")
    done
    if [[ -L $p ]]; then
        p=$(realpath -e -- "$p" 2>/dev/null) || return 1
    elif [[ -d $p ]]; then
        p=$(cd -P -- "$p" 2>/dev/null && pwd -P) || return 1
    else
        local d
        d=$(cd -P -- "$(dirname -- "$p")" 2>/dev/null && pwd -P) || return 1
        p=$d/$(basename -- "$p")
    fi
    printf '%s\n' "$p$tail"
}

# merge_json FILE [jq args...] JQ_FILTER
#
# Apply FILTER to FILE's JSON object (a missing or blank file is `{}`) and
# write the result atomically — only if it differs semantically, so a boot
# never churns the file. FILE must resolve, symlinks included, to a path under
# $HOME/.kiro; anything else is left alone. The filter may `error("skip: …")`
# to leave the file untouched with a warning, or `error("notice: …")` to leave
# it untouched with an informational line. Returns 0 when the file is as the
# filter wants it (written or already so), 1 when it was left alone.
merge_json() {
    local path=$1
    shift
    local home root real
    home=$(cd -P -- "$HOME" 2>/dev/null && pwd -P) || {
        warn "skip $path: HOME is not a directory"
        return 1
    }
    root=$home/.kiro
    real=$(phys "$path") || {
        warn "skip $path: symlink target does not exist"
        return 1
    }
    case $real in
        "$root"/*) ;;
        *)
            warn "skip $path: resolves outside $HOME/.kiro ($real); not following it"
            return 1
            ;;
    esac
    if [[ -e $real && ! -f $real ]]; then
        warn "skip $real: not a regular file"
        return 1
    fi
    local input='{}'
    if [[ -e $real ]] && grep -q '[^[:space:]]' -- "$real" 2>/dev/null; then
        # Exactly one JSON value, and an object: `{} {}` is not a config.
        if ! jq -e -s 'length == 1 and (.[0] | type) == "object"' -- "$real" >/dev/null 2>&1; then
            warn "skip $real: not a single JSON object"
            return 1
        fi
        input=$(cat -- "$real") || { warn "skip $real: unreadable"; return 1; }
    fi
    local output
    if ! output=$(jq --indent 2 "$@" <<<"$input" 2>&1); then
        output=${output#jq: error (at <stdin>:*): }
        if [[ $output == notice:* ]]; then
            info "${output#notice: }"
        else
            warn "skip $real: $output"
        fi
        return 1
    fi
    if [[ $(jq -cS . <<<"$input") == "$(jq -cS . <<<"$output")" ]]; then
        return 0
    fi
    local dir
    dir=$(dirname -- "$real")
    mkdir -p -- "$dir" || { warn "skip $real: cannot create $dir"; return 1; }
    local tmp
    tmp=$(mktemp "$dir/.$(basename -- "$real").XXXXXX") || {
        warn "skip $real: cannot create a temp file in $dir"
        return 1
    }
    # A short write (ENOSPC, EIO) must never be renamed over the real file.
    if ! printf '%s\n' "$output" >"$tmp"; then
        rm -f -- "$tmp"
        warn "skip $real: write failed"
        return 1
    fi
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
        return 1
    fi
}

# --- kiro-cli ---------------------------------------------------------------
#
# The `computer` server lives in a terminal-only agent,
# ~/.kiro/agents/openab-terminal.json (`includeMcpJson: true`, so the user's
# ~/.kiro/settings/mcp.json servers still load in the terminal), and that agent
# becomes kiro's default: `chat.defaultAgent` in ~/.kiro/settings/cli.json, the
# key `kiro-cli agent set-default` writes. A default the user already chose,
# `kiro_default` included, is left alone with a notice.
#
# Nothing goes into ~/.kiro/settings/mcp.json any more (#76): anything else
# running kiro-cli under this HOME, such as a colocated bot, reads that file,
# has no session token, and got a 401 on every session. A bot started with
# `kiro-cli acp --agent <its own agent>` never loads openab-terminal. kiro
# expands ${VAR} inside `headers` but not inside `url` (verified, #44), which
# is why the key travels in the header.
wire_kiro() {
    local agent=$HOME/.kiro/agents/$AGENT.json
    local listen=${OPENAB_PTY_TOOLS_LISTEN:-}
    if [[ -n $listen ]]; then
        # The agent file is the hook's, but a user may add to it: keys it
        # already has are kept (`includeMcpJson: false` too), and a `computer`
        # entry that is not ours by header, or a different `name`, means the
        # file is someone else's: warn and leave it. A new file gets
        # kiro_default's resources (as of kiro-cli 2.35/2.36), so AGENTS.md,
        # README.md and skills still load in the terminal.
        if merge_json "$agent" \
            --arg agent "$AGENT" --arg name "$SERVER" --arg url "http://${listen}/mcp" \
            --arg auth "$AUTH_HEADER" --arg trust "$TRUST" \
            --arg skills "skill://$HOME/.kiro/skills/*/SKILL.md" '
            def default(k; v): if has(k) then . else .[k] = v end;
            if has("name") and .name != $agent
            then error("skip: name is not \($agent); leaving it") else . end
            | if has("mcpServers") and (.mcpServers | type) != "object"
              then error("skip: mcpServers is not an object") else . end
            | if has("allowedTools") and (.allowedTools | type) != "array"
              then error("skip: allowedTools is not an array") else . end
            | (.mcpServers[$name]) as $cur
            | if $cur != null and ((try $cur.headers.Authorization catch null) != $auth)
              then error("skip: mcpServers.computer is not ours; leaving it") else . end
            | default("name"; $agent)
            | default("description";
                "openab-pty terminal agent: carries the computer MCP server (written by the openab-pty startup hook)")
            | default("tools"; ["*"])
            | default("resources"; ["file://AmazonQ.md", "file://AGENTS.md", "file://README.md",
                                    "skill://.kiro/skills/*/SKILL.md", $skills])
            | default("includeMcpJson"; true)
            | .mcpServers[$name] = {url: $url, headers: {Authorization: $auth}}
            | if ((.allowedTools // []) | any(. == $trust or . == "@computer" or . == "*"))
              then . else .allowedTools = ((.allowedTools // []) + [$trust]) end'
        then
            merge_json "$HOME/.kiro/settings/cli.json" \
                --arg key chat.defaultAgent --arg agent "$AGENT" '
                if .[$key] == null or .[$key] == "" then .[$key] = $agent
                elif .[$key] == $agent then .
                else error("notice: kiro default agent is \(.[$key] | tojson) (chat.defaultAgent, set by the user); left unchanged, so the computer MCP server needs: kiro-cli chat --agent \($agent)")
                end'
        fi
    elif [[ -e $agent ]]; then
        # Tools plane off: withdraw only the server entry this hook wrote, so a
        # persistent workspace does not keep a server that can only answer
        # 401. The agent, its trust and the default agent stay.
        merge_json "$agent" --arg name "$SERVER" --arg auth "$AUTH_HEADER" "$JQ_OURS"'
            if (.mcpServers | type) == "object" and (.mcpServers[$name] | ours($auth))
            then del(.mcpServers[$name]) else . end'
    fi

    # Before #76 the hook wrote `computer` into the shared mcp.json, so remove
    # that entry, and only that entry: by name and its exact shape. A
    # hand-wired or edited `computer` is the user's. Either way the hook no
    # longer touches other agents' `allowedTools`; `@computer/*` it appended to
    # them earlier is indistinguishable from a user's own, so it stays (it
    # names a server those agents no longer get from the hook).
    local mcp=$HOME/.kiro/settings/mcp.json
    [[ -e $mcp ]] || return 0
    merge_json "$mcp" --arg name "$SERVER" --arg auth "$AUTH_HEADER" "$JQ_OURS"'
        if (.mcpServers | type) == "object" and (.mcpServers[$name] | ours($auth))
        then del(.mcpServers[$name]) else . end'
}

main() {
    if [[ -z ${HOME:-} ]]; then
        warn "HOME is unset; nothing to wire"
        return 0
    fi
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
