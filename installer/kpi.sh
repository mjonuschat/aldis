#!/bin/bash
# --- kpi.sh v0.1.0 ---
# Klipper Plugin Installer library
# https://github.com/mjonuschat/kpi-sh

# --- header.sh ---
# header.sh — Library Init. See docs/superpowers/specs/2026-06-27-kpi-sh-design.md
# "header.sh — Library Init" for the full rationale behind every check below.

if declare -F _kpi_init_done >/dev/null 2>&1; then
    # shellcheck disable=SC2317  # reached only when sourced (return fails
    # and falls through to exit) when this file is executed directly
    return 0 2>/dev/null || exit 0
fi

if ((BASH_VERSINFO[0] < 4)); then
    echo "error: kpi-sh requires Bash 4.0 or newer (found ${BASH_VERSION})" >&2
    exit 1
fi

: "${_KPI_INPUT_FD:=}"
# _KPI_PYTHON's DETECTION stays lazy (first json_get call, see json.sh) —
# but the variable itself must exist under a consumer's set -u before
# that first call ever happens, same reasoning as _KPI_INPUT_FD above.
: "${_KPI_PYTHON:=}"
if [ -z "${_KPI_INPUT_FD}" ]; then
    if [ -e /proc/self/fd/9 ]; then
        # fd 9 already belongs to the consumer (any mode) — don't touch it.
        _KPI_INPUT_FD=""
    elif { exec 9<>/dev/tty; } 2>/dev/null; then
        _KPI_INPUT_FD=9
    else
        _KPI_INPUT_FD=""
    fi
fi

_kpi_init_done() { :; }

# --- log.sh ---
# log.sh — Logging. stdout is reserved for return data across this entire
# library; log/err/die never write to it.

log() {
    printf '%s\n' "$1" >&2
}

err() {
    printf 'error: %s\n' "$1" >&2
}

die() {
    err "$1"
    exit 1
}

# --- path.sh ---
# path.sh — Path Utilities. abspath is purely lexical (never follows
# symlinks); functions elsewhere that need symlink-aware resolution use
# realpath/realpath -m directly instead. See spec "path.sh" section.

abspath() {
    local path="$1"
    if [ -z "$path" ]; then
        die "abspath: empty path"
    fi

    if [[ "$path" != /* ]]; then
        path="$PWD/$path"
    fi

    local -a parts=()
    IFS='/' read -ra parts <<< "$path"
    # `read -ra` splits on IFS without performing pathname expansion, unlike
    # an unquoted `parts=($path)` — that would glob-expand a literal `*` in
    # the path and word-split on embedded spaces. This does neither.

    local -a stack=()
    local part
    for part in "${parts[@]}"; do
        case "$part" in
            "" | ".") continue ;;
            "..")
                if ((${#stack[@]} > 0)); then
                    # Bash 4.0 compat: negative array indices (stack[-1])
                    # require Bash 4.2+. Compute the last index arithmetically
                    # instead — this is the same class of version-floor bug
                    # the spec review already caught once (dynamic fd alloc).
                    unset "stack[$((${#stack[@]} - 1))]"
                fi
                ;;
            *) stack+=("$part") ;;
        esac
    done

    if ((${#stack[@]} == 0)); then
        echo "/"
    else
        local result="" s
        for s in "${stack[@]}"; do
            result="$result/$s"
        done
        echo "$result"
    fi
}

script_dir() {
    # BASH_SOURCE[-1] (negative array indexing) requires Bash 4.2+; this
    # spec's floor is 4.0, so the last frame is computed arithmetically —
    # ${#BASH_SOURCE[@]}-1 — instead of using the shorter negative-index
    # syntax the design doc's prose mentions.
    local n="${#BASH_SOURCE[@]}"
    if [ "$n" -eq 0 ]; then
        die "script_dir: no BASH_SOURCE available (not running from a file)"
    fi
    # ${#BASH_SOURCE[@]} is never actually 0 in practice once any function
    # is executing — bash always populates at least one frame, using the
    # literal placeholder "bash" for code with no real file backing (e.g.
    # a function defined via `eval` rather than `source`). The check above
    # is defensive; this is the check that actually fires for "no
    # meaningful script directory."
    local top="${BASH_SOURCE[$((n - 1))]}"
    if [ ! -f "$top" ]; then
        die "script_dir: no real entrypoint script file backs this call: $top"
    fi
    abspath "$(dirname "$top")"
}

# --- json.sh ---
# json.sh — JSON Parsing. Python 3 is autodetected lazily here (first
# call), not at header.sh init — see spec "Python 3 detection is lazy."

_kpi_ensure_python() {
    if [ -n "${_KPI_PYTHON:-}" ]; then
        return 0
    fi
    local candidate
    for candidate in python3 python; do
        if command -v "$candidate" >/dev/null 2>&1; then
            if "$candidate" -c 'import sys; sys.exit(0 if sys.version_info[0] >= 3 else 1)' 2>/dev/null; then
                _KPI_PYTHON="$(command -v "$candidate")"
                return 0
            fi
        fi
    done
    die "json_get: no Python 3 interpreter found (tried python3, python)"
}

json_get() {
    _kpi_ensure_python

    local rc=0
    "$_KPI_PYTHON" -c '
import sys, json

keys = sys.argv[1:]
try:
    data = json.load(sys.stdin)
except (json.JSONDecodeError, ValueError) as exc:
    print(f"json_get: malformed JSON: {exc}", file=sys.stderr)
    sys.exit(2)

cur = data
for key in keys:
    if not isinstance(cur, dict) or key not in cur:
        sys.exit(1)
    cur = cur[key]

if isinstance(cur, (dict, list)):
    sys.exit(1)
if cur is None:
    print("")
elif isinstance(cur, bool):
    print("true" if cur else "false")
else:
    print(cur)
sys.exit(0)
' "$@" || rc=$?
    if [ "$rc" -eq 2 ]; then
        die "json_get: malformed input JSON"
    fi
    return "$rc"
}

# --- symlink.sh ---
# symlink.sh — Safe Symlink Management. Concurrency is explicitly out of
# scope (single-shot, single-process installer runs) — see spec.

classify_path() {
    local path="$1"
    local parent
    parent="$(dirname "$path")"

    if [ -d "$parent" ] && [ ! -x "$parent" ]; then
        die "cannot determine status of $path: permission denied on $parent"
    fi

    if [ -L "$path" ]; then
        echo "symlink"
    elif [ -e "$path" ]; then
        echo "real"
    else
        echo "absent"
    fi
}

remove_safe_link() {
    local path="$1"
    local state
    state="$(classify_path "$path")" || return 2

    case "$state" in
        absent) return 0 ;;
        symlink)
            rm -f "$path" || return 2
            return 0
            ;;
        real)
            err "refusing to remove non-symlink: $path"
            return 1
            ;;
    esac
}

link_safe() {
    local allow_dangling=0
    if [ "$1" = "--allow-dangling" ]; then
        allow_dangling=1
        shift
    fi
    local target link_name="$2"
    target="$(abspath "$1")" || die "link_safe: cannot resolve target: $1"

    if [ "$allow_dangling" -eq 0 ] && [ ! -e "$target" ]; then
        die "link_safe: target does not exist: $target"
    fi

    local state
    # classify_path can itself die (permission error on the parent) — that
    # die() only terminates the command-substitution subshell, so it must
    # be checked explicitly here rather than trusting $state to be sane.
    state="$(classify_path "$link_name")" || die "link_safe: cannot determine status of $link_name"
    case "$state" in
        real)
            die "link_safe: refusing to overwrite non-symlink: $link_name"
            ;;
        symlink)
            rm -f "$link_name" || die "link_safe: cannot remove existing symlink: $link_name"
            ;;
    esac

    local parent
    parent="$(dirname "$link_name")"
    mkdir -p "$parent" || die "link_safe: cannot create directory: $parent"
    ln -s "$target" "$link_name" || die "link_safe: cannot create symlink: $link_name -> $target"
}

# --- preflight.sh ---
# preflight.sh — Preflight Checks. require_not_root has NO override of any
# kind — see spec "preflight.sh" for why two prior env-var-gated drafts
# were both rejected as structurally bypassable.

require_not_root() {
    if [ "$EUID" -eq 0 ]; then
        die "this script must not be run as root"
    fi
}

require_systemd_service() {
    local name="$1"
    if ! systemctl cat "$name" >/dev/null 2>&1; then
        die "systemd service not installed: $name"
    fi
}

require_python_min() {
    local -x LC_ALL=C
    local interpreter="$1" major="$2" minor="$3"
    local version
    version="$("$interpreter" -c 'import sys; print("%d.%d" % sys.version_info[:2])' 2>/dev/null)" \
        || die "require_python_min: could not query version from $interpreter"

    local found_major="${version%%.*}"
    local found_minor="${version##*.}"
    if ((found_major < major)) || ((found_major == major && found_minor < minor)); then
        die "require_python_min: $interpreter is $version, need >= $major.$minor"
    fi
}

# --- git.sh ---
# git.sh — Git Repository Management. Never fetches/pulls an
# already-valid existing checkout — see spec "How updates actually
# happen" for why (Moonraker's update_manager owns that, not kpi-sh).

# BSD/macOS realpath has no -m (GNU-only: resolve a path that may not
# exist yet). Walk up to the nearest existing ancestor, realpath that
# (so symlinked ancestors still resolve), then append the missing tail
# lexically.
_kpi_git_realpath_m() {
    local path="$1"
    if [ -e "$path" ]; then
        realpath "$path"
        return
    fi
    local dir base resolved_dir
    dir="$(dirname "$path")"
    base="$(basename "$path")"
    resolved_dir="$(_kpi_git_realpath_m "$dir")"
    printf '%s/%s\n' "$resolved_dir" "$base"
}

git_ensure_clone() {
    local repo_url="$1" dest="$2" validator="${3:-}" ref="${4:-}"
    local resolved
    resolved="$(_kpi_git_realpath_m "$dest")" || die "git_ensure_clone: cannot resolve $dest"

    if [ -e "$resolved" ] && [ "$(ls -A "$resolved" 2>/dev/null)" ]; then
        if [ -d "$resolved/.git" ]; then
            local origin
            origin="$(git -C "$resolved" remote get-url origin 2>/dev/null)"
            if [ "$origin" != "$repo_url" ]; then
                die "git_ensure_clone: $resolved origin ($origin) does not match $repo_url"
            fi
        else
            die "git_ensure_clone: $resolved exists, is non-empty, and is not a git repo"
        fi
    else
        git clone -q "$repo_url" "$resolved" || die "git_ensure_clone: clone failed for $repo_url"
    fi

    if [ -n "$ref" ]; then
        if [ -n "$(git -C "$resolved" status --porcelain 2>/dev/null)" ]; then
            die "git_ensure_clone: $resolved has uncommitted changes, refusing to checkout $ref"
        fi
        # `git fetch origin <tag-or-branch-name>` does not create a local
        # ref by that name (only a raw SHA that's already a local ref
        # would resolve directly) — it only updates FETCH_HEAD. Checking
        # out FETCH_HEAD instead of $ref works uniformly for a branch
        # tip, a tag, or a raw commit SHA. Fetch's exit status must be
        # checked explicitly: on failure, FETCH_HEAD is left unchanged
        # from a prior fetch/clone, so an unchecked fetch would silently
        # check out stale state instead of failing.
        git -C "$resolved" fetch origin "$ref" >/dev/null 2>&1 \
            || die "git_ensure_clone: could not resolve ref $ref in $resolved"
        git -C "$resolved" checkout --detach -q FETCH_HEAD 2>/dev/null \
            || die "git_ensure_clone: could not checkout ref $ref in $resolved"
    fi

    if [ -n "$validator" ] && [ ! -e "$resolved/$validator" ]; then
        die "git_ensure_clone: validator missing after checkout: $resolved/$validator"
    fi
}

# --- config_block.sh ---
# config_block.sh — Sentinel-Delimited Config Blocks. Writes are atomic
# via temp-file-then-rename (visibility-only, not fsync durability — see
# spec). Matching is fixed-string, never regex.

# BSD/macOS realpath has no -m (GNU-only: resolve a path that may not
# exist yet). Walk up to the nearest existing ancestor, realpath that
# (so symlinked ancestors still resolve), then append the missing tail
# lexically. Same fix pattern as git.sh's _kpi_git_realpath_m.
_kpi_config_block_realpath_m() {
    local path="$1"
    if [ -e "$path" ]; then
        realpath "$path"
        return
    fi
    local dir base resolved_dir
    dir="$(dirname "$path")"
    base="$(basename "$path")"
    resolved_dir="$(_kpi_config_block_realpath_m "$dir")"
    printf '%s/%s\n' "$resolved_dir" "$base"
}

# BSD/macOS chmod has no --reference (GNU-only). Read the source file's
# mode with stat (whose format flag also differs: GNU `-c %a`, BSD
# `-f %Lp`) and apply it numerically instead.
_kpi_config_block_chmod_reference() {
    local ref="$1" target="$2"
    local mode
    mode="$(stat -c %a "$ref" 2>/dev/null || stat -f %Lp "$ref" 2>/dev/null)"
    [ -n "$mode" ] || return 1
    chmod "$mode" "$target"
}

_kpi_config_block_check_name() {
    local name="$1" caller="${2:-config_block}"
    [[ "$name" =~ ^[A-Za-z0-9_.-]+$ ]] || die "$caller: invalid name: $name"
}

_kpi_config_block_write() {
    # Args: resolved_file, new_content (already has trailing newline)
    local resolved="$1" new_content="$2"
    local tmp=""
    local _kpi_prev_trap _kpi_prev_trap_cmd=""
    _kpi_prev_trap="$(trap -p EXIT)"
    # `eval "$_kpi_prev_trap"` (the full `trap -- '...' EXIT` string) only
    # re-registers the trap — a no-op if it fires from inside our own EXIT
    # trap, since the shell is already exiting and an EXIT trap fires once.
    # Shadow `trap` as a function so evaluating that string hands us its
    # bare command text ($2) instead, which we can actually execute on the
    # abnormal-exit path below.
    if [ -n "$_kpi_prev_trap" ]; then
        # shellcheck disable=SC2317,SC2329  # invoked indirectly: eval-ing
        # "$_kpi_prev_trap" (a `trap -- '...' EXIT` string) calls this
        # shadowed `trap` with the captured command as $2.
        _kpi_prev_trap_cmd="$(trap() { printf '%s' "$2"; }; eval "$_kpi_prev_trap")"
    fi
    trap 'rm -f "$tmp"; if [ -n "$_kpi_prev_trap_cmd" ]; then eval "$_kpi_prev_trap_cmd"; fi' EXIT

    tmp="$(mktemp "$(dirname "$resolved")/.kpi.XXXXXX")" || die "config_block: mktemp failed for $resolved"
    printf '%s' "$new_content" > "$tmp" || die "config_block: write to temp file failed: $tmp"
    if [ -f "$resolved" ]; then
        _kpi_config_block_chmod_reference "$resolved" "$tmp" \
            || die "config_block: failed to preserve permissions from $resolved onto temp file"
    fi
    mv "$tmp" "$resolved" || die "config_block: rename failed: $tmp -> $resolved"

    if [ -n "$_kpi_prev_trap" ]; then
        eval "$_kpi_prev_trap"
    else
        trap - EXIT
    fi
}

_kpi_config_block_resolve_existing() {
    # Prints the realpath of $1 if it resolves to a real (non-dangling)
    # file, empty string if absent/dangling. Never dies here — callers
    # decide what absence means for them.
    local file="$1"
    if [ -L "$file" ] && [ ! -e "$file" ]; then
        echo ""
        return 0
    fi
    if [ -e "$file" ]; then
        realpath "$file"
    else
        echo ""
    fi
}

# Prints $resolved with every block named $name removed or, when a
# replacement is given, the first one's body swapped for it and any later
# duplicates removed. Dies on a malformed block.
_kpi_config_block_rewrite() {
    local caller="$1" resolved="$2" name="$3"
    local start="# --- $name ---" end="# --- /$name ---"
    local replace=0 replacement=""
    if [ "$#" -ge 4 ]; then
        replace=1
        replacement="$4"
    fi

    local out rc=0
    # The replacement goes through ENVIRON: awk -v would interpret backslash
    # escapes in it.
    out="$(KPI_REPLACEMENT="$replacement" awk -v start="$start" -v end="$end" -v replace="$replace" '
        BEGIN { in_block = 0; done = 0 }
        {
            if (!in_block && $0 == start) {
                in_block = 1
                if (replace && !done) { print start; print ENVIRON["KPI_REPLACEMENT"]; print end }
                done = 1
                next
            }
            if (in_block) {
                if ($0 == end) { in_block = 0; next }
                if ($0 ~ /^# --- .+ ---$/) {
                    print "config_block: foreign sentinel inside block: " $0 > "/dev/stderr"
                    exit 3
                }
                next
            }
            print
        }
        END { if (in_block) exit 4 }
    ' "$resolved")" || rc=$?
    if [ "$rc" -eq 3 ] || [ "$rc" -eq 4 ]; then
        die "$caller: malformed block for $name in $resolved"
    elif [ "$rc" -ne 0 ]; then
        # Any other nonzero awk exit (e.g. a read failure) must stop here
        # rather than feed partial output into the atomic write.
        die "$caller: failed to process $resolved (awk exit $rc)"
    fi
    printf '%s' "$out"
}

# Replaces the body of an existing block in place, or no-ops when it
# already matches. Returns 1 if the block is absent.
_kpi_config_block_update() {
    local caller="$1" resolved="$2" name="$3" content="$4"
    local grep_rc=0
    grep -qxF "# --- $name ---" "$resolved" || grep_rc=$?
    if [ "$grep_rc" -eq 1 ]; then
        return 1
    elif [ "$grep_rc" -ne 0 ]; then
        die "$caller: cannot read $resolved (grep exit $grep_rc)"
    fi

    local existing_content new_content
    existing_content="$(cat "$resolved")" || die "$caller: cannot read $resolved"
    new_content="$(_kpi_config_block_rewrite "$caller" "$resolved" "$name" "$content")" \
        || die "$caller: cannot update block $name in $resolved"
    if [ "$new_content" != "$existing_content" ]; then
        _kpi_config_block_write "$resolved" "$new_content"$'\n'
    fi
}

config_block_add() {
    local -x LC_ALL=C
    local file="$1" name="$2" content="$3"
    _kpi_config_block_check_name "$name" config_block_add
    if printf '%s\n' "$content" | grep -qE '^# --- .+ ---$'; then
        die "config_block_add: content contains a sentinel-shaped line"
    fi

    local resolved
    resolved="$(_kpi_config_block_resolve_existing "$file")"
    if [ -z "$resolved" ]; then
        die "config_block_add: file does not exist: $file"
    fi

    if _kpi_config_block_update config_block_add "$resolved" "$name" "$content"; then
        return 0
    fi
    local start="# --- $name ---"

    # Checked explicitly: an unreadable existing file (e.g. permission
    # revoked after resolve) would otherwise make this `cat` fail
    # silently, and the atomic write below would then replace the whole
    # file with just the new block — a real data-loss path, not just a
    # missed error message.
    local existing_content
    existing_content="$(cat "$resolved")" || die "config_block_add: cannot read $resolved"
    local new_content
    new_content="$existing_content"$'\n'"$start"$'\n'"$content"$'\n'"# --- /$name ---"$'\n'
    _kpi_config_block_write "$resolved" "$new_content"
}

config_block_ensure() {
    local -x LC_ALL=C
    local file="$1" name="$2" content="$3"
    _kpi_config_block_check_name "$name" config_block_ensure
    if printf '%s\n' "$content" | grep -qE '^# --- .+ ---$'; then
        die "config_block_ensure: content contains a sentinel-shaped line"
    fi

    # A dangling symlink at $file is removed first so we don't try to
    # write through a broken link.
    if [ -L "$file" ] && [ ! -e "$file" ]; then
        rm -f "$file" || die "config_block_ensure: cannot remove dangling symlink: $file"
    fi

    local resolved
    resolved="$(_kpi_config_block_resolve_existing "$file")"
    local existing=""
    if [ -n "$resolved" ]; then
        if _kpi_config_block_update config_block_ensure "$resolved" "$name" "$content"; then
            return 0
        fi
        local existing_content
        existing_content="$(cat "$resolved")" || die "config_block_ensure: cannot read $resolved"
        existing="$existing_content"$'\n'
    else
        resolved="$(_kpi_config_block_realpath_m "$file")" \
            || die "config_block_ensure: cannot resolve $file"
    fi

    local new_content
    new_content="${existing}# --- $name ---"$'\n'"$content"$'\n'"# --- /$name ---"$'\n'
    _kpi_config_block_write "$resolved" "$new_content"
}

config_block_remove() {
    local -x LC_ALL=C
    local file="$1" name="$2"
    _kpi_config_block_check_name "$name" config_block_remove
    local resolved
    resolved="$(_kpi_config_block_resolve_existing "$file")"
    if [ -z "$resolved" ]; then
        return 0
    fi

    # Distinguish "genuinely not found" (grep exit 1) from "couldn't even
    # read the file" (any other nonzero) — `if ! grep ...` alone treats a
    # permission error identically to "not found" and would silently
    # no-op instead of ever reaching the failure checks below.
    local grep_rc=0
    grep -qxF "# --- $name ---" "$resolved" || grep_rc=$?
    if [ "$grep_rc" -eq 1 ]; then
        return 0
    elif [ "$grep_rc" -ne 0 ]; then
        die "config_block_remove: cannot read $resolved (grep exit $grep_rc)"
    fi

    local new_content
    new_content="$(_kpi_config_block_rewrite config_block_remove "$resolved" "$name")" \
        || die "config_block_remove: cannot remove block $name from $resolved"
    _kpi_config_block_write "$resolved" "$new_content"$'\n'
}

# --- service.sh ---
# service.sh — Systemd Service Management. sudo is always used; 30s
# timeout is a fixed design constant. Exit 124 is treated as "timed
# out" — a scoped assumption, see spec.

service_restart_if() {
    local service="$1" predicate="${2:-}"

    if [ -n "$predicate" ]; then
        if ! "$predicate"; then
            log "skipping restart of $service: predicate declined or failed"
            return 0
        fi
    fi

    local rc=0
    timeout 30 sudo systemctl restart "$service" || rc=$?
    if [ "$rc" -eq 124 ]; then
        die "service_restart_if: restart timed out after 30s: $service"
    elif [ "$rc" -ne 0 ]; then
        die "service_restart_if: restart failed for $service (exit $rc)"
    fi
}

# --- backup.sh ---
# backup.sh — Config Backup. Stages off to the side, one atomic rename to
# the final name. INT/TERM re-raise to $BASHPID (not $$) so command
# substitution invocation terminates correctly — see spec.

# BSD/macOS realpath has no -m (GNU-only: resolve a path that may not
# exist yet). Same portable ancestor-walking replacement as
# config_block.sh's _kpi_config_block_realpath_m.
_kpi_backup_realpath_m() {
    local path="$1"
    if [ -e "$path" ]; then
        realpath "$path"
        return
    fi
    local dir base resolved_dir
    dir="$(dirname "$path")"
    base="$(basename "$path")"
    resolved_dir="$(_kpi_backup_realpath_m "$dir")"
    printf '%s/%s\n' "$resolved_dir" "$base"
}

backup_dir_timestamped() {
    local source="$1" backup_root="$2"

    if [ -n "$source" ] && [ -e "$source" ] && [ ! -d "$source" ]; then
        die "backup_dir_timestamped: source must be a directory or absent: $source"
    fi

    local have_source=0
    if [ -d "$source" ]; then
        have_source=1
        local rsource rroot
        rsource="$(realpath "$source")" || die "backup_dir_timestamped: cannot resolve $source"
        rroot="$(_kpi_backup_realpath_m "$backup_root")" \
            || die "backup_dir_timestamped: cannot resolve $backup_root"
        case "$rroot/" in
            "$rsource/"*) die "backup_dir_timestamped: backup_root is inside source" ;;
        esac
        case "$rsource/" in
            "$rroot/"*) die "backup_dir_timestamped: source is inside backup_root" ;;
        esac
    fi

    mkdir -p "$backup_root" || die "backup_dir_timestamped: cannot create backup_root: $backup_root"

    local staging=""
    local _kpi_prev_exit _kpi_prev_exit_cmd="" _kpi_prev_int _kpi_prev_term
    _kpi_prev_exit="$(trap -p EXIT)"
    _kpi_prev_int="$(trap -p INT)"
    _kpi_prev_term="$(trap -p TERM)"
    # See config_block.sh's _kpi_config_block_write: eval-ing the full
    # `trap -p` string only re-registers the prior trap, a no-op once it
    # fires from inside our own EXIT trap (the shell is already exiting).
    # Shadowing `trap` as a function while evaluating that string hands us
    # its bare command text instead, which we can actually execute here.
    if [ -n "$_kpi_prev_exit" ]; then
        # shellcheck disable=SC2317,SC2329  # invoked indirectly: eval-ing
        # "$_kpi_prev_exit" (a `trap -- '...' EXIT` string) calls this
        # shadowed `trap` with the captured command as $2.
        _kpi_prev_exit_cmd="$(trap() { printf '%s' "$2"; }; eval "$_kpi_prev_exit")"
    fi

    trap 'rm -rf "$staging"; if [ "$BASHPID" = "$$" ] && [ -n "$_kpi_prev_exit_cmd" ]; then eval "$_kpi_prev_exit_cmd"; fi' EXIT
    trap 'rm -rf "$staging"; trap - INT; kill -INT "$BASHPID"' INT
    trap 'rm -rf "$staging"; trap - TERM; kill -TERM "$BASHPID"' TERM

    staging="$(mktemp -d "$backup_root/.kpi-backup.XXXXXX")" \
        || die "backup_dir_timestamped: mktemp -d failed in $backup_root"
    # Checked explicitly: an unchecked failure here would leave $staging
    # empty, and the cp/mv calls below would then operate on "" (which
    # resolves to the filesystem root in several of the path expressions
    # used elsewhere) instead of a real staging directory.

    if [ "$have_source" -eq 1 ]; then
        cp -fa "$source/." "$staging/" || die "backup_dir_timestamped: copy failed from $source"
    else
        log "backup_dir_timestamped: source does not exist, creating empty backup: $source"
    fi

    local base
    base="$backup_root/$(date +%Y_%m_%d-%H%M%S)"
    local final="$base"
    local n=2
    while [ -e "$final" ]; do
        final="${base}-${n}"
        n=$((n + 1))
    done

    mv "$staging" "$final" || die "backup_dir_timestamped: rename failed: $staging -> $final"
    staging=""

    trap - INT
    trap - TERM
    # Re-arming a caller's trap via the `trap` builtin only matters in the
    # shell that will actually reach its own exit later — this function's
    # documented calling convention is `dir="$(backup_dir_timestamped ...)"`,
    # which runs it inside a throwaway command-substitution subshell. Bash
    # does not auto-fire an inherited-but-never-(re)armed EXIT trap when
    # such a subshell terminates (verified by standalone repro), but it
    # DOES fire one that gets explicitly re-armed here via `trap ... EXIT`
    # — so unconditionally eval-ing the caller's prior trap string would
    # make it fire once here (spuriously, mid-caller-run) and again for
    # real at the caller's actual exit. $BASHPID == $$ only in a shell
    # that was never forked, so restore only there.
    if [ "$BASHPID" = "$$" ]; then
        if [ -n "$_kpi_prev_exit" ]; then
            eval "$_kpi_prev_exit"
        else
            trap - EXIT
        fi
        if [ -n "$_kpi_prev_int" ]; then eval "$_kpi_prev_int"; fi
        if [ -n "$_kpi_prev_term" ]; then eval "$_kpi_prev_term"; fi
    else
        trap - EXIT
    fi

    echo "$final"
}

backup_scrub_symlinks() {
    local -x LC_ALL=C
    local backup_dir="$1" prefix="$2"
    local rprefix
    rprefix="$(realpath "$prefix")" || die "backup_scrub_symlinks: cannot resolve prefix: $prefix"
    # find's exit status is lost inside `< <(...)`, so check up front.
    if [ ! -d "$backup_dir" ] || [ ! -r "$backup_dir" ]; then
        die "backup_scrub_symlinks: cannot read $backup_dir"
    fi

    local link raw rtarget
    while IFS= read -r -d '' link; do
        raw="$(readlink "$link")" || die "backup_scrub_symlinks: cannot read link: $link"
        # A relative link resolves against the backup dir, not where it was
        # copied from, so its real target is unknowable here.
        [[ "$raw" == /* ]] || continue
        # GNU realpath resolves a dangling link whose last component is
        # missing; BSD and busybox fail. Skip dangling links explicitly.
        [ -e "$link" ] || continue
        rtarget="$(realpath "$link")" || die "backup_scrub_symlinks: cannot resolve $link"
        case "$rtarget" in
            "$rprefix" | "$rprefix"/*)
                rm -f "$link" || die "backup_scrub_symlinks: cannot remove $link"
                ;;
        esac
    done < <(find "$backup_dir" -type l -print0)
}

# --- prompt.sh ---
# prompt.sh — Interactive Prompts. Bounded retry (3 attempts), EOF and
# no-tty both fall back to $default (or die if none) — never blocks,
# never loops forever. See spec.

confirm_yn() {
    local prompt="$1" default="${2:-}"

    if [ -n "$default" ] && [[ "$default" != [yYnN] ]]; then
        die "confirm_yn: invalid default: $default"
    fi

    local hint="(y/n)"
    case "$default" in
        [yY]) hint="(Y/n)" ;;
        [nN]) hint="(y/N)" ;;
    esac

    if [ -z "${_KPI_INPUT_FD:-}" ]; then
        if [ -z "$default" ]; then
            die "confirm_yn: no interactive input available and no default given"
        fi
        log "non-interactive environment, using default: $default"
        [[ "$default" =~ [yY] ]] && return 0 || return 1
    fi

    local attempt=1 answer
    while [ "$attempt" -le 3 ]; do
        log "$prompt $hint"
        if ! IFS= read -u "$_KPI_INPUT_FD" -r answer; then
            answer="__EOF__"
        fi

        if [ -z "$answer" ] && [ -n "$default" ]; then
            answer="$default"
        fi

        case "$answer" in
            [yY] | [yY][eE][sS]) return 0 ;;
            [nN] | [nN][oO]) return 1 ;;
            "__EOF__") break ;;
            *)
                log "please answer y or n"
                attempt=$((attempt + 1))
                ;;
        esac
    done

    if [ -n "$default" ]; then
        log "no valid response, using default: $default"
        [[ "$default" =~ [yY] ]] && return 0 || return 1
    fi
    die "confirm_yn: no valid response and no default given"
}

select_from_dir() {
    local -x LC_ALL=C
    local prompt="$1" dir="$2" glob="$3"

    local -a files=()
    while IFS= read -r -d '' f; do
        files+=("$f")
    done < <(find "$dir" -maxdepth 1 -type f -name "$glob" -print0 2>/dev/null | sort -z)

    if [ "${#files[@]}" -eq 0 ]; then
        log "select_from_dir: no files match $glob in $dir"
        return 1
    fi

    if [ -z "${_KPI_INPUT_FD:-}" ]; then
        log "non-interactive environment, skipping selection: $prompt"
        return 1
    fi

    local i display
    log "$prompt"
    log "  0) skip"
    for i in "${!files[@]}"; do
        display="$(basename "${files[$i]}")"
        display="${display%.*}"
        display="${display//_/ }"
        display="${display//-/ }"
        log "  $((i + 1))) $display"
    done

    local attempt=1 choice
    while [ "$attempt" -le 3 ]; do
        if ! IFS= read -u "$_KPI_INPUT_FD" -r choice; then
            log "select_from_dir: no more input, skipping"
            return 1
        fi
        if [[ "$choice" =~ ^[0-9]+$ ]]; then
            if [ "$choice" -eq 0 ]; then
                return 1
            elif [ "$choice" -ge 1 ] && [ "$choice" -le "${#files[@]}" ]; then
                echo "${files[$((choice - 1))]}"
                return 0
            fi
        fi
        log "please enter a number between 0 and ${#files[@]}"
        attempt=$((attempt + 1))
    done

    log "select_from_dir: no valid selection, skipping"
    return 1
}

# --- version.sh ---
# version.sh — Version Tracking. Same realpath-first, temp-file,
# permission-preserving pattern as config_block.sh — never in place.

# BSD/macOS realpath has no -m; BSD/macOS chmod has no --reference. Same
# portable replacements as config_block.sh's
# _kpi_config_block_realpath_m / _kpi_config_block_chmod_reference —
# pre-empted here rather than discovered via a later test failure.
_kpi_version_realpath_m() {
    local path="$1"
    if [ -e "$path" ]; then
        realpath "$path"
        return
    fi
    local dir base resolved_dir
    dir="$(dirname "$path")"
    base="$(basename "$path")"
    resolved_dir="$(_kpi_version_realpath_m "$dir")"
    printf '%s/%s\n' "$resolved_dir" "$base"
}

_kpi_version_chmod_reference() {
    local ref="$1" target="$2"
    local mode
    mode="$(stat -c %a "$ref" 2>/dev/null || stat -f %Lp "$ref" 2>/dev/null)"
    [ -n "$mode" ] || return 1
    chmod "$mode" "$target"
}

version_stamp() {
    local repo_dir="$1" dest_file="$2"
    local sha
    sha="$(git -C "$repo_dir" rev-parse HEAD)" || die "version_stamp: cannot resolve HEAD in $repo_dir"

    local resolved
    if [ -e "$dest_file" ]; then
        resolved="$(realpath "$dest_file")" || die "version_stamp: cannot resolve $dest_file"
    else
        resolved="$(_kpi_version_realpath_m "$dest_file")" || die "version_stamp: cannot resolve $dest_file"
    fi

    local tmp=""
    local _kpi_prev_trap _kpi_prev_trap_cmd=""
    _kpi_prev_trap="$(trap -p EXIT)"
    # See config_block.sh's _kpi_config_block_write: eval-ing the full
    # `trap -p` string only re-registers the prior trap, a no-op once it
    # fires from inside our own EXIT trap (the shell is already exiting).
    # Shadowing `trap` as a function while evaluating that string hands us
    # its bare command text instead, which we can actually execute here.
    if [ -n "$_kpi_prev_trap" ]; then
        # shellcheck disable=SC2317,SC2329  # invoked indirectly: eval-ing
        # "$_kpi_prev_trap" (a `trap -- '...' EXIT` string) calls this
        # shadowed `trap` with the captured command as $2.
        _kpi_prev_trap_cmd="$(trap() { printf '%s' "$2"; }; eval "$_kpi_prev_trap")"
    fi
    trap 'rm -f "$tmp"; if [ -n "$_kpi_prev_trap_cmd" ]; then eval "$_kpi_prev_trap_cmd"; fi' EXIT

    tmp="$(mktemp "$(dirname "$resolved")/.kpi.XXXXXX")" || die "version_stamp: mktemp failed"
    echo "$sha" > "$tmp" || die "version_stamp: write failed: $tmp"
    if [ -f "$resolved" ]; then
        _kpi_version_chmod_reference "$resolved" "$tmp" \
            || die "version_stamp: failed to preserve permissions from $resolved onto temp file"
    fi
    mv "$tmp" "$resolved" || die "version_stamp: rename failed: $tmp -> $resolved"

    if [ -n "$_kpi_prev_trap" ]; then
        eval "$_kpi_prev_trap"
    else
        trap - EXIT
    fi
}

is_first_install() {
    local dest_file="$1"
    [ ! -e "$dest_file" ]
}

# --- klipper.sh ---
# klipper.sh — Klipper Ecosystem. Discovered values are validated by
# identity and constrained under $HOME; env overrides bypass validation
# (trusted, same class as $repo_url in git.sh) — see spec.

moonraker_query() {
    local endpoint="$1"
    curl -fsS --max-time 8 --max-filesize 1048576 "${MOONRAKER_HOST:-http://localhost:7125}${endpoint}"
}

_kpi_klipper_home() {
    if [ -z "${HOME:-}" ]; then
        die "discover_klipper_env: \$HOME is unset"
    fi
    # GNU realpath accepts a missing last component, so check existence first.
    if [ ! -d "$HOME" ]; then
        die "discover_klipper_env: \$HOME does not resolve to a real directory: $HOME"
    fi
    realpath "$HOME" 2>/dev/null || die "discover_klipper_env: \$HOME does not resolve to a real directory: $HOME"
}

_kpi_under_home() {
    # $1: candidate path, $2: canonical $HOME
    local candidate="$1" home="$2" resolved
    resolved="$(realpath "$candidate" 2>/dev/null)" || return 1
    case "$resolved/" in
        "$home/"*) return 0 ;;
        *) return 1 ;;
    esac
}

discover_klipper_env() {
    local home
    # _kpi_klipper_home's own die() only exits the command-substitution
    # subshell it runs in — an unguarded assignment here would let
    # discover_klipper_env continue with an empty $home instead of
    # actually stopping, defeating the "invalid $HOME is a hard stop"
    # contract from the spec.
    home="$(_kpi_klipper_home)" || die "discover_klipper_env: cannot determine \$HOME"

    local host_was_set=0
    [ -n "${MOONRAKER_HOST:-}" ] && host_was_set=1
    : "${MOONRAKER_HOST:=http://localhost:7125}"
    if [ "$host_was_set" -eq 1 ] && [ "$MOONRAKER_HOST" != "http://localhost:7125" ]; then
        log "MOONRAKER_HOST overridden to a non-default host — discovery will trust that host's response"
    fi

    local info=""
    if ! info="$(moonraker_query /printer/info 2>/dev/null)"; then
        log "discover_klipper_env: Moonraker unreachable, falling back to hardcoded defaults"
        info=""
    fi

    if [ -z "${KLIPPER_PATH:-}" ]; then
        local candidate
        # `|| true`: this is a plain assignment, not part of an if/&&/||
        # list, so under a consumer's `set -e` a nonzero json_get (missing
        # key — the normal case when Moonraker has no klipper_path) would
        # otherwise abort the whole installer instead of falling back.
        candidate="$(printf '%s' "$info" | json_get result klipper_path 2>/dev/null)" || true
        if [ -n "$candidate" ] && _kpi_under_home "$candidate" "$home" && [ -f "$candidate/klippy/klippy.py" ]; then
            KLIPPER_PATH="$candidate"
        else
            log "discover_klipper_env: no usable klipper_path from Moonraker, falling back to default"
            KLIPPER_PATH="${HOME}/klipper"
        fi
    fi

    if [ -z "${KLIPPY_PYTHON:-}" ]; then
        local candidate
        candidate="$(printf '%s' "$info" | json_get result python_path 2>/dev/null)" || true
        if [ -n "$candidate" ] && _kpi_under_home "$candidate" "$home" \
            && [ -x "$candidate" ] \
            && [ "$("$candidate" -c 'import sys; print(sys.version_info[0])' 2>/dev/null)" = "3" ]; then
            KLIPPY_PYTHON="$candidate"
        else
            log "discover_klipper_env: no usable python_path from Moonraker, falling back to default"
            KLIPPY_PYTHON="${HOME}/klippy-env/bin/python"
        fi
    fi

    if [ -z "${KLIPPER_PLUGINS_PATH:-}" ]; then
        if [ -d "${KLIPPER_PATH}/klippy/plugins" ]; then
            KLIPPER_PLUGINS_PATH="${KLIPPER_PATH}/klippy/plugins"
        else
            KLIPPER_PLUGINS_PATH="${KLIPPER_PATH}/klippy/extras"
        fi
    fi

    if [ -z "${MOONRAKER_CONFIG:-}" ]; then
        if [ -f "${HOME}/printer_data/config/moonraker.conf" ]; then
            MOONRAKER_CONFIG="${HOME}/printer_data/config/moonraker.conf"
        else
            MOONRAKER_CONFIG="${HOME}/klipper_config/moonraker.conf"
        fi
    fi
}

check_no_active_print() {
    local state
    # `|| true`: a plain assignment, not part of an if/&&/|| list — an
    # unreachable Moonraker or a missing field must not abort a
    # consumer's set -e script here (the documented "unreachable -> 1"
    # outcome needs to be reached, not skipped by an early script exit).
    state="$(moonraker_query "/printer/objects/query?print_stats=state" 2>/dev/null | json_get result status print_stats state 2>/dev/null)" || true
    case "$state" in
        standby | complete | cancelled | error) return 0 ;;
        *) return 1 ;;
    esac
}

# --- /kpi.sh ---
