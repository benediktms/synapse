bin_dir := env('HOME') / '.local/bin'
syn := justfile_directory() / 'target/release/syn'

_default:
    @just --list

# build the release binaries
build:
    cargo build --release

# symlink syn into ~/.local/bin (rebuilds first)
install: build
    mkdir -p {{ bin_dir }}
    ln -sf {{ syn }} {{ bin_dir }}/syn
    @{{ bin_dir }}/syn --version

test:
    cargo test --workspace

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# everything CI would check
check: lint test

# install the skill and session hooks into Claude Code, Codex CLI and Copilot CLI
agents *args:
    cargo run --quiet -p xtask -- install-agents {{ args }}

# preview what `just agents` would change
agents-dry:
    @just agents --dry-run

# rebuild the server image and restart it (data in ./data survives)
serve:
    docker compose up -d --build
    @sleep 6; curl -sf localhost:8737/health && echo

logs:
    docker compose logs -f --tail 50

# end-to-end drills against the built image — WIPES ./data
verify *args:
    ./scripts/verify.sh {{ args }}
