setup() {
  export TEST_DIR="$BATS_TMPDIR/aldis-bootstrap-$$"
  mkdir -p "$TEST_DIR"

  curl() {
    echo "$*" >> "$TEST_DIR/curl.log"
    case "$*" in
      *api.github.com*releases/latest*)
        cat "$TEST_DIR/latest_release.json" ;;
      *api.github.com*releases/tags/*)
        cat "$TEST_DIR/tagged_release.json" ;;
      *installer.sh.sha256*)
        cp "$TEST_DIR/installer.sh.sha256" "$(_curl_output_arg "$@")" ;;
      *installer.sh*)
        cp "$TEST_DIR/installer.sh" "$(_curl_output_arg "$@")" ;;
      *) echo "unexpected curl: $*" >&2; return 1 ;;
    esac
  }
  _curl_output_arg() {
    while [ $# -gt 0 ]; do
      if [ "$1" = "-o" ]; then echo "$2"; return; fi
      shift
    done
  }
  exec() {
    case "$1" in
      bash)
        echo "exec:$*" >> "$TEST_DIR/exec.log"
        echo "release_api:${ALDIS_RELEASE_API-unset}" >> "$TEST_DIR/exec.log" ;;
      *) command exec "$@" ;;
    esac
  }
  export -f curl _curl_output_arg exec

  printf 'echo real installer ran\n' > "$TEST_DIR/installer.sh"
  { shasum -a 256 "$TEST_DIR/installer.sh" 2>/dev/null || sha256sum "$TEST_DIR/installer.sh"; } | awk '{print $1"  aldis-installer.sh"}' > "$TEST_DIR/installer.sh.sha256"
  cat > "$TEST_DIR/latest_release.json" <<EOF
{"tag_name": "v0.3.0", "assets": [
  {"name": "aldis-installer.sh", "browser_download_url": "https://example.test/installer.sh"},
  {"name": "aldis-installer.sh.sha256", "browser_download_url": "https://example.test/installer.sh.sha256"}
]}
EOF
  cp "$TEST_DIR/latest_release.json" "$TEST_DIR/tagged_release.json"

  source "$BATS_TEST_DIRNAME/../../install.sh"
}
teardown() { rm -rf "$TEST_DIR"; }

@test "aldis_bootstrap resolves latest and execs the verified installer with forwarded args" {
  run aldis_bootstrap --diagnostics
  [ "$status" -eq 0 ]
  grep -q 'exec:bash .*installer.sh --diagnostics' "$TEST_DIR/exec.log"
}

@test "aldis_bootstrap consumes --version and does not forward it" {
  run aldis_bootstrap --version v0.2.0 --force
  [ "$status" -eq 0 ]
  grep -q -- '--force' "$TEST_DIR/exec.log"
  ! grep -q -- '--version' "$TEST_DIR/exec.log"
  grep -q 'api.github.com/repos/mjonuschat/aldis/releases/tags/v0.2.0' "$TEST_DIR/curl.log"
}

@test "aldis_bootstrap pins the installer's release API to the --version tag" {
  run aldis_bootstrap --version v0.2.0
  [ "$status" -eq 0 ]
  grep -q '^release_api:.*/releases/tags/v0.2.0$' "$TEST_DIR/exec.log"
}

@test "aldis_bootstrap leaves the installer's release API unset without --version" {
  run aldis_bootstrap
  [ "$status" -eq 0 ]
  grep -q '^release_api:unset$' "$TEST_DIR/exec.log"
}

@test "aldis_bootstrap aborts on a checksum mismatch" {
  printf 'tampered\n' >> "$TEST_DIR/installer.sh"
  run aldis_bootstrap
  [ "$status" -eq 1 ]
  [[ "$output" == *"checksum"* ]]
  [ ! -f "$TEST_DIR/exec.log" ]
}

@test "aldis_bootstrap fails with a clear error when --version has no value" {
  run timeout 5 bash -c "source $BATS_TEST_DIRNAME/../../install.sh; aldis_bootstrap --version"
  [ "$status" -eq 1 ]
  [[ "$output" == *"--version requires a value"* ]]
}

@test "aldis_bootstrap fails clearly when the GitHub API is unreachable" {
  curl() {
    case "$*" in
      *api.github.com*) return 7 ;;  # curl's connection-failed exit code
      *) echo "unexpected curl: $*" >&2; return 1 ;;
    esac
  }
  export -f curl
  run aldis_bootstrap
  [ "$status" -eq 1 ]
  [[ "$output" == *"GitHub API"* ]]
  [ ! -f "$TEST_DIR/exec.log" ]
}

@test "piping install.sh into bash runs the bootstrap and forwards args" {
  run env -u BATS_TEST_FILENAME bash -c "cat $BATS_TEST_DIRNAME/../../install.sh | bash -s -- --diagnostics"
  [ "$status" -eq 0 ]
  grep -q 'exec:bash .*installer.sh --diagnostics' "$TEST_DIR/exec.log"
}
