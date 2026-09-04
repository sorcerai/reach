#!/usr/bin/env bash
# Test & Verification Suite for Reach MicroVM and Sandbox Lifecycle Engine
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MICROVM="$REPO_DIR/scripts/microvm.sh"

BOLD='\033[1m'
GREEN='\033[0;32m'
RED='\033[0;31m'
CYAN='\033[0;36m'
NC='\033[0m'

pass() { echo -e "${GREEN}✔ [PASS]${NC} $1"; }
fail() { echo -e "${RED}✖ [FAIL]${NC} $1" >&2; exit 1; }
info() { echo -e "${CYAN}▶ [TEST]${NC} $1"; }

get_time_ms() {
    python3 -c "import time; print(int(time.time() * 1000))"
}

echo -e "${BOLD}═══════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}Reach MicroVM & Sandbox Lifecycle Verification Suite${NC}"
echo -e "${BOLD}═══════════════════════════════════════════════════════════${NC}"

# Check script exists and is executable
if [ ! -x "$MICROVM" ]; then
    fail "$MICROVM is not executable"
fi
pass "Lifecycle script found and executable"

# ═══════════════════════════════════════════════════════════
# 1. ORBSTACK MICROVM VERIFICATION
# ═══════════════════════════════════════════════════════════
if command -v orbctl >/dev/null 2>&1; then
    info "Running OrbStack MicroVM lifecycle tests..."
    TEST_VM="reach-test-verify-$$"

    # Step 1: Golden init
    info "Testing golden-init..."
    "$MICROVM" golden-init
    pass "golden-init completed successfully"

    # Step 2: Spawn
    info "Testing spawn ($TEST_VM)..."
    t0=$(get_time_ms)
    "$MICROVM" spawn "$TEST_VM"
    t1=$(get_time_ms)
    spawn_time=$((t1 - t0))
    echo "Total spawn + healthcheck elapsed: ${spawn_time}ms"
    if [ "$spawn_time" -gt 5000 ]; then
        fail "Spawn took longer than 5000ms ($spawn_time ms)"
    fi
    pass "spawn completed and healthy in ${spawn_time}ms"

    # Step 3: Healthcheck
    info "Testing standalone healthcheck..."
    "$MICROVM" healthcheck "$TEST_VM"
    pass "healthcheck returned healthy (supervisor + noVNC)"

    # Step 4: Dirty state injection
    info "Injecting dirty state into microVM..."
    "$MICROVM" exec "$TEST_VM" touch /tmp/reach_dirty_marker.txt
    if ! "$MICROVM" exec "$TEST_VM" test -f /tmp/reach_dirty_marker.txt; then
        fail "Failed to inject dirty state"
    fi
    pass "Dirty state injected (/tmp/reach_dirty_marker.txt)"

    # Step 5: Reset
    info "Testing instant reset..."
    t0=$(get_time_ms)
    "$MICROVM" reset "$TEST_VM"
    t1=$(get_time_ms)
    reset_time=$((t1 - t0))
    echo "Total reset elapsed: ${reset_time}ms"
    pass "reset completed in ${reset_time}ms"

    # Step 6: Verify dirty state was purged (pristine golden template restored)
    info "Verifying dirty state was completely purged..."
    if "$MICROVM" exec "$TEST_VM" test -f /tmp/reach_dirty_marker.txt; then
        fail "Dirty state persisted across reset! Pristine isolation failed."
    fi
    pass "Dirty state was completely wiped; golden template restored pristine"

    # Step 7: Nuke
    info "Testing nuke..."
    t0=$(get_time_ms)
    "$MICROVM" nuke "$TEST_VM"
    t1=$(get_time_ms)
    nuke_time=$((t1 - t0))
    echo "Total nuke elapsed: ${nuke_time}ms"
    if orbctl list 2>/dev/null | awk '{print $1}' | grep -qx "$TEST_VM"; then
        fail "Instance $TEST_VM still exists after nuke!"
    fi
    pass "nuke succeeded in ${nuke_time}ms with zero residue"
else
    echo "orbctl not found on this host; skipping OrbStack microVM test."
fi

# ═══════════════════════════════════════════════════════════
# 2. DOCKER SANDBOX VERIFICATION
# ═══════════════════════════════════════════════════════════
RUN_DOCKER=false
if command -v docker >/dev/null 2>&1; then
    if docker image inspect reach:latest >/dev/null 2>&1; then
        RUN_DOCKER=true
    fi
fi

if [ "$RUN_DOCKER" = true ]; then
    info "Running Docker Sandbox lifecycle tests..."
    TEST_CONTAINER="reach-test-docker-$$"

    # Step 1: Golden init
    info "Testing Docker golden-init..."
    MICROVM_MODE=docker "$MICROVM" golden-init
    pass "Docker golden-init completed"

    # Step 2: Spawn
    info "Testing Docker spawn ($TEST_CONTAINER)..."
    t0=$(get_time_ms)
    MICROVM_MODE=docker "$MICROVM" spawn "$TEST_CONTAINER"
    t1=$(get_time_ms)
    spawn_time=$((t1 - t0))
    echo "Total Docker spawn + healthcheck elapsed: ${spawn_time}ms"
    pass "Docker spawn completed in ${spawn_time}ms"

    # Step 3: Healthcheck
    info "Testing Docker healthcheck..."
    MICROVM_MODE=docker "$MICROVM" healthcheck "$TEST_CONTAINER"
    pass "Docker healthcheck returned healthy"

    # Step 4: Inject dirty marker
    info "Injecting dirty state into Docker container..."
    docker exec "$TEST_CONTAINER" touch /tmp/docker_dirty_marker.txt
    if ! docker exec "$TEST_CONTAINER" test -f /tmp/docker_dirty_marker.txt; then
        fail "Failed to inject dirty state into container"
    fi
    pass "Dirty state injected"

    # Step 5: Reset
    info "Testing Docker reset..."
    t0=$(get_time_ms)
    MICROVM_MODE=docker "$MICROVM" reset "$TEST_CONTAINER"
    t1=$(get_time_ms)
    reset_time=$((t1 - t0))
    echo "Total Docker reset elapsed: ${reset_time}ms"
    pass "Docker reset completed in ${reset_time}ms"

    # Step 6: Verify dirty state purged
    info "Verifying dirty state was purged..."
    if docker exec "$TEST_CONTAINER" test -f /tmp/docker_dirty_marker.txt; then
        fail "Dirty state persisted across container reset!"
    fi
    pass "Docker container restored pristine from golden image"

    # Step 7: Nuke
    info "Testing Docker nuke..."
    t0=$(get_time_ms)
    MICROVM_MODE=docker "$MICROVM" nuke "$TEST_CONTAINER"
    t1=$(get_time_ms)
    nuke_time=$((t1 - t0))
    echo "Total Docker nuke elapsed: ${nuke_time}ms"
    if docker ps -a --format '{{.Names}}' | grep -qx "$TEST_CONTAINER"; then
        fail "Container $TEST_CONTAINER still exists after nuke!"
    fi
    pass "Docker container nuked in ${nuke_time}ms"
fi

echo -e "${BOLD}═══════════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}${BOLD}ALL MICROVM & SANDBOX LIFECYCLE TESTS PASSED!${NC}"
echo -e "${BOLD}═══════════════════════════════════════════════════════════${NC}"
