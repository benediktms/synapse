bin_dir := env('HOME', env('USERPROFILE', '')) / '.local/bin'
syn := justfile_directory() / 'target/release/syn'

set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

_default:
    @just --list

# build the release binaries
build:
    cargo build --release

# symlink syn + synd into ~/.local/bin and load the daemon unit (rebuilds first)
[unix]
install: build
    mkdir -p {{ bin_dir }}
    ln -sf {{ syn }} {{ bin_dir }}/syn
    ln -sf {{ syn }}d {{ bin_dir }}/synd
    @{{ bin_dir }}/syn --version
    {{ bin_dir }}/syn daemon install

# copy syn + synd into ~/.local/bin and register the task; the stop must precede
# the copy because Windows refuses to overwrite a running executable image
[windows]
install: build
    New-Item -ItemType Directory -Force -ErrorAction Stop "{{ bin_dir }}" | Out-Null
    $installed = "{{ bin_dir }}\synd.exe"; if (Test-Path -LiteralPath $installed) { & "{{ syn }}.exe" daemon stop }
    Copy-Item -Force -ErrorAction Stop "{{ syn }}.exe" "{{ bin_dir }}\syn.exe"
    Copy-Item -Force -ErrorAction Stop "{{ syn }}d.exe" "{{ bin_dir }}\synd.exe"
    & "{{ bin_dir }}\syn.exe" daemon install

# unload the daemon unit and remove the ~/.local/bin symlinks
[unix]
uninstall:
    if [ -x {{ bin_dir }}/syn ]; then {{ bin_dir }}/syn daemon uninstall; \
    else echo "syn not installed; skipping daemon uninstall"; fi
    rm -f {{ bin_dir }}/syn {{ bin_dir }}/synd

# unregister the task, then remove the installed executables
[windows]
uninstall:
    $syn = "{{ bin_dir }}\syn.exe"; if (Test-Path -LiteralPath $syn) { & $syn daemon uninstall } else { Write-Host "syn not installed; skipping daemon uninstall" }
    @("{{ bin_dir }}\syn.exe", "{{ bin_dir }}\synd.exe") | Where-Object { Test-Path -LiteralPath $_ } | Remove-Item -Force -ErrorAction Stop

test:
    cargo test --workspace

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# everything CI would check
check: lint test

# install the skill and session hooks into every harness — Claude Code, Codex CLI,
# Copilot CLI and Oh My Pi. Narrow it with `just agents --harness claude`.
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
