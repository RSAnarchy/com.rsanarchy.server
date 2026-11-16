#!/usr/bin/env bash
# Builds horizon-server and horizon-client (the engine that runs the world) for Linux with the
# "fast" profile. Run from the repository, or from anywhere with the path to it.
#
#   scripts/build-linux.sh [--skip-packages] [--jobs N]
#
# Needs about 20 GiB free and, for a comfortable build, 8 GiB of RAM. Parallel jobs default to
# the smaller of the CPU count and half the RAM in GiB.
set -euo pipefail

SKIP_PACKAGES=0
JOBS=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-packages) SKIP_PACKAGES=1 ;;
        --jobs) JOBS="$2"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

REPO="$(cd "$(dirname "$0")/.." && pwd)"
[[ -f "$REPO/Cargo.toml" ]] || { echo "no Cargo.toml in $REPO" >&2; exit 1; }
MIN_RUST="1.95"
BINARIES=(horizon-server horizon-client)

step() { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

# 1. Preflight -----------------------------------------------------------------
step "Preflight"
[[ "$(uname -s)" == "Linux" ]] || fail "this script builds for Linux; run it inside Linux or WSL."
RAM_GIB=$(awk '/MemTotal/ { printf "%.1f", $2 / 1048576 }' /proc/meminfo)
CORES=$(nproc)
FREE_GIB=$(df -Pk "$REPO" | awk 'NR == 2 { printf "%.1f", $4 / 1048576 }')
info "RAM ${RAM_GIB} GiB, ${CORES} threads, ${FREE_GIB} GiB free at the repo"
awk -v a="$RAM_GIB" 'BEGIN { exit !(a < 8) }' && info "WARNING: under 8 GiB RAM; the build may run out of memory."
awk -v a="$FREE_GIB" 'BEGIN { exit !(a < 20) }' && fail "need about 20 GiB free; ${FREE_GIB} GiB available."
if [[ -z "$JOBS" ]]; then
    HALF_RAM=$(awk -v r="$RAM_GIB" 'BEGIN { printf "%d", r / 2 }')
    JOBS=$(( CORES < HALF_RAM ? CORES : HALF_RAM ))
    (( JOBS < 1 )) && JOBS=1
fi
info "cargo jobs: $JOBS"

SUDO=""
[[ "$(id -u)" -eq 0 ]] || SUDO="sudo"

# 2. System packages -----------------------------------------------------------
if [[ "$SKIP_PACKAGES" -eq 0 ]]; then
    step "System packages"
    export DEBIAN_FRONTEND=noninteractive
    $SUDO apt-get update -q
    $SUDO apt-get install -y -q \
        build-essential pkg-config git curl ca-certificates \
        libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev \
        libx11-dev libxi-dev libxrandr-dev libxcursor-dev libxinerama-dev libvulkan-dev
else
    info "skipping packages (--skip-packages)"
fi

# 3. Rust ----------------------------------------------------------------------
step "Rust toolchain"
[[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
if ! command -v cargo >/dev/null 2>&1; then
    info "installing rustup (stable, minimal profile)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
    . "$HOME/.cargo/env"
fi
RUST_VERSION=$(rustc --version | awk '{ print $2 }')
if ! awk -v have="$RUST_VERSION" -v want="$MIN_RUST" 'BEGIN {
        split(have, h, "."); split(want, w, ".");
        exit !(h[1] > w[1] || (h[1] == w[1] && h[2] >= w[2])) }'; then
    info "rustc $RUST_VERSION is older than $MIN_RUST; updating stable"
    rustup toolchain install stable --profile minimal
    rustup default stable
fi
info "rustc $(rustc --version | awk '{ print $2 }')"

# 4. Build ---------------------------------------------------------------------
step "Building ${BINARIES[*]} (profile fast)"
cd "$REPO"
LOG="$REPO/build-linux-$(date +%Y%m%d-%H%M%S).log"
ARGS=(build --locked --profile fast)
for bin in "${BINARIES[@]}"; do ARGS+=(-p "$bin"); done
if ! CARGO_BUILD_JOBS="$JOBS" cargo "${ARGS[@]}" >"$LOG" 2>&1; then
    tail -n 30 "$LOG" >&2
    fail "cargo build failed. Full log: $LOG"
fi
info "log: $LOG"

# 5. Check --------------------------------------------------------------------
step "Binaries"
for bin in "${BINARIES[@]}"; do
    path="$REPO/target/fast/$bin"
    [[ -f "$path" ]] || fail "build output missing: $path"
    chmod +x "$path"
    info "$(sha256sum "$path")"
done
info "done. Binaries are in $REPO/target/fast/"
