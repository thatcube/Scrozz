#!/usr/bin/env bash
# Run one command with a lease on a bounded Cargo target pool.
#
# Cargo's stable target directory is not a concurrent cross-workspace cache.
# Pointing every worktree at one directory can corrupt or misidentify runnable
# artifacts, while giving every worktree its own directory duplicates gigabytes
# of dependencies. This wrapper keeps a small fixed number of target roots,
# permits exactly one command in each root at a time, and invalidates workspace
# packages whenever a slot changes worktrees. External dependencies remain warm.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  tools/cargo-pool.sh <command> [argument...]
  tools/cargo-pool.sh --status

Environment:
  SCROZZ_CARGO_POOL_ROOT          pool location (must be a local filesystem)
  SCROZZ_CARGO_POOL_SLOTS         fixed concurrency/space bound (default: 2)
  SCROZZ_CARGO_POOL_WAIT_SECONDS  lease wait before failing (default: 900)

An explicit CARGO_TARGET_DIR bypasses the pool. CI also keeps its job-local
target directory. Stale leases are reported but never removed automatically.
USAGE
}

default_pool_root() {
  case "$(uname -s 2>/dev/null || true)" in
    Darwin)
      printf '%s\n' "$HOME/Library/Caches/com.thatcube.Scrozz/cargo-pool"
      ;;
    MINGW* | MSYS* | CYGWIN*)
      printf '%s\n' "${LOCALAPPDATA:-$HOME/.cache}/Scrozz/cargo-pool"
      ;;
    *)
      printf '%s\n' "${XDG_CACHE_HOME:-$HOME/.cache}/scrozz/cargo-pool"
      ;;
  esac
}

ROOT="${SCROZZ_CARGO_POOL_ROOT:-$(default_pool_root)}"
SLOTS="${SCROZZ_CARGO_POOL_SLOTS:-2}"
WAIT_SECONDS="${SCROZZ_CARGO_POOL_WAIT_SECONDS:-900}"

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ "${1:-}" != "--status" ]]; then
  if [[ "$#" -eq 0 ]]; then
    usage >&2
    exit 2
  fi
  if [[ "$1" == "--" ]]; then
    shift
  fi
  if [[ "$#" -eq 0 ]]; then
    usage >&2
    exit 2
  fi

  # Hosted jobs already have one isolated workspace and lifecycle-managed
  # cache. A caller-provided target directory is likewise an explicit ownership
  # decision and must not depend on pool configuration being available.
  if [[ "${SCROZZ_CARGO_LEASE_HELD:-0}" == "1" ||
        "${CI:-}" == "true" ||
        "${GITHUB_ACTIONS:-}" == "true" ||
        -n "${CARGO_TARGET_DIR:-}" ]]; then
    exec "$@"
  fi
fi

case "$SLOTS" in
  "" | *[!0-9]*)
    echo "cargo-pool: SCROZZ_CARGO_POOL_SLOTS must be an integer" >&2
    exit 2
    ;;
esac
if ((SLOTS < 1 || SLOTS > 8)); then
  echo "cargo-pool: SCROZZ_CARGO_POOL_SLOTS must be between 1 and 8" >&2
  exit 2
fi

case "$WAIT_SECONDS" in
  "" | *[!0-9]*)
    echo "cargo-pool: SCROZZ_CARGO_POOL_WAIT_SECONDS must be an integer" >&2
    exit 2
    ;;
esac

mkdir -p "$ROOT"
ROOT="$(cd "$ROOT" && pwd -P)"

if [[ "${1:-}" == "--status" ]]; then
  echo "Cargo pool: $ROOT"
  for ((index = 1; index <= SLOTS; index++)); do
    slot="$ROOT/slot-$index"
    lease="$slot/.lease"
    if [[ -d "$lease" ]]; then
      echo
      echo "slot-$index: leased"
      if [[ -f "$lease/owner" ]]; then
        sed 's/^/  /' "$lease/owner"
      else
        echo "  owner metadata missing; manual inspection required"
      fi
    else
      echo "slot-$index: available"
    fi
  done
  if [[ -d "$ROOT/workspaces" ]]; then
    for workspace_lock in "$ROOT"/workspaces/*.lease; do
      [[ -d "$workspace_lock" ]] || continue
      echo
      echo "workspace lease: $(basename "$workspace_lock")"
      if [[ -f "$workspace_lock/owner" ]]; then
        sed 's/^/  /' "$workspace_lock/owner"
      else
        echo "  owner metadata missing; manual inspection required"
      fi
    done
  fi
  exit 0
fi

workspace="$PWD"
if git_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
  workspace="$(cd "$git_root" && pwd -P)"
else
  echo "cargo-pool: run from inside the Git worktree being built" >&2
  exit 2
fi

lease=""
slot=""
workspace_lease=""
start="$(date +%s)"
announced_wait=0
owns_lease=0
owns_workspace_lease=0
preserve_lease=0
child_pid=""
child_started=0
acquiring_lease=0
pending_signal=""
pending_signal_status=0

# Invoked by the EXIT trap below.
# shellcheck disable=SC2329
release_lease() {
  if [[ "$preserve_lease" == "1" || "$child_started" == "1" ]]; then
    echo "cargo-pool: interrupted; retaining leases for manual inspection" >&2
    return
  fi
  if [[ "$owns_lease" == "1" && -n "$lease" && -d "$lease" ]]; then
    rm -f "$lease/owner"
    rmdir "$lease" 2>/dev/null || true
    owns_lease=0
  fi
  if [[ "$owns_workspace_lease" == "1" &&
        -n "$workspace_lease" &&
        -d "$workspace_lease" ]]; then
    rm -f "$workspace_lease/owner"
    rmdir "$workspace_lease" 2>/dev/null || true
    owns_workspace_lease=0
  fi
}

# A signal must never remove the lease while a child or orphaned compiler may
# still be writing. Forward it, wait for the direct child, and intentionally
# leave a stale lease for a human to inspect.
# shellcheck disable=SC2329
handle_signal() {
  local signal="$1"
  local status="$2"
  if [[ "$acquiring_lease" == "1" ]]; then
    pending_signal="$signal"
    pending_signal_status="$status"
    return
  fi
  trap - HUP INT TERM
  if [[ "$child_started" == "1" ]]; then
    preserve_lease=1
  fi
  if [[ "$child_started" == "1" &&
        -n "$child_pid" ]] &&
    kill -0 "$child_pid" 2>/dev/null; then
    # Monitor mode gives the child its own process group. Signal the whole
    # group so rustc/build-script descendants cannot outlive Cargo.
    kill -s "$signal" -- "-$child_pid" 2>/dev/null ||
      kill -s "$signal" "$child_pid" 2>/dev/null ||
      true
    wait "$child_pid" 2>/dev/null || true
  fi
  exit "$status"
}

# shellcheck disable=SC2329
flush_pending_signal() {
  if [[ -z "$pending_signal" ]]; then
    return
  fi
  local signal="$pending_signal"
  local status="$pending_signal_status"
  pending_signal=""
  pending_signal_status=0
  handle_signal "$signal" "$status"
}

# Run every target-writing process in its own process group. The surrounding
# calls always inspect the status, so errexit cannot skip lease cleanup.
# shellcheck disable=SC2329
run_managed() {
  local status
  set -m
  child_started=1
  "$@" &
  child_pid=$!
  set +m
  wait "$child_pid"
  status=$?
  child_pid=""
  if ((status > 128)); then
    preserve_lease=1
    echo "cargo-pool: child exited from a signal; retaining $lease" >&2
  else
    child_started=0
  fi
  return "$status"
}

trap release_lease EXIT
trap 'handle_signal HUP 129' HUP
trap 'handle_signal INT 130' INT
trap 'handle_signal TERM 143' TERM

workspace_name="$(printf '%s' "$(basename "$workspace")" | tr -c 'A-Za-z0-9._-' '_')"
workspace_checksum="$(printf '%s' "$workspace" | cksum | awk '{print $1}')"
mkdir -p "$ROOT/workspaces"
workspace_lease="$ROOT/workspaces/$workspace_name-$workspace_checksum.lease"
workspace_wait_start="$(date +%s)"
workspace_announced_wait=0
while true; do
  workspace_acquired=0
  acquiring_lease=1
  if mkdir "$workspace_lease" 2>/dev/null; then
    owns_workspace_lease=1
    workspace_acquired=1
  fi
  acquiring_lease=0
  flush_pending_signal
  if [[ "$workspace_acquired" == "1" ]]; then
    break
  fi

  now="$(date +%s)"
  elapsed=$((now - workspace_wait_start))
  if ((elapsed >= WAIT_SECONDS)); then
    echo "cargo-pool: this worktree stayed leased for ${WAIT_SECONDS}s" >&2
    echo "cargo-pool: inspect owners with: $0 --status" >&2
    exit 75
  fi
  if [[ "$workspace_announced_wait" == "0" ]]; then
    echo "cargo-pool: this worktree is already building; waiting up to ${WAIT_SECONDS}s" >&2
    workspace_announced_wait=1
  fi
  sleep 2
done
{
  printf 'pid=%s\n' "$$"
  printf 'workspace=%s\n' "$workspace"
  printf 'started_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
} >"$workspace_lease/owner"

while [[ -z "$lease" ]]; do
  for ((index = 1; index <= SLOTS; index++)); do
    candidate="$ROOT/slot-$index"
    mkdir -p "$candidate"
    slot_acquired=0
    acquiring_lease=1
    if mkdir "$candidate/.lease" 2>/dev/null; then
      slot="$candidate"
      lease="$candidate/.lease"
      owns_lease=1
      slot_acquired=1
    fi
    acquiring_lease=0
    flush_pending_signal
    if [[ "$slot_acquired" == "1" ]]; then
      {
        printf 'pid=%s\n' "$$"
        printf 'workspace=%s\n' "$workspace"
        printf 'started_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'command='
        printf '%q ' "$@"
        printf '\n'
      } >"$lease/owner"
      break
    fi
  done

  if [[ -n "$lease" ]]; then
    break
  fi

  now="$(date +%s)"
  elapsed=$((now - start))
  if ((elapsed >= WAIT_SECONDS)); then
    echo "cargo-pool: no lease became available within ${WAIT_SECONDS}s" >&2
    echo "cargo-pool: inspect owners with: $0 --status" >&2
    exit 75
  fi
  if [[ "$announced_wait" == "0" ]]; then
    echo "cargo-pool: all $SLOTS slots are leased; waiting up to ${WAIT_SECONDS}s" >&2
    announced_wait=1
  fi
  sleep 2
done

workspace_marker="$slot/workspace"
previous_workspace="$(cat "$workspace_marker" 2>/dev/null || true)"
if [[ "$previous_workspace" != "$workspace" ]]; then
  echo "cargo-pool: switching slot to $workspace" >&2
  echo "cargo-pool: invalidating workspace-local fingerprints" >&2

  # Stable Cargo can give same-name path packages in separate worktrees the
  # same artifact hash. Their dep-info paths are relative, so an older checkout
  # can otherwise accept another branch's newer local artifact as fresh. Purge
  # workspace-package artifacts in every profile/target subtree while the slot
  # is exclusively leased. Registry/git dependencies remain warm. --locked
  # makes invalidation incapable of changing the incoming worktree's lockfile.
  export CARGO_TARGET_DIR="$slot/target"
  if ! run_managed cargo clean --locked --workspace \
    --manifest-path "$workspace/Cargo.toml" >&2; then
    echo "cargo-pool: failed to invalidate workspace package artifacts" >&2
    exit 1
  fi
  if ! run_managed cargo clean --locked --workspace --release \
    --manifest-path "$workspace/Cargo.toml" >&2; then
    echo "cargo-pool: failed to invalidate release package artifacts" >&2
    exit 1
  fi

  target_list="$slot/rust-target-list.$$"
  if ! rustc --print target-list >"$target_list"; then
    rm -f "$target_list"
    echo "cargo-pool: could not enumerate Rust target triples" >&2
    exit 1
  fi
  for target_dir in "$slot"/target/*; do
    [[ -d "$target_dir" && ! -L "$target_dir" ]] || continue
    target="$(basename "$target_dir")"
    grep -Fxq "$target" "$target_list" || continue
    if ! run_managed cargo clean --locked --workspace --target "$target" \
      --manifest-path "$workspace/Cargo.toml" >&2; then
      echo "cargo-pool: failed to invalidate $target package artifacts" >&2
      exit 1
    fi
    if ! run_managed cargo clean --locked --workspace --target "$target" --release \
      --manifest-path "$workspace/Cargo.toml" >&2; then
      echo "cargo-pool: failed to invalidate $target release artifacts" >&2
      exit 1
    fi
  done
  rm -f "$target_list"

  marker_tmp="$slot/workspace.$$"
  printf '%s\n' "$workspace" >"$marker_tmp"
  mv "$marker_tmp" "$workspace_marker"
fi

export CARGO_TARGET_DIR="$slot/target"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-line-tables-only}"
export CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-line-tables-only}"
export SCROZZ_CARGO_LEASE_HELD=1
export SCROZZ_CARGO_POOL_SLOT="$slot"

echo "cargo-pool: leased $(basename "$slot")" >&2
echo "cargo-pool: target $CARGO_TARGET_DIR" >&2

if run_managed "$@"; then
  exit 0
else
  status=$?
  exit "$status"
fi
