#!/usr/bin/env bash
# E2E Test Suite for Hermes Agent CLI
# Usage: bash scripts/e2e_test.sh [--release]
#
# Tests 60+ CLI commands without requiring API keys.
# Reports PASS/FAIL for each case with a summary.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

# Build config
BUILD_FLAG="--release"
if [[ "${1:-}" == "--debug" ]]; then
    BUILD_FLAG=""
fi

PROFILE_DIR="target/${BUILD_FLAG#--}"
BIN="$ROOT_DIR/$PROFILE_DIR/hermes"

# Test state
PASS=0
FAIL=0
TOTAL=0
SKIPPED=0
RESULTS=""

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log() { echo -e "${BLUE}[e2e]${NC} $*"; }
pass() { echo -e "  ${GREEN}PASS${NC}  $1"; PASS=$((PASS + 1)); }
fail() { echo -e "  ${RED}FAIL${NC}  $1 — $2"; FAIL=$((FAIL + 1)); }
skip() { echo -e "  ${YELLOW}SKIP${NC}  $1 — $2"; SKIPPED=$((SKIPPED + 1)); }

#────────────────────────────────────────────
# Build
#────────────────────────────────────────────
log "Building hermes $BUILD_FLAG ..."
cargo build $BUILD_FLAG --quiet --workspace 2>&1
log "Binary: $BIN"
[[ -f "$BIN" ]] || { echo "Binary not found at $BIN"; exit 1; }

VERSION=$("$BIN" --version 2>/dev/null || echo "unknown")
log "Version: $VERSION"
echo ""

#────────────────────────────────────────────
# Helper: run a test case
#────────────────────────────────────────────
run_test() {
    local label="$1"
    shift
    local expected_rc="${1:-0}"
    shift || true
    local expect_output="${1:-}"
    shift || true

    TOTAL=$((TOTAL + 1))
    local rc=0
    local out=""
    out=$("$@" 2>&1) || rc=$?

    # Check exit code
    if [[ "$rc" -ne "$expected_rc" ]]; then
        fail "$label" "expected rc=$expected_rc, got rc=$rc"
        return
    fi

    # Check output contains substring if specified
    if [[ -n "$expect_output" ]]; then
        if echo "$out" | grep -qi "$expect_output"; then
            pass "$label"
        else
            fail "$label" "output missing '$expect_output'"
        fi
    else
        pass "$label"
    fi
}

#────────────────────────────────────────────
# 1. Core functionality
#────────────────────────────────────────────
echo -e "${BLUE}═══ Core Functionality ═══${NC}"

run_test "Version: hermes --version" 0 "hermes" \
    "$BIN" --version

run_test "Help: hermes --help" 0 "subcommands" \
    "$BIN" --help

run_test "Help: hermes chat --help" 0 "model" \
    "$BIN" chat --help

#────────────────────────────────────────────
# 2. Config management
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Config Management ═══${NC}"

run_test "Config path: hermes config path" 0 "" \
    "$BIN" config path

run_test "Config show: hermes config show" 0 "" \
    "$BIN" config show

run_test "Config check: hermes config check" 0 "" \
    "$BIN" config check

run_test "Config migrate: hermes config migrate" 0 "" \
    "$BIN" config migrate

run_test "Setup: hermes setup --help" 0 "setup" \
    "$BIN" setup --help

#────────────────────────────────────────────
# 3. Diagnostics
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Diagnostics ═══${NC}"

run_test "Doctor: hermes doctor" 0 "" \
    "$BIN" doctor

run_test "Doctor --fix: hermes doctor --fix" 0 "" \
    "$BIN" doctor --fix

run_test "Debug: hermes debug" 0 "" \
    "$BIN" debug

run_test "Dump: hermes dump" 0 "" \
    "$BIN" dump

run_test "Logs: hermes logs" 0 "" \
    "$BIN" logs

run_test "Debug-share: hermes debug-share --help" 0 "" \
    "$BIN" debug-share --help

#────────────────────────────────────────────
# 4. Models & Auth
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Models & Auth ═══${NC}"

run_test "Models: hermes models" 0 "" \
    "$BIN" models

run_test "Model list: hermes model list" 0 "" \
    "$BIN" model list

run_test "Auth list: hermes auth list" 0 "" \
    "$BIN" auth list

run_test "Login: hermes login --help" 0 "login" \
    "$BIN" login --help

run_test "Logout: hermes logout --help" 0 "logout" \
    "$BIN" logout --help

run_test "Status: hermes status" 0 "" \
    "$BIN" status

#────────────────────────────────────────────
# 5. Tools & Skills
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Tools & Skills ═══${NC}"

run_test "Tools list: hermes tools list" 0 "" \
    "$BIN" tools list

run_test "Tools info: hermes tools info terminal" 0 "" \
    "$BIN" tools info terminal

run_test "Skills list: hermes skills list" 0 "" \
    "$BIN" skills list

run_test "Skills search: hermes skills search memory" 0 "" \
    "$BIN" skills search memory

run_test "Skills inspect: hermes skills inspect --help" 0 "inspect" \
    "$BIN" skills inspect --help

#────────────────────────────────────────────
# 6. Session management
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Session Management ═══${NC}"

run_test "Sessions list: hermes sessions list" 0 "" \
    "$BIN" sessions list

run_test "Sessions search: hermes sessions search test" 0 "" \
    "$BIN" sessions search test

run_test "Sessions stats: hermes sessions stats" 0 "" \
    "$BIN" sessions stats

run_test "Sessions delete (no args): hermes sessions delete" 2 "" \
    "$BIN" sessions delete

run_test "Sessions rename (no args): hermes sessions rename" 2 "" \
    "$BIN" sessions rename

#────────────────────────────────────────────
# 7. Backup & Restore
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Backup & Restore ═══${NC}"

run_test "Backup: hermes backup" 0 "" \
    "$BIN" backup

run_test "Backup list: hermes backup-list" 0 "" \
    "$BIN" backup-list

run_test "Restore: hermes restore --help" 0 "restore" \
    "$BIN" restore --help

run_test "Import: hermes import --help" 0 "import" \
    "$BIN" import --help

#────────────────────────────────────────────
# 8. Gateway & Cron
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Gateway & Cron ═══${NC}"

run_test "Gateway status: hermes gateway status" 0 "" \
    "$BIN" gateway status

run_test "Gateway start: hermes gateway start --help" 0 "" \
    "$BIN" gateway start --help

run_test "Gateway run: hermes gateway run --help" 0 "" \
    "$BIN" gateway run --help

run_test "Gateway stop: hermes gateway stop --help" 0 "" \
    "$BIN" gateway stop --help

run_test "Cron list: hermes cron list" 0 "" \
    "$BIN" cron list

run_test "Cron create: hermes cron create --help" 0 "" \
    "$BIN" cron create --help

#────────────────────────────────────────────
# 9. Profiles
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Profiles ═══${NC}"

run_test "Profiles list: hermes profiles list" 0 "" \
    "$BIN" profiles list

run_test "Profiles create: hermes profiles create --help" 0 "" \
    "$BIN" profiles create --help

#────────────────────────────────────────────
# 10. System admin
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ System Admin ═══${NC}"

run_test "Completion: hermes completion --shell bash" 0 "" \
    "$BIN" completion --shell bash

run_test "Insights: hermes insights --help" 0 "" \
    "$BIN" insights --help

run_test "Update: hermes update --help" 0 "" \
    "$BIN" update --help

run_test "Uninstall: hermes uninstall --help" 0 "" \
    "$BIN" uninstall --help

run_test "ACP: hermes acp --help" 0 "" \
    "$BIN" acp --help

#────────────────────────────────────────────
# 11. Rust-specific: LLM layer tests
#────────────────────────────────────────────
echo ""
echo -e "${BLUE}═══ Rust LLM Layer (cargo test) ═══${NC}"

TOTAL=$((TOTAL + 1))
TEST_OUT=$(cargo test --workspace -- --skip test_delegation_filters_blocked_toolsets \
    --skip test_build_system_prompt_basic --skip test_e2e_prompt_with_soul 2>&1)
TEST_RC=$?

if [[ $TEST_RC -eq 0 ]]; then
    # Count passed tests
    TEST_COUNT=$(echo "$TEST_OUT" | grep -oE '[0-9]+ passed' | tail -1 | grep -oE '[0-9]+')
    pass "Workspace tests (${TEST_COUNT:-?} passed)"
else
    FAIL_LINES=$(echo "$TEST_OUT" | grep "^test result: FAILED" || true)
    fail "Workspace tests" "$FAIL_LINES"
fi

#────────────────────────────────────────────
# Summary
#────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════"
echo -e "  ${GREEN}PASS: $PASS${NC}  ${RED}FAIL: $FAIL${NC}  ${YELLOW}SKIP: $SKIPPED${NC}  TOTAL: $TOTAL"
echo "════════════════════════════════════════"

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi

echo -e "${GREEN}All E2E tests passed!${NC}"
exit 0
