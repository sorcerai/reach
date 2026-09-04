#!/usr/bin/env bash
# Reach MicroVM & Sandbox Instant Lifecycle Engine (Nuke & Restart)
# Supports OrbStack microVMs (sub-second CoW clones) and Docker sandboxes.

set -euo pipefail

GOLDEN_VM="${MICROVM_GOLDEN_VM:-reach-golden}"
SOURCE_VM="${MICROVM_SOURCE_VM:-reach-lab}"
GOLDEN_IMAGE="${MICROVM_GOLDEN_IMAGE:-reach:golden}"
BASE_IMAGE="${MICROVM_BASE_IMAGE:-reach:latest}"
MODE="${MICROVM_MODE:-auto}"

# Styling
BOLD='\033[1m'
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
PURPLE='\033[0;35m'
CYAN='\033[0;36m'
NC='\033[0m'

log() { echo -e "${CYAN}▶${NC} $1"; }
success() { echo -e "${GREEN}✔${NC} $1"; }
warn() { echo -e "${YELLOW}▲${NC} $1"; }
error() { echo -e "${RED}✖${NC} $1" >&2; }

detect_mode() {
    if [ "$MODE" != "auto" ]; then
        echo "$MODE"
        return
    fi

    if command -v orbctl >/dev/null 2>&1; then
        echo "orb"
    elif command -v docker >/dev/null 2>&1; then
        echo "docker"
    else
        error "Neither orbctl nor docker found on system."
        exit 1
    fi
}

RESOLVED_MODE="$(detect_mode)"

# Time helper in milliseconds
get_time_ms() {
    python3 -c "import time; print(int(time.time() * 1000))"
}

# ═══════════════════════════════════════════════════════════
# ORBSTACK MICROVM ENGINE
# ═══════════════════════════════════════════════════════════

orb_golden_init() {
    local force="${1:-false}"
    log "Initializing OrbStack Golden MicroVM template [${BOLD}${GOLDEN_VM}${NC}]..."

    if orb_machine_exists "$GOLDEN_VM"; then
        if [ "$force" = true ]; then
            log "Force requested. Nuking existing golden template..."
            orbctl delete -f "$GOLDEN_VM"
        else
            success "Golden template [${GOLDEN_VM}] already exists and is ready."
            # Ensure it is stopped so cloning is immediate and safe
            local state
            state="$(orbctl info "$GOLDEN_VM" 2>/dev/null | { grep -E '^State:' || true; } | awk '{print $2}')"
            if [ "$state" = "running" ]; then
                log "Stopping golden template to lock clean state..."
                orbctl stop "$GOLDEN_VM"
            fi
            return 0
        fi
    fi

    # Verify source machine exists
    if ! orb_machine_exists "$SOURCE_VM"; then
        error "Source VM [${SOURCE_VM}] not found. Cannot clone golden template."
        error "Available machines: $(orbctl list -q | tr '\n' ' ')"
        exit 1
    fi

    log "Cloning from [${SOURCE_VM}] to create golden template [${GOLDEN_VM}]..."
    local t0 t1 duration
    t0=$(get_time_ms)
    orbctl clone "$SOURCE_VM" "$GOLDEN_VM"
    # Ensure golden is stopped
    orbctl stop "$GOLDEN_VM" 2>/dev/null || true
    t1=$(get_time_ms)
    duration=$((t1 - t0))

    success "Golden template [${GOLDEN_VM}] created in ${BOLD}${duration}ms${NC}."
}

orb_machine_exists() {
    local name="$1"
    local found
    found="$(orbctl list 2>/dev/null | awk '{print $1}' | grep -x "$name" || true)"
    [ -n "$found" ]
}

orb_get_ip() {
    local name="$1"
    orbctl info "$name" 2>/dev/null | { grep -E '^IPv4:' || true; } | awk '{print $2}'
}

orb_spawn() {
    local name="$1"
    log "Spawning fresh microVM instance [${BOLD}${name}${NC}] from [${GOLDEN_VM}]..."

    # Check if instance already exists
    if orb_machine_exists "$name"; then
        warn "Instance [${name}] already exists. Nuking first..."
        orbctl delete -f "$name"
    fi

    # Ensure golden template exists
    if ! orb_machine_exists "$GOLDEN_VM"; then
        orb_golden_init
    fi

    local t0 t_clone t_start t1 d_clone d_start d_total
    t0=$(get_time_ms)

    # 1. Sub-second APFS CoW clone
    orbctl clone "$GOLDEN_VM" "$name"
    t_clone=$(get_time_ms)
    d_clone=$((t_clone - t0))

    # 2. Instant boot
    orbctl start "$name"
    t_start=$(get_time_ms)
    d_start=$((t_start - t_clone))
    d_total=$((t_start - t0))

    # 3. Resolve dedicated IP address
    local ip=""
    for _ in {1..30}; do
        ip="$(orb_get_ip "$name")"
        if [ -n "$ip" ]; then break; fi
        sleep 0.1
    done

    if [ -z "$ip" ]; then
        error "Failed to acquire IP for [${name}]."
        exit 1
    fi

    local novnc_url="http://${ip}:6080/vnc.html?autoconnect=1&resize=remote"
    local health_url="http://${ip}:8400/health"

    success "MicroVM [${BOLD}${name}${NC}] spawned in ${BOLD}${d_total}ms${NC} (clone: ${d_clone}ms, start: ${d_start}ms)!"
    echo -e "  ${PURPLE}•${NC} IPv4:       ${BOLD}${ip}${NC}"
    echo -e "  ${PURPLE}•${NC} noVNC:      ${BLUE}${novnc_url}${NC}"
    echo -e "  ${PURPLE}•${NC} Supervisor: ${BLUE}${health_url}${NC}"

    # Perform healthcheck
    orb_healthcheck "$name" "$ip"
}

orb_healthcheck() {
    local name="$1"
    local ip="${2:-}"
    if [ -z "$ip" ]; then
        ip="$(orb_get_ip "$name")"
    fi

    if [ -z "$ip" ]; then
        error "Could not resolve IP for [${name}]."
        return 1
    fi

    log "Running healthcheck on [${name}] (${ip})..."

    local healthy=false
    local attempts=0
    local max_attempts=25

    while [ $attempts -lt $max_attempts ]; do
        attempts=$((attempts + 1))
        # Check supervisor health JSON
        local health_resp
        health_resp="$(curl -s --connect-timeout 1 "http://${ip}:8400/health" 2>/dev/null || true)"
        if echo "$health_resp" | grep -q '"status":"healthy"'; then
            # Check noVNC HTTP 200
            local vnc_resp
            vnc_resp="$(curl -sI --connect-timeout 1 "http://${ip}:6080/vnc.html" 2>/dev/null | head -n 1 || true)"
            if echo "$vnc_resp" | grep -q '200 OK'; then
                healthy=true
                break
            fi
        fi
        sleep 0.2
    done

    if [ "$healthy" = true ]; then
        success "Healthcheck PASSED for [${name}] in $((attempts * 200))ms! (Supervisor + noVNC active)"
        return 0
    else
        error "Healthcheck FAILED for [${name}] after ${max_attempts} attempts."
        return 1
    fi
}

orb_nuke() {
    local name="$1"
    log "Nuking microVM instance [${BOLD}${name}${NC}]..."

    local t0 t1 duration
    t0=$(get_time_ms)
    orbctl delete -f "$name"
    t1=$(get_time_ms)
    duration=$((t1 - t0))

    success "MicroVM [${BOLD}${name}${NC}] obliterated in ${BOLD}${duration}ms${NC} (zero residue)."
}

orb_reset() {
    local name="$1"
    log "Executing instant RESET on [${BOLD}${name}${NC}]..."
    local t0 t1 duration
    t0=$(get_time_ms)

    orb_nuke "$name"
    orb_spawn "$name"

    t1=$(get_time_ms)
    duration=$((t1 - t0))
    success "Instant RESET of [${BOLD}${name}${NC}] completed in ${BOLD}${duration}ms${NC}!"
}

orb_list() {
    log "Listing Reach MicroVMs:"
    orbctl list | grep -E 'reach' || true
}

orb_exec() {
    local name="$1"
    shift
    orb -m "$name" "$@"
}

# ═══════════════════════════════════════════════════════════
# DOCKER SANDBOX ENGINE (INSIDE REACH-LAB OR LOCAL)
# ═══════════════════════════════════════════════════════════

docker_golden_init() {
    local force="${1:-false}"
    log "Initializing Docker Golden image [${BOLD}${GOLDEN_IMAGE}${NC}]..."

    if docker image inspect "$GOLDEN_IMAGE" >/dev/null 2>&1 && [ "$force" != true ]; then
        success "Golden Docker image [${GOLDEN_IMAGE}] already exists."
        return 0
    fi

    if ! docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
        error "Base image [${BASE_IMAGE}] not found."
        exit 1
    fi

    docker tag "$BASE_IMAGE" "$GOLDEN_IMAGE"
    success "Tagged [${BASE_IMAGE}] as golden template [${GOLDEN_IMAGE}]."
}

find_available_port() {
    local start_port="$1"
    python3 -c "
import socket, sys
port = int(sys.argv[1])
while port < 65535:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.bind(('0.0.0.0', port))
        s.close()
        print(port)
        break
    except OSError:
        port += 1
" "$start_port"
}

docker_spawn() {
    local name="$1"
    local vnc_port="${2:-}"
    local novnc_port="${3:-}"
    local health_port="${4:-}"

    if [ -z "$vnc_port" ]; then
        vnc_port="$(find_available_port 5900)"
    fi
    if [ -z "$novnc_port" ]; then
        novnc_port="$(find_available_port 6080)"
    fi
    if [ -z "$health_port" ]; then
        health_port="$(find_available_port 8400)"
    fi

    log "Spawning fresh Docker sandbox [${BOLD}${name}${NC}] (ports: ${vnc_port}, ${novnc_port}, ${health_port})..."

    if docker ps -a --format '{{.Names}}' | grep -qx "$name"; then
        warn "Container [${name}] already exists. Nuking first..."
        docker rm -f "$name" >/dev/null 2>&1
    fi

    docker_golden_init

    local t0 t1 duration
    t0=$(get_time_ms)

    local cid
    cid="$(docker run -d \
        --name "$name" \
        -p "${vnc_port}:5900" \
        -p "${novnc_port}:6080" \
        -p "${health_port}:8400" \
        "$GOLDEN_IMAGE")"

    t1=$(get_time_ms)
    duration=$((t1 - t0))

    local novnc_url="http://localhost:${novnc_port}/vnc.html?autoconnect=1&resize=remote"
    local health_url="http://localhost:${health_port}/health"

    success "Docker sandbox [${BOLD}${name}${NC}] spawned in ${BOLD}${duration}ms${NC}!"
    echo -e "  ${PURPLE}•${NC} Container:   ${BOLD}${cid:0:12}${NC}"
    echo -e "  ${PURPLE}•${NC} noVNC:       ${BLUE}${novnc_url}${NC}"
    echo -e "  ${PURPLE}•${NC} Supervisor:  ${BLUE}${health_url}${NC}"

    docker_healthcheck "$name" "$health_port" "$novnc_port"
}

docker_healthcheck() {
    local name="$1"
    local health_port="${2:-}"
    local novnc_port="${3:-}"

    if [ -z "$health_port" ]; then
        health_port="$(docker port "$name" 8400/tcp 2>/dev/null | head -n 1 | awk -F: '{print $NF}' || true)"
        if [ -z "$health_port" ]; then health_port="8400"; fi
    fi
    if [ -z "$novnc_port" ]; then
        novnc_port="$(docker port "$name" 6080/tcp 2>/dev/null | head -n 1 | awk -F: '{print $NF}' || true)"
        if [ -z "$novnc_port" ]; then novnc_port="6080"; fi
    fi

    log "Running healthcheck on [${name}] (ports: ${health_port}/${novnc_port})..."

    local healthy=false
    local attempts=0
    local max_attempts=25

    while [ $attempts -lt $max_attempts ]; do
        attempts=$((attempts + 1))
        local health_resp
        health_resp="$(curl -s --connect-timeout 1 "http://localhost:${health_port}/health" 2>/dev/null || true)"
        if echo "$health_resp" | grep -q '"status":"healthy"'; then
            local vnc_resp
            vnc_resp="$(curl -sI --connect-timeout 1 "http://localhost:${novnc_port}/vnc.html" 2>/dev/null | head -n 1 || true)"
            if echo "$vnc_resp" | grep -q '200 OK'; then
                healthy=true
                break
            fi
        fi
        sleep 0.2
    done

    if [ "$healthy" = true ]; then
        success "Healthcheck PASSED for [${name}]! (Supervisor + noVNC active)"
        return 0
    else
        error "Healthcheck FAILED for [${name}] after ${max_attempts} attempts."
        return 1
    fi
}

docker_nuke() {
    local name="$1"
    log "Nuking Docker sandbox [${BOLD}${name}${NC}]..."

    local t0 t1 duration
    t0=$(get_time_ms)
    docker rm -f "$name" >/dev/null 2>&1 || true
    t1=$(get_time_ms)
    duration=$((t1 - t0))

    success "Docker sandbox [${BOLD}${name}${NC}] nuked in ${BOLD}${duration}ms${NC}."
}

docker_reset() {
    local name="$1"
    log "Resetting Docker sandbox [${BOLD}${name}${NC}]..."
    local t0 t1 duration
    t0=$(get_time_ms)

    local vnc_port=""
    local novnc_port=""
    local health_port=""
    if docker ps -a --format '{{.Names}}' | grep -qx "$name"; then
        vnc_port="$(docker port "$name" 5900/tcp 2>/dev/null | head -n 1 | awk -F: '{print $NF}' || true)"
        novnc_port="$(docker port "$name" 6080/tcp 2>/dev/null | head -n 1 | awk -F: '{print $NF}' || true)"
        health_port="$(docker port "$name" 8400/tcp 2>/dev/null | head -n 1 | awk -F: '{print $NF}' || true)"
    fi

    docker_nuke "$name"
    docker_spawn "$name" "$vnc_port" "$novnc_port" "$health_port"

    t1=$(get_time_ms)
    duration=$((t1 - t0))
    success "Docker sandbox [${BOLD}${name}${NC}] reset completed in ${BOLD}${duration}ms${NC}!"
}

docker_list() {
    log "Listing Reach Docker Sandboxes:"
    docker ps --filter "ancestor=${GOLDEN_IMAGE}" --filter "ancestor=${BASE_IMAGE}"
}

docker_exec() {
    local name="$1"
    shift
    docker exec -it "$name" "$@"
}

# ═══════════════════════════════════════════════════════════
# CLI DISPATCH
# ═══════════════════════════════════════════════════════════

usage() {
    cat << EOF
${BOLD}Reach MicroVM & Sandbox Lifecycle Engine${NC}
Mode: ${BOLD}${RESOLVED_MODE}${NC} (Use MICROVM_MODE=orb|docker to override)

${BOLD}Usage:${NC}
  $(basename "$0") golden-init [--force]          Initialize the pristine golden template
  $(basename "$0") spawn <instance-name>         Instant clone and start (<2s)
  $(basename "$0") fork <instance-name>          Alias for spawn
  $(basename "$0") healthcheck <instance-name>   Verify supervisor /health & noVNC
  $(basename "$0") nuke <instance-name>          Instantly destroy instance without residue
  $(basename "$0") reset <instance-name>         Nuke and immediately spawn a fresh pristine copy
  $(basename "$0") list                          List active instances
  $(basename "$0") exec <instance-name> <cmd...> Run command inside instance
  $(basename "$0") vnc <instance-name>           Show or open live stream URL

${BOLD}Examples:${NC}
  ./scripts/microvm.sh golden-init
  ./scripts/microvm.sh spawn agent-42
  ./scripts/microvm.sh healthcheck agent-42
  ./scripts/microvm.sh reset agent-42
  ./scripts/microvm.sh nuke agent-42
EOF
    exit 1
}

CMD="${1:-help}"
shift || true

case "$CMD" in
    golden-init)
        FORCE=false
        if [ "${1:-}" = "--force" ]; then FORCE=true; fi
        if [ "$RESOLVED_MODE" = "orb" ]; then
            orb_golden_init "$FORCE"
        else
            docker_golden_init "$FORCE"
        fi
        ;;
    spawn|fork)
        NAME="${1:-}"
        if [ -z "$NAME" ]; then error "Instance name required."; usage; fi
        if [ "$RESOLVED_MODE" = "orb" ]; then
            orb_spawn "$NAME"
        else
            docker_spawn "$NAME" "${2:-}" "${3:-}" "${4:-}"
        fi
        ;;
    healthcheck)
        NAME="${1:-}"
        if [ -z "$NAME" ]; then error "Instance name required."; usage; fi
        if [ "$RESOLVED_MODE" = "orb" ]; then
            orb_healthcheck "$NAME"
        else
            docker_healthcheck "$NAME"
        fi
        ;;
    nuke)
        NAME="${1:-}"
        if [ -z "$NAME" ]; then error "Instance name required."; usage; fi
        if [ "$RESOLVED_MODE" = "orb" ]; then
            orb_nuke "$NAME"
        else
            docker_nuke "$NAME"
        fi
        ;;
    reset)
        NAME="${1:-}"
        if [ -z "$NAME" ]; then error "Instance name required."; usage; fi
        if [ "$RESOLVED_MODE" = "orb" ]; then
            orb_reset "$NAME"
        else
            docker_reset "$NAME"
        fi
        ;;
    list)
        if [ "$RESOLVED_MODE" = "orb" ]; then
            orb_list
        else
            docker_list
        fi
        ;;
    exec)
        NAME="${1:-}"
        shift || true
        if [ -z "$NAME" ]; then error "Instance name required."; usage; fi
        if [ "$RESOLVED_MODE" = "orb" ]; then
            orb_exec "$NAME" "$@"
        else
            docker_exec "$NAME" "$@"
        fi
        ;;
    vnc)
        NAME="${1:-}"
        if [ -z "$NAME" ]; then error "Instance name required."; usage; fi
        if [ "$RESOLVED_MODE" = "orb" ]; then
            IP="$(orb_get_ip "$NAME")"
            echo "http://${IP}:6080/vnc.html?autoconnect=1&resize=remote"
        else
            echo "http://localhost:6080/vnc.html?autoconnect=1&resize=remote"
        fi
        ;;
    *)
        usage
        ;;
esac
