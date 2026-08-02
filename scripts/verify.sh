#!/usr/bin/env bash
# End-to-end verification drills against the containerised server.
# Usage: scripts/verify.sh [all | 1 2 3 4 5 6 7]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SYN="$ROOT/target/release/syn"
IMAGE="synapse-server:local"
MODEL="bge-small-en-v1.5"
DATA="$ROOT/data"
SCRATCH="$ROOT/.verify"
BASE_URL="http://127.0.0.1:8737"
PROXY_URL="http://127.0.0.1:8738"
AIRGAP_NET="synapse-airgap"
CURL_IMAGE="curlimages/curl:latest"

export SYNAPSE_TOKEN="${SYNAPSE_TOKEN:-verify-token}"
export SYNAPSE_UID="${SYNAPSE_UID:-$(id -u)}"
export SYNAPSE_GID="${SYNAPSE_GID:-$(id -g)}"

WORK_FACT="Staging deploys go through ArgoCD every Friday."
PERSONAL_FACT="Sourdough starter needs feeding every Friday."
PREFERENCE="Always answer with the conclusion first."
CROSS_QUERY="every Friday"
PREFERENCE_QUERY="answer with the conclusion first"

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok() { printf '   ok   %s\n' "$*"; }
info() { printf '   ..   %s\n' "$*"; }
die() {
	printf '   FAIL %s\n' "$*" >&2
	exit 1
}

require_tools() {
	for tool in docker jq sqlite3 python3 curl; do
		command -v "$tool" >/dev/null || die "$tool is required"
	done
	[[ -x "$SYN" ]] || {
		info "building syn"
		cargo build --release --bin syn >/dev/null
	}
}

syn_as() { # syn_as <client> <cwd> <args...>
	local client=$1 cwd=$2
	shift 2
	(
		cd "$cwd"
		SYNAPSE_CONFIG_DIR="$SCRATCH/$client/config" \
			SYNAPSE_STATE_DIR="$SCRATCH/$client/state" \
			"$SYN" "$@"
	)
}

health() { curl -sS -m 5 "$BASE_URL/health" 2>/dev/null || echo '{"status":"down"}'; }
health_status() { health | jq -r '.status // "down"'; }
health_reason() { health | jq -r '.reason // ""'; }

wait_for_status() {
	local want=$1 tries=${2:-120} soft=${3:-}
	for ((i = 0; i < tries; i++)); do
		[[ "$(health_status)" == "$want" ]] && return 0
		sleep 1
	done
	[[ -n "$soft" ]] && return 1
	die "server never reported status=$want (last: $(health))"
}

# Docker Desktop propagates a host-side wipe of the bind mount lazily, and readiness is
# decided once at boot: a container that starts on the stale view stays unready forever.
stack_up() {
	for _ in 1 2 3; do
		docker compose up -d >/dev/null
		wait_for_status ready 30 soft && return 0
		docker compose down >/dev/null 2>&1 || true
		sleep 2
	done
	die "server never reported ready (last: $(health))"
}

client_init() {
	local client=$1 url=${2:-$BASE_URL}
	mkdir -p "$SCRATCH/$client/config" "$SCRATCH/$client/state"
	syn_as "$client" "$ROOT" config set-url "$url" >/dev/null
	syn_as "$client" "$ROOT" config set-token "$SYNAPSE_TOKEN" >/dev/null
}

stack_reset() {
	docker compose down --remove-orphans >/dev/null 2>&1 || true
	rm -rf "$SCRATCH"
	wipe_data
	stack_up
	client_init A
	client_init B
}

seed_workspaces() {
	mkdir -p "$SCRATCH/cwd/work" "$SCRATCH/cwd/personal"
	for client in A B; do
		syn_as "$client" "$ROOT" workspace create work >/dev/null
		syn_as "$client" "$ROOT" workspace create personal >/dev/null
		syn_as "$client" "$ROOT" workspace map "$SCRATCH/cwd/work" work >/dev/null
		syn_as "$client" "$ROOT" workspace map "$SCRATCH/cwd/personal" personal >/dev/null
		syn_as "$client" "$ROOT" workspace use work >/dev/null
	done
}

bulk_dump() { # bulk_dump <workspace> <count>
	jq -n --arg ws "$1" --argjson n "$2" '{
      version: 1,
      origin: { workspace: $ws },
      memories: [range(0; $n) as $i | {
        id: ("m_bulk" + (("000000000000000000" + ($i | tostring)) | .[-18:])),
        content: "Bulk seeded fact \($i) about release pipelines and rollbacks.",
        kind: "reference",
        scope: "workspace",
        tags: [],
        pinned: false,
        created_at: "2026-08-01T00:00:00Z",
        updated_at: "2026-08-01T00:00:00Z"
      }]
    }'
}

db_model() { sqlite3 "$DATA/$1.db" "SELECT embedding_model FROM meta WHERE id = 1;"; }

# Empties the volume in place — replacing the directory would leave the bind mount
# pointing at a dead inode — then lets the change propagate before a container boots.
wipe_data() {
	mkdir -p "$DATA"
	find "$DATA" -mindepth 1 -delete
	sleep 2
}

drill_1() {
	say "drill 1 — save, sever the response, retry: no duplicate"
	stack_reset
	seed_workspaces
	python3 "$ROOT/scripts/sever-proxy.py" 8738 127.0.0.1 8737 >"$SCRATCH/proxy.log" 2>&1 &
	local proxy=$!
	for _ in {1..50}; do grep -q ready "$SCRATCH/proxy.log" 2>/dev/null && break; sleep 0.1; done
	client_init A "$PROXY_URL"

	local out
	out=$(syn_as A "$SCRATCH/cwd/work" save "$WORK_FACT" --type project 2>&1) || true
	sed 's/^/        /' <<<"$out"
	grep -q '^queued ' <<<"$out" || die "expected the save to stay queued when the reply is lost"
	local id
	id=$(awk '/^queued /{print $2}' <<<"$out")

	kill "$proxy" 2>/dev/null || true
	wait "$proxy" 2>/dev/null || true
	client_init A "$BASE_URL"

	syn_as A "$SCRATCH/cwd/work" list >/dev/null
	[[ -z "$(syn_as A "$ROOT" list --pending)" ]] || die "outbox still holds items after the retry"
	local stored
	stored=$(syn_as A "$SCRATCH/cwd/work" list | grep -c -- "$WORK_FACT" || true)
	[[ "$stored" == 1 ]] || die "expected exactly one stored copy of the fact, found $stored"
	syn_as A "$SCRATCH/cwd/work" show "$id" >/dev/null || die "$id is not the memory that landed"
	ok "single memory $id survived the severed response and the retry"
}

drill_2() {
	say "drill 2 — two-client round trip"
	stack_reset
	seed_workspaces
	syn_as A "$SCRATCH/cwd/work" save "$WORK_FACT" --type project >/dev/null
	local out
	out=$(syn_as B "$SCRATCH/cwd/work" recall "ArgoCD staging deploys")
	sed 's/^/        /' <<<"$out"
	grep -q -- "$WORK_FACT" <<<"$out" || die "client B cannot recall what client A saved"
	ok "client B recalled client A's memory"
}

drill_3() {
	say "drill 3 — save while the server is down, flush on the next command"
	stack_reset
	seed_workspaces
	docker compose stop >/dev/null
	local out
	out=$(syn_as A "$SCRATCH/cwd/work" save "$WORK_FACT" --type project 2>&1) || true
	sed 's/^/        /' <<<"$out"
	grep -q 'not yet recallable' <<<"$out" || die "offline save did not report as queued"
	syn_as A "$ROOT" list --pending | grep -q 'queued' || die "nothing queued in the outbox"

	docker compose start >/dev/null
	wait_for_status ready
	out=$(syn_as A "$SCRATCH/cwd/work" recall "ArgoCD staging deploys")
	sed 's/^/        /' <<<"$out"
	grep -q -- "$WORK_FACT" <<<"$out" || die "the queued save never reached the server"
	[[ -z "$(syn_as A "$ROOT" list --pending)" ]] || die "outbox not drained after the flush"
	ok "queued save flushed by the next read and became recallable"
}

drill_4() {
	say "drill 4 — export, wipe the volume, import, recall"
	stack_reset
	seed_workspaces
	syn_as A "$SCRATCH/cwd/work" save "$WORK_FACT" --type project >/dev/null
	syn_as A "$SCRATCH/cwd/work" remember "$PREFERENCE" >/dev/null
	syn_as A "$ROOT" export --workspace work >"$SCRATCH/work.json"
	syn_as A "$ROOT" export --preference >"$SCRATCH/preferences.json"
	info "dumped $(jq '.memories | length' <"$SCRATCH/work.json") work memories, \
$(jq '.memories | length' <"$SCRATCH/preferences.json") preferences"

	docker compose down >/dev/null
	wipe_data
	stack_up
	[[ "$(syn_as A "$ROOT" workspace list)" == "" ]] || die "the wipe left workspaces behind"

	syn_as A "$ROOT" workspace create work >/dev/null
	syn_as A "$ROOT" import --workspace work <"$SCRATCH/work.json" | sed 's/^/        /'
	syn_as A "$ROOT" import --preference <"$SCRATCH/preferences.json" | sed 's/^/        /'
	local out
	out=$(syn_as A "$SCRATCH/cwd/work" recall "ArgoCD staging deploys")
	sed 's/^/        /' <<<"$out"
	grep -q -- "$WORK_FACT" <<<"$out" || die "restored work memory is not recallable"
	out=$(syn_as A "$SCRATCH/cwd/work" recall "$PREFERENCE_QUERY")
	sed 's/^/        /' <<<"$out"
	grep -q '(preference,' <<<"$out" || die "restored preference is not recallable"
	ok "restore from dumps recovered both the workspace memory and the preference"
}

drill_5() {
	say "drill 5 — cold start with no route to the HF hub"
	docker compose down >/dev/null 2>&1 || true
	docker rm -f synapse-airgap >/dev/null 2>&1 || true
	docker pull -q "$CURL_IMAGE" >/dev/null
	docker network rm "$AIRGAP_NET" >/dev/null 2>&1 || true
	docker network create --internal "$AIRGAP_NET" >/dev/null

	local egress
	egress=$(docker run --rm --network "$AIRGAP_NET" "$CURL_IMAGE" \
		-sS -m 8 -o /dev/null https://huggingface.co 2>&1 || true)
	[[ -n "$egress" ]] || die "the airgap network still reaches huggingface.co"
	info "egress check: ${egress//$'\n'/ }"

	docker run -d --name synapse-airgap --network "$AIRGAP_NET" \
		-e SYNAPSE_TOKEN="$SYNAPSE_TOKEN" "$IMAGE" >/dev/null
	local status=""
	for _ in {1..60}; do
		status=$(docker run --rm --network "$AIRGAP_NET" "$CURL_IMAGE" \
			-sS -m 5 http://synapse-airgap:8737/health 2>/dev/null | jq -r '.status // ""' || true)
		[[ "$status" == "ready" ]] && break
		sleep 1
	done
	docker logs synapse-airgap 2>&1 | sed 's/^/        /'
	[[ "$status" == "ready" ]] || die "the airgapped container never reported ready (last: ${status:-none})"
	docker rm -f synapse-airgap >/dev/null
	docker network rm "$AIRGAP_NET" >/dev/null
	ok "baked model loaded and /health went ready with no egress"
}

drill_6() {
	say "drill 6 — workspace isolation on the default path"
	stack_reset
	seed_workspaces
	syn_as A "$SCRATCH/cwd/work" save "$WORK_FACT" --type project >/dev/null
	syn_as A "$SCRATCH/cwd/personal" save "$PERSONAL_FACT" --type project >/dev/null
	syn_as A "$SCRATCH/cwd/work" remember "$PREFERENCE" >/dev/null

	local from_work from_personal grouped out
	from_work=$(syn_as A "$SCRATCH/cwd/work" recall "$CROSS_QUERY")
	sed 's/^/        /' <<<"$from_work"
	grep -q -- "$WORK_FACT" <<<"$from_work" || die "work recall lost its own memory"
	grep -q -- "$PERSONAL_FACT" <<<"$from_work" && die "personal memory leaked into the work default path"

	from_personal=$(syn_as A "$SCRATCH/cwd/personal" recall "$CROSS_QUERY")
	sed 's/^/        /' <<<"$from_personal"
	grep -q -- "$PERSONAL_FACT" <<<"$from_personal" || die "personal recall lost its own memory"
	grep -q -- "$WORK_FACT" <<<"$from_personal" && die "work memory leaked into the personal default path"

	for cwd in work personal; do
		out=$(syn_as A "$SCRATCH/cwd/$cwd" recall "$PREFERENCE_QUERY")
		sed "s|^|        $cwd: |" <<<"$out"
		grep -q '(preference,' <<<"$out" || die "the preference does not reach the $cwd workspace"
	done

	grouped=$(syn_as A "$SCRATCH/cwd/work" recall --all-workspaces "$CROSS_QUERY")
	sed 's/^/        /' <<<"$grouped"
	grep -q '^## work$' <<<"$grouped" || die "--all-workspaces did not group the work hits"
	grep -q '^## personal$' <<<"$grouped" || die "--all-workspaces did not group the personal hits"
	grep -q -- "$WORK_FACT" <<<"$grouped" || die "--all-workspaces missed the work memory"
	grep -q -- "$PERSONAL_FACT" <<<"$grouped" || die "--all-workspaces missed the personal memory"
	local preference_groups
	preference_groups=$(grep -c '^## preference$' <<<"$grouped" || true)
	[[ "$preference_groups" -le 1 ]] || die "preferences emitted $preference_groups times, expected at most one group"
	ok "default path stayed inside its workspace; --all-workspaces crossed it, grouped"
}

drill_7() {
	say "drill 7 — reembed killed mid-run: unready, then resumed"
	stack_reset
	syn_as A "$ROOT" workspace create alpha >/dev/null
	syn_as A "$ROOT" workspace create zeta >/dev/null
	syn_as A "$ROOT" save "$WORK_FACT" --type project --workspace alpha >/dev/null
	bulk_dump zeta 400 | syn_as A "$ROOT" import --workspace zeta | sed 's/^/        /'

	docker compose stop >/dev/null
	for ws in alpha zeta; do
		sqlite3 "$DATA/$ws.db" "UPDATE meta SET embedding_model = 'stale-model' WHERE id = 1;"
	done
	docker compose start >/dev/null
	wait_for_status unready
	local reason
	reason=$(health_reason)
	info "unready: $reason"
	grep -q 'embedding meta mismatch' <<<"$reason" || die "a mismatched registry did not block readiness"
	docker compose stop >/dev/null

	docker rm -f synapse-reembed >/dev/null 2>&1 || true
	docker run -d --name synapse-reembed -v "$DATA:/data" --user "$SYNAPSE_UID:$SYNAPSE_GID" \
		-e SYNAPSE_TOKEN="$SYNAPSE_TOKEN" "$IMAGE" reembed --model "$MODEL" >/dev/null
	local killed=false
	for _ in {1..600}; do
		if docker logs synapse-reembed 2>&1 | grep -q 'reembedded'; then
			docker kill synapse-reembed >/dev/null
			killed=true
			break
		fi
		sleep 0.1
	done
	docker logs synapse-reembed 2>&1 | sed 's/^/        /'
	docker rm -f synapse-reembed >/dev/null
	[[ "$killed" == true ]] || die "reembed never reported a converted workspace"
	[[ -f "$DATA/reembed.target" ]] || die "the kill left no reembed.target — the run finished, raise the bulk seed"
	[[ "$(db_model zeta)" == "stale-model" ]] || die "zeta converted before the kill — raise the bulk seed"
	info "killed with alpha=$(db_model alpha) zeta=$(db_model zeta)"

	docker compose start >/dev/null
	wait_for_status unready
	reason=$(health_reason)
	info "unready: $reason"
	grep -q 'reembed is in progress' <<<"$reason" || die "an interrupted reembed did not block readiness"
	docker compose stop >/dev/null

	docker run --rm -v "$DATA:/data" --user "$SYNAPSE_UID:$SYNAPSE_GID" \
		-e SYNAPSE_TOKEN="$SYNAPSE_TOKEN" "$IMAGE" reembed --model "$MODEL" 2>&1 | sed 's/^/        /'
	[[ ! -f "$DATA/reembed.target" ]] || die "reembed.target outlived a completed run"
	[[ "$(db_model zeta)" == "$MODEL" ]] || die "zeta is still at $(db_model zeta)"

	docker compose start >/dev/null
	wait_for_status ready
	syn_as A "$ROOT" recall "ArgoCD staging deploys" --workspace alpha | sed 's/^/        /'
	syn_as A "$ROOT" recall "release pipelines rollbacks" --workspace zeta | tail -1 | sed 's/^/        /'
	ok "interrupted reembed blocked readiness; the restart resumed and completed it"
}

main() {
	require_tools
	local drills=("$@")
	if [[ ${#drills[@]} -eq 0 || "${drills[0]}" == all ]]; then
		drills=(1 2 3 4 5 6 7)
	fi
	for drill in "${drills[@]}"; do
		"drill_$drill"
	done
	say "all requested drills passed"
	docker compose down >/dev/null 2>&1 || true
}

main "$@"
