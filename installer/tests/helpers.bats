setup() {
  TEST_ROOT="$(mktemp -d "${BATS_TMPDIR:-/tmp}/aldis-helpers.XXXXXX")"
  log() { :; }
  export -f log
  source "$BATS_TEST_DIRNAME/../installer.sh"
}
teardown() { rm -rf "$TEST_ROOT"; }

digest_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

@test "maps_aarch64_armv7l_and_x86_64" {
  run aldis_map_arch aarch64
  [ "$status" -eq 0 ]
  [ "$output" = "aarch64-linux" ]
  run aldis_map_arch armv7l
  [ "$status" -eq 0 ]
  [ "$output" = "armv7l-linux" ]
  run aldis_map_arch x86_64
  [ "$status" -eq 0 ]
  [ "$output" = "x86_64-linux" ]
}

@test "rejects_armv6l_and_unknown_arches_with_a_message" {
  for arch in armv6l riscv64 ""; do
    run aldis_map_arch "$arch"
    [ "$status" -eq 1 ]
    [ -n "$output" ]
  done
}

@test "checksum_passes_on_a_matching_line" {
  printf 'payload' > "$TEST_ROOT/aldis-armv7l-linux"
  printf '%s  aldis-armv7l-linux\n%s  aldis-x86_64-linux\n' "$(digest_of "$TEST_ROOT/aldis-armv7l-linux")" \
    "$(printf 'x%.0s' {1..64})" > "$TEST_ROOT/SHA256SUMS"
  run aldis_verify_checksum "$TEST_ROOT/SHA256SUMS" aldis-armv7l-linux "$TEST_ROOT/aldis-armv7l-linux"
  [ "$status" -eq 0 ]
}

@test "checksum_fails_on_a_mismatched_digest" {
  printf 'payload' > "$TEST_ROOT/aldis-armv7l-linux"
  printf '%s  aldis-armv7l-linux\n' "$(printf '0%.0s' {1..64})" > "$TEST_ROOT/SHA256SUMS"
  run aldis_verify_checksum "$TEST_ROOT/SHA256SUMS" aldis-armv7l-linux "$TEST_ROOT/aldis-armv7l-linux"
  [ "$status" -ne 0 ]
}

@test "checksum_fails_when_the_asset_has_no_line" {
  printf 'payload' > "$TEST_ROOT/aldis-armv7l-linux"
  printf '%s  aldis-x86_64-linux\n' "$(digest_of "$TEST_ROOT/aldis-armv7l-linux")" > "$TEST_ROOT/SHA256SUMS"
  run aldis_verify_checksum "$TEST_ROOT/SHA256SUMS" aldis-armv7l-linux "$TEST_ROOT/aldis-armv7l-linux"
  [ "$status" -ne 0 ]
}

@test "checksum_fails_on_a_duplicated_line" {
  printf 'payload' > "$TEST_ROOT/aldis-armv7l-linux"
  d="$(digest_of "$TEST_ROOT/aldis-armv7l-linux")"
  printf '%s  aldis-armv7l-linux\n%s  aldis-armv7l-linux\n' "$d" "$d" > "$TEST_ROOT/SHA256SUMS"
  run aldis_verify_checksum "$TEST_ROOT/SHA256SUMS" aldis-armv7l-linux "$TEST_ROOT/aldis-armv7l-linux"
  [ "$status" -ne 0 ]
}

@test "install_files_creates_binary_symlink_and_release_info_with_the_exact_tag" {
  printf 'binary' > "$TEST_ROOT/dl"
  run aldis_install_files "$TEST_ROOT/dest" aldis-armv7l-linux "$TEST_ROOT/dl" v1.2.3-rc.1 mjonuschat aldis
  [ "$status" -eq 0 ]
  [ -x "$TEST_ROOT/dest/aldis-armv7l-linux" ]
  [ "$(cat "$TEST_ROOT/dest/aldis-armv7l-linux")" = "binary" ]
  [ "$(readlink "$TEST_ROOT/dest/aldis")" = "aldis-armv7l-linux" ]
  [ "$(cat "$TEST_ROOT/dest/aldis")" = "binary" ]
  grep -q '"project_name": *"aldis"' "$TEST_ROOT/dest/release_info.json"
  grep -q '"project_owner": *"mjonuschat"' "$TEST_ROOT/dest/release_info.json"
  grep -q '"version": *"v1.2.3-rc.1"' "$TEST_ROOT/dest/release_info.json"
  grep -q '"asset_name": *"aldis-armv7l-linux"' "$TEST_ROOT/dest/release_info.json"
}

@test "install_files_replaces_an_older_arch_binary_and_symlink" {
  printf 'old' > "$TEST_ROOT/dl-old"
  printf 'new' > "$TEST_ROOT/dl-new"
  aldis_install_files "$TEST_ROOT/dest" aldis-armv7l-linux "$TEST_ROOT/dl-old" v1.0.0 mjonuschat aldis
  run aldis_install_files "$TEST_ROOT/dest" aldis-aarch64-linux "$TEST_ROOT/dl-new" v1.1.0 mjonuschat aldis
  [ "$status" -eq 0 ]
  [ "$(cat "$TEST_ROOT/dest/aldis")" = "new" ]
  [ -x "$TEST_ROOT/dest/aldis-aarch64-linux" ]
  grep -q '"version": *"v1.1.0"' "$TEST_ROOT/dest/release_info.json"
  grep -q '"asset_name": *"aldis-aarch64-linux"' "$TEST_ROOT/dest/release_info.json"
}
