#!/bin/sh
# Rust build cache for ci.yml on the self-hosted CI runner: sccache with a local disk
# cache on the PR VM's persistent cache disk (/var/cache/hlab-ci/deps), which
# the guest keeps under 4 GiB (LRU) and outside the root's 15 GiB free-space
# floor. Each runner slot gets its own cache and server port, so concurrent
# jobs never share a server. Without the cache directory, the job builds
# without sccache.
#
#   ci_sccache.sh setup   install the pinned sccache, export its env to the job
#   ci_sccache.sh stats   print hit/miss stats and cache size, stop the server
set -eu

root=${CI_CACHE_ROOT:-/var/cache/hlab-ci/deps}/axe-sccache
version=v0.18.0
asset=sccache-$version-x86_64-unknown-linux-musl
asset_sha256=45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89
bin_sha256=973cb15f6a986d84ca334bbed3bbe2eb8f1ee8fd81bf9e115b8539a293bf8d59
slot=${HLAB_RUNNER_SLOT:-0}
cache_dir=$root/slot-$slot
# Two slots of 1.5 GiB plus the npm cache stay under the guest's 4 GiB.
cache_size=1500M
port=$((4300 + slot))
# The file must be named `sccache`: under any other name sccache acts as the
# compiler of that name and calls itself.
bin=$root/bin/$version/sccache

# Every step is checked explicitly: `set -e` does not apply inside `if`.
install_sccache() {
  if [ -x "$bin" ] && echo "$bin_sha256  $bin" | sha256sum -c --status; then
    return 0
  fi
  tmp=$(mktemp -d "$root/bin/.install.XXXXXX") || return 1
  curl -fsSL --retry 3 -o "$tmp/$asset.tar.gz" \
    "https://github.com/mozilla/sccache/releases/download/$version/$asset.tar.gz" &&
    echo "$asset_sha256  $tmp/$asset.tar.gz" | sha256sum -c --status &&
    tar -xzf "$tmp/$asset.tar.gz" -C "$tmp" "$asset/sccache" &&
    echo "$bin_sha256  $tmp/$asset/sccache" | sha256sum -c --status &&
    chmod 0755 "$tmp/$asset/sccache" &&
    mv -f "$tmp/$asset/sccache" "$bin"
  status=$?
  rm -rf "$tmp"
  return $status
}

case "${1:-}" in
  setup)
    if ! mkdir -p "$root/bin/$version" 2>/dev/null || [ ! -w "$root/bin/$version" ]; then
      echo "::warning::no writable $root; building without sccache"
      exit 0
    fi
    if ! install_sccache; then
      echo "::warning::could not install sccache $version; building without it"
      exit 0
    fi
    # The guest's LRU sweep (run by the other slot's jobs too) may evict the
    # cached copy mid-job, so the job runs its own copy.
    touch "$bin"
    wrapper=$bin
    # A unique directory: a running server keeps an earlier copy busy.
    if [ -n "${RUNNER_TEMP:-}" ] && copy=$(mktemp -d "$RUNNER_TEMP/sccache.XXXXXX") &&
        cp "$bin" "$copy/sccache"; then
      wrapper=$copy/sccache
    fi
    # A server that cannot start in time would stall every rustc call.
    if ! SCCACHE_DIR=$cache_dir SCCACHE_CACHE_SIZE=$cache_size SCCACHE_SERVER_PORT=$port \
        timeout 60 "$wrapper" --start-server; then
      echo "::warning::sccache server did not start; building without it"
      SCCACHE_SERVER_PORT=$port timeout 10 "$wrapper" --stop-server >/dev/null 2>&1 || true
      exit 0
    fi
    {
      echo "RUSTC_WRAPPER=$wrapper"
      echo "SCCACHE_DIR=$cache_dir"
      echo "SCCACHE_CACHE_SIZE=$cache_size"
      echo "SCCACHE_SERVER_PORT=$port"
      # sccache cannot cache incremental builds.
      echo "CARGO_INCREMENTAL=0"
    } >> "${GITHUB_ENV:-/dev/stdout}"
    ;;
  stats)
    if [ -n "${RUSTC_WRAPPER:-}" ]; then
      # Bounded: a wedged server must not hold the runner.
      timeout 60 "$RUSTC_WRAPPER" --show-stats || true
      timeout 30 "$RUSTC_WRAPPER" --stop-server >/dev/null 2>&1 || true
    fi
    du -sh "$root" 2>/dev/null || true
    ;;
  *)
    echo "usage: $0 setup|stats" >&2
    exit 2
    ;;
esac
