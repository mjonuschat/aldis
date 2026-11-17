#!/usr/bin/env bash

aldis_tune_sha256_check() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "$1"
    else
        sha256sum -c "$1"
    fi
}

aldis_bootstrap() {
    local version="" args=()
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)
                if [ $# -lt 2 ]; then
                    echo "--version requires a value" >&2
                    return 1
                fi
                version="$2"; shift 2 ;;
            *) args+=("$1"); shift ;;
        esac
    done

    local api_url="https://api.github.com/repos/mjonuschat/aldis/releases/latest"
    [ -n "$version" ] && api_url="https://api.github.com/repos/mjonuschat/aldis/releases/tags/$version"

    local release_json
    if ! release_json="$(curl -fsSL "$api_url")"; then
        echo "Could not reach the GitHub API to resolve a aldis release." >&2
        return 1
    fi

    local installer_url sha_url
    installer_url="$(printf '%s' "$release_json" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(a["browser_download_url"] for a in d["assets"] if a["name"]=="aldis-installer.sh"))')"
    sha_url="$(printf '%s' "$release_json" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(a["browser_download_url"] for a in d["assets"] if a["name"]=="aldis-installer.sh.sha256"))')"

    local tmp_dir installer_path sha_path
    tmp_dir="$(mktemp -d)"
    installer_path="$tmp_dir/aldis-installer.sh"
    sha_path="$tmp_dir/aldis-installer.sh.sha256"

    curl -fsSL "$installer_url" -o "$installer_path"
    curl -fsSL "$sha_url" -o "$sha_path"

    if ! (cd "$tmp_dir" && aldis_tune_sha256_check aldis-installer.sh.sha256 >/dev/null 2>&1); then
        echo "checksum verification failed for the downloaded installer" >&2
        return 1
    fi

    [ -n "$version" ] && export ALDIS_RELEASE_API="$api_url"

    exec bash "$installer_path" "${args[@]}"
}

# When piped into bash (curl | bash) BASH_SOURCE[0] is empty, so fall back to $0.
if [ "${BATS_TEST_FILENAME:-}" = "" ] && [ "${BASH_SOURCE[0]:-$0}" = "${0}" ]; then
    aldis_bootstrap "$@"
fi
