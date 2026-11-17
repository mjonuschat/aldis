#!/usr/bin/env bash

aldis_map_arch() {
  case "$1" in
    aarch64) echo aarch64-linux ;;
    armv7l) echo armv7l-linux ;;
    x86_64) echo x86_64-linux ;;
    *)
      echo "Unsupported architecture '$1': aldis supports aarch64, armv7l and x86_64." >&2
      return 1
      ;;
  esac
}

_aldis_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

aldis_verify_checksum() {
  local sums_file="$1" asset="$2" file="$3"
  local lines expected actual
  lines="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$sums_file")" || return 1
  [ -n "$lines" ] || return 1
  [ "$(printf '%s\n' "$lines" | wc -l | tr -d ' ')" = "1" ] || return 1
  expected="$lines"
  actual="$(_aldis_sha256 "$file")" || return 1
  [ "$expected" = "$actual" ]
}

_aldis_json_escape() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '%s' "$s"
}

aldis_install_files() {
  local dir="$1" asset="$2" downloaded="$3" tag="$4" owner="$5" project="$6"
  mkdir -p "$dir" || return 1
  cp "$downloaded" "$dir/.$asset.new" || return 1
  chmod 0755 "$dir/.$asset.new" || return 1
  mv -f "$dir/.$asset.new" "$dir/$asset" || return 1
  ln -sfn "$asset" "$dir/.aldis.new" || return 1
  mv -fh "$dir/.aldis.new" "$dir/aldis" 2>/dev/null ||
    mv -fT "$dir/.aldis.new" "$dir/aldis" || return 1
  printf '{\n  "project_name": "%s",\n  "project_owner": "%s",\n  "version": "%s",\n  "asset_name": "%s"\n}\n' \
    "$(_aldis_json_escape "$project")" "$(_aldis_json_escape "$owner")" \
    "$(_aldis_json_escape "$tag")" "$(_aldis_json_escape "$asset")" >"$dir/release_info.json.new" || return 1
  mv -f "$dir/release_info.json.new" "$dir/release_info.json"
}

ALDIS_RELEASE_API="${ALDIS_RELEASE_API:-https://api.github.com/repos/mjonuschat/aldis/releases/latest}"
ALDIS_DOWNLOAD_BASE="${ALDIS_DOWNLOAD_BASE:-https://github.com/mjonuschat/aldis/releases/download}"
ALDIS_OWNER="mjonuschat"
ALDIS_PROJECT="aldis"
ALDIS_BLOCK_NAME="aldis"

aldis_usage() {
  cat <<'USAGE'
Usage: aldis-installer.sh [--no-agent] [--help]

Installs the latest aldis release into ~/aldis, runs `sudo aldis setup --agent`,
and registers aldis with Moonraker's update manager. Run as the normal user.

  --no-agent  Do not install the aldis service: run `sudo aldis setup` for the udev
              rules and sudoers policy only. The update manager entry is still
              added, without a managed service.
USAGE
}

_aldis_unmanaged_section() {
  local conf="$1"
  [ -f "$conf" ] || return 1
  awk -v start="# --- $ALDIS_BLOCK_NAME ---" -v end="# --- /$ALDIS_BLOCK_NAME ---" '
    $0 == start { in_block = 1; next }
    $0 == end { in_block = 0; next }
    !in_block && $0 ~ /^\[update_manager[ \t]+aldis\][ \t]*$/ { found = 1 }
    END { exit found ? 0 : 1 }
  ' "$conf"
}

_aldis_require_idle() {
  if ! check_no_active_print; then
    log "A print is active or the printer state could not be determined; re-run when idle." >&2
    return 1
  fi
}

_aldis_download() {
  local tmp="$1" asset="$2" body
  body="$(curl -fsSL "$ALDIS_RELEASE_API")" || { log "Could not query the latest release." >&2; return 1; }
  ALDIS_TAG="$(printf '%s' "$body" | json_get tag_name)" || ALDIS_TAG=""
  if [ -z "$ALDIS_TAG" ]; then
    log "The release API response had no tag_name." >&2
    return 1
  fi
  curl -fsSL -o "$tmp/$asset" "$ALDIS_DOWNLOAD_BASE/$ALDIS_TAG/$asset" ||
    { log "Download failed for $asset ($ALDIS_TAG)." >&2; return 1; }
  curl -fsSL -o "$tmp/SHA256SUMS" "$ALDIS_DOWNLOAD_BASE/$ALDIS_TAG/SHA256SUMS" ||
    { log "Download failed for SHA256SUMS ($ALDIS_TAG)." >&2; return 1; }
  if ! aldis_verify_checksum "$tmp/SHA256SUMS" "$asset" "$tmp/$asset"; then
    log "Checksum verification failed for $asset; nothing was installed." >&2
    return 1
  fi
}

aldis_main() {
  local agent=1
  while [ $# -gt 0 ]; do
    case "$1" in
      --no-agent) agent=0 ;;
      -h | --help) aldis_usage; return 0 ;;
      *) log "Unknown argument: $1" >&2; return 1 ;;
    esac
    shift
  done

  discover_klipper_env
  require_not_root

  if _aldis_unmanaged_section "$MOONRAKER_CONFIG"; then
    log "$MOONRAKER_CONFIG already has an [update_manager aldis] section that this" >&2
    log "installer does not manage. Remove or rename it, then re-run; nothing was changed." >&2
    return 1
  fi

  _aldis_require_idle || return 1

  local arch asset dir="$HOME/aldis" tmp
  arch="$(aldis_map_arch "$(uname -m)")" || return 1
  asset="aldis-$arch"

  tmp="$(mktemp -d)" || return 1
  if ! _aldis_download "$tmp" "$asset"; then
    rm -rf "$tmp"
    return 1
  fi

  if ! aldis_install_files "$dir" "$asset" "$tmp/$asset" "$ALDIS_TAG" "$ALDIS_OWNER" "$ALDIS_PROJECT"; then
    rm -rf "$tmp"
    log "Could not install files into $dir." >&2
    return 1
  fi
  rm -rf "$tmp"

  _aldis_require_idle || return 1

  local setup_args=(setup)
  [ "$agent" -eq 1 ] && setup_args+=(--agent)
  if ! sudo "$dir/aldis" "${setup_args[@]}"; then
    log "aldis setup failed. The files are in place; retry with:" >&2
    log "  sudo $dir/aldis ${setup_args[*]}" >&2
    return 1
  fi

  local block
  block="type: executable
channel: stable
repo: mjonuschat/aldis
path: ~/aldis"
  [ "$agent" -eq 1 ] && block="$block
managed_services: aldis"
  # The subshell keeps kpi's die() from ending the installer; it inherits the caller's EXIT
  # trap, which kpi's atomic file writer would re-arm and fire a second time.
  if ! (
    trap - EXIT
    config_block_ensure "$MOONRAKER_CONFIG" "$ALDIS_BLOCK_NAME" "[update_manager aldis]
$block"
  ); then
    log "Could not write the update manager block. aldis itself is installed." >&2
    log "Add this to $MOONRAKER_CONFIG and restart Moonraker:" >&2
    log "[update_manager aldis]" >&2
    log "$block" >&2
    return 1
  fi

  _aldis_require_idle || {
    log "Restart Moonraker when idle: sudo systemctl restart moonraker" >&2
    return 1
  }
  if ! (trap - EXIT; service_restart_if moonraker); then
    log "Restart Moonraker manually: sudo systemctl restart moonraker" >&2
    return 1
  fi

  log "aldis is installed. Future releases appear in the update panel of Mainsail and Fluidd." >&2
  if [ "$agent" -eq 0 ]; then
    log "The aldis service was not installed; add it later with: sudo $dir/aldis setup --agent" >&2
  fi
}
