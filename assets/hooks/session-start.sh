#!/bin/sh
# Session-start digest for harnesses that inject hook stdout as context.
# Plain stdout by default; `--json` wraps it as {"additionalContext": "..."}.
# Every failure is silent and immediate: a session must never wait on memory.
set -u

command -v syn >/dev/null 2>&1 || exit 0

digest=$(syn context 2>/dev/null) || exit 0
[ -n "$digest" ] || exit 0

if [ "${1:-}" = "--json" ]; then
    escaped=$(
        printf '%s' "$digest" |
            sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/\\t/g' |
            awk 'BEGIN { ORS = "" } NR > 1 { print "\\n" } { print }'
    )
    printf '{"additionalContext":"%s"}\n' "$escaped"
else
    printf '%s\n' "$digest"
fi
