#!/bin/sh
# shellcheck disable=SC2086 # $CRATES is a word list on purpose
# Release iphone-use: bump the crate versions, run the release gate's helper tests, commit
# "chore(release): vX.Y.Z", push main and the tag, wait for release-binaries.yml to build and
# publish the GitHub Release, then sync the plugin marketplace (it reads this repo's version
# from the latest release tag) so Claude Code plugin installs pick it up right away.
#   scripts/release.sh [--dry-run] 0.6.8
# --dry-run: preflight + checks + show the bump diff, then revert. Nothing is committed or pushed.
set -eu
run_ok() {  # run_ok <run-id> [-R owner/repo]: wait until the run completes (gh run watch can drop on a network error), then require success
  _r=$1; shift
  until [ "$(gh run view "$_r" "$@" --json status -q .status 2>/dev/null)" = completed ]; do gh run watch "$_r" "$@" >/dev/null 2>&1 || sleep 15; done
  [ "$(gh run view "$_r" "$@" --json conclusion -q .conclusion)" = success ]
}
DRY=0 V=
for a in "$@"; do
  case $a in --dry-run) DRY=1 ;; -*) V= ; break ;; *) V=${a#v} ;; esac
done
[ -n "$V" ] || { echo "usage: scripts/release.sh [--dry-run] <version>" >&2; exit 2; }
MARKETPLACE=leeguooooo/plugins PLUGIN=iphone-use
CRATES="crates/core/Cargo.toml crates/server/Cargo.toml crates/mcp/Cargo.toml"
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."

echo "$V" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' || die "bad version: $V"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
[ -z "$(git status --porcelain)" ] || die "working tree not clean"
git fetch -q origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "main is not in sync with origin/main"
git rev-parse -q --verify "refs/tags/v$V" >/dev/null && die "v$V already exists"

trap 'git checkout -q -- $CRATES Cargo.lock' EXIT  # undo the bump on --dry-run or a failed check
# Same version in all three crates (the release gate checks they match the tag) and in Cargo.lock.
for f in $CRATES; do
  sed -i.bak "1,/^version = /s/^version = \".*\"/version = \"$V\"/" "$f" && rm "$f.bak"
done
for n in core server iphone-use-mcp; do
  sed -i.bak "/^name = \"$n\"\$/{n;s/^version = \".*\"/version = \"$V\"/;}" Cargo.lock && rm Cargo.lock.bak
done
cargo metadata --locked -q --format-version 1 >/dev/null || die "Cargo.lock out of sync"

# The release gate's helper tests (release-binaries.yml "Validate installer and release coherence").
t() { "$@" >/dev/null 2>&1 || die "$* failed"; }
t bash -n install.sh
t bash scripts/test-install-release-transaction.sh
t bash scripts/test-setup-wda-warp-preflight.sh
t bash scripts/test-setup-wda-lock-backoff.sh
t bash scripts/test-setup-wda-runner-build.sh
t bash scripts/test-install-runner-sources.sh
t bash runner/ci-check.sh
t bash scripts/test-setup-wda-probe-threshold.sh
t python3 scripts/test-setup-wda-status.py
t python3 scripts/test-setup-wda-runner-product.py
t python3 scripts/test-setup-wda-asc-signing.py
t python3 scripts/test-auto-update.py
echo "checks passed"

if [ "$DRY" = 1 ]; then
  git --no-pager diff --stat
  git --no-pager diff -U0 -- $CRATES
  echo "dry run: would commit \"chore(release): v$V\", push main + v$V, wait for release-binaries.yml, sync $MARKETPLACE"
  exit 0
fi

git diff --quiet || git commit -qm "chore(release): v$V" -- $CRATES Cargo.lock
trap - EXIT
git tag "v$V"
git push -q origin main "v$V"

# release-binaries.yml builds, signs and publishes the Release on the tag push; the plugin must not
# update before its binaries exist.
RUN='' i=0
while [ -z "$RUN" ]; do
  i=$((i + 1)); [ $i -le 30 ] || die "no release-binaries.yml run for v$V after 5 min"
  sleep 10
  RUN=$(gh run list -w release-binaries.yml -b "v$V" -e push -L 1 --json databaseId -q '.[0].databaseId')
done
echo "waiting for release build: $(gh run view "$RUN" --json url -q .url)"
run_ok "$RUN" || die "release build failed: gh run view $RUN --log-failed"
gh release view "v$V" --json url -q .url

gh workflow run auto-sync-versions.yml -R "$MARKETPLACE"
sleep 5
RUN=$(gh run list -R "$MARKETPLACE" -w auto-sync-versions.yml -e workflow_dispatch -L 1 --json databaseId -q '.[0].databaseId')
run_ok "$RUN" -R "$MARKETPLACE" && echo "marketplace synced" || echo "warn: marketplace sync run $RUN failed; the hourly run will retry"
gh api "repos/$MARKETPLACE/contents/.claude-plugin/marketplace.json" -q .content | base64 -d \
  | python3 -c "import json,sys; print('marketplace $PLUGIN:', next(p['version'] for p in json.load(sys.stdin)['plugins'] if p['name']=='$PLUGIN'))"
