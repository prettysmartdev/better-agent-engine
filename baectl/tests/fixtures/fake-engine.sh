#!/bin/sh
# A stand-in for `docker` / Apple's `container`, used by baectl's offline CLI
# tests (tests/cli_offline.rs). Installed on PATH under both names.
#
# It records every invocation (cwd + argv) to $FAKE_ENGINE_STATE/calls.log and
# emulates just enough of the in-container `baectl` admin surface
# (`list profiles|keys`, `create profile|key`, `update profile`) over two
# JSON-lines files, so readiness checks and their fixes can be observed without
# a container engine. Every other engine verb (`compose up`, `build`, `run`,
# `rm`, …) is logged and succeeds.
#
# Shell builtins only: some tests run baectl with a PATH holding nothing but
# this script.
#
# Knobs (files under $FAKE_ENGINE_STATE):
#   profiles.jsonl / keys.jsonl  one compact JSON object per line
#   fail_list                    present → `list …` exits 1 (server unreachable)

state=${FAKE_ENGINE_STATE:?FAKE_ENGINE_STATE must be set}
printf 'cwd=%s argv=%s\n' "$(pwd)" "$*" >>"$state/calls.log"

list_jsonl() {
    printf '['
    sep=''
    if [ -f "$1" ]; then
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            printf '%s%s' "$sep" "$line"
            sep=','
        done <"$1"
    fi
    printf ']\n'
}

next_id() {
    n=0
    [ -f "$state/$1.counter" ] && read -r n <"$state/$1.counter"
    n=$((n + 1))
    printf '%s\n' "$n" >"$state/$1.counter"
    printf '%s' "$n"
}

# Parse `create/update profile` flags into JSON array bodies.
parse_profile_flags() {
    FB=''; TOOLS=''; MCP=''; SBX=''; NAME_FLAG=''
    while [ $# -gt 0 ]; do
        case "$1" in
            --fallback) FB="$FB${FB:+,}\"$2\""; shift 2 ;;
            --allowed-tool) TOOLS="$TOOLS${TOOLS:+,}\"$2\""; shift 2 ;;
            --mcp-server) MCP="$MCP${MCP:+,}\"$2\""; shift 2 ;;
            --available-sandbox) SBX="$SBX${SBX:+,}\"$2\""; shift 2 ;;
            --name) NAME_FLAG=$2; shift 2 ;;
            *) shift ;;
        esac
    done
}

profile_json() { # id name primary
    printf '{"id":"%s","name":"%s","primary_provider":"%s","fallback_providers":[%s],"allowed_tools":[%s],"mcp_servers":[%s],"available_sandboxes":[%s]}' \
        "$1" "$2" "$3" "$FB" "$TOOLS" "$MCP" "$SBX"
}

admin() {
    case "$1 $2" in
        "list profiles")
            [ -f "$state/fail_list" ] && { echo "connection refused" >&2; exit 1; }
            list_jsonl "$state/profiles.jsonl" ;;
        "list keys")
            [ -f "$state/fail_list" ] && { echo "connection refused" >&2; exit 1; }
            list_jsonl "$state/keys.jsonl" ;;
        "create profile")
            name=$3; primary=$4; shift 4
            parse_profile_flags "$@"
            id="pro_new$(next_id profile)"
            profile_json "$id" "$name" "$primary" >>"$state/profiles.jsonl"
            printf '\n' >>"$state/profiles.jsonl"
            printf '{"id":"%s","name":"%s","created_at":"2026-01-01T00:00:00Z"}\n' "$id" "$name" ;;
        "create key")
            name=$3; pid=$4
            n=$(next_id key)
            printf '{"id":"key_new%s","profile_id":"%s","name":"%s","prefix":"bae_x","last_used_at":null}\n' \
                "$n" "$pid" "$name" >>"$state/keys.jsonl"
            printf '{"id":"key_new%s","profile_id":"%s","name":"%s","key":"bae_fake_secret_%s"}\n' \
                "$n" "$pid" "$name" "$n" ;;
        "update profile")
            id=$3; primary=$4; shift 4
            parse_profile_flags "$@"
            new=$(profile_json "$id" "$NAME_FLAG" "$primary")
            : >"$state/profiles.next"
            if [ -f "$state/profiles.jsonl" ]; then
                while IFS= read -r line; do
                    case "$line" in
                        *"\"id\":\"$id\""*) printf '%s\n' "$new" >>"$state/profiles.next" ;;
                        *) printf '%s\n' "$line" >>"$state/profiles.next" ;;
                    esac
                done <"$state/profiles.jsonl"
            fi
            # `mv` is not a builtin; copy back line by line instead.
            : >"$state/profiles.jsonl"
            while IFS= read -r line; do printf '%s\n' "$line" >>"$state/profiles.jsonl"; done <"$state/profiles.next"
            printf '%s\n' "$new" ;;
        *)
            echo "fake engine: unsupported in-container baectl call: $*" >&2
            exit 64 ;;
    esac
}

case "$1" in
    inspect)
        [ -f "$state/fail_inspect" ] && exit 1
        if [ -f "$state/inspect.json" ]; then
            while IFS= read -r line; do printf '%s\n' "$line"; done <"$state/inspect.json"
        else
            printf '[{"status":{"networks":[{"network":"default","ipv4Address":"192.168.64.3/24"}]}}]\n'
        fi ;;
    compose)
        shift
        case "$1" in
            exec)
                # compose exec -T <service> baectl <args…>
                shift
                [ "$1" = "-T" ] && shift
                shift # service
                [ "$1" = "baectl" ] && shift
                admin "$@" ;;
            *) exit 0 ;; # up -d / down / restart …
        esac ;;
    exec)
        # Apple: container exec <name> baectl <args…>
        shift 2
        [ "$1" = "baectl" ] && shift
        admin "$@" ;;
    run)
        # Like the real engine, an --env-file path resolves against the cwd.
        while [ $# -gt 0 ]; do
            if [ "$1" = "--env-file" ]; then
                [ -f "$2" ] || { echo "open $2: no such file or directory" >&2; exit 125; }
            fi
            shift
        done
        exit 0 ;;
    *) exit 0 ;; # build / rm / stop / logs …
esac
