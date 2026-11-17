setup() {
  REPO_ROOT="$BATS_TEST_DIRNAME/../.."
  rm -rf "$REPO_ROOT/build"
}
teardown() { rm -rf "$REPO_ROOT/build"; }

@test "make installer builds an executable script that --help exits 0" {
  run make -C "$REPO_ROOT" installer
  [ "$status" -eq 0 ]
  [ -x "$REPO_ROOT/build/aldis-installer.sh" ]
  bash -n "$REPO_ROOT/build/aldis-installer.sh"
  run bash "$REPO_ROOT/build/aldis-installer.sh" --help
  [ "$status" -eq 0 ]
}

@test "make installer writes a checksum that shasum verifies" {
  run make -C "$REPO_ROOT" installer
  [ "$status" -eq 0 ]
  cd "$REPO_ROOT/build"
  run shasum -a 256 -c aldis-installer.sh.sha256
  [ "$status" -eq 0 ]
}
