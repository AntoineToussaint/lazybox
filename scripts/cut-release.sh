#!/usr/bin/env bash
# Validate and publish a lazybox release from the exact main-branch commit.

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
step() { echo "==> $*"; }

usage() {
	cat <<'EOF'
Usage: scripts/cut-release.sh <version> [--publish] [--manual-checks-confirmed]

Without --publish, runs the complete automated release preflight but does not
create or push a tag. Publishing additionally requires the explicit manual-
checks attestation because the real-provider and dogfood checks cannot be
proved by this script.

Examples:
  scripts/cut-release.sh 0.1.15
  scripts/cut-release.sh 0.1.15 --publish --manual-checks-confirmed
EOF
}

version=""
publish=false
manual_checks_confirmed=false
while (($#)); do
	case "$1" in
		--publish) publish=true ;;
		--manual-checks-confirmed) manual_checks_confirmed=true ;;
		-h|--help) usage; exit 0 ;;
		-*) die "unknown option: $1" ;;
		*) [[ -z "$version" ]] || die "version supplied more than once"; version="$1" ;;
	esac
	shift
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+][0-9A-Za-z.-]+)?$ ]] \
	|| die "version must be SemVer without a leading v (for example 0.1.15)"

for command_name in git gh cargo make npm jq; do
	command -v "$command_name" >/dev/null || die "required command not found: $command_name"
done

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not in a git checkout"
cd "$repo_root"

remote_url="$(git remote get-url origin)"
case "$remote_url" in
	git@github.com:AntoineToussaint/lazybox.git|https://github.com/AntoineToussaint/lazybox.git) ;;
	*) die "origin is not the canonical lazybox repository: $remote_url" ;;
esac

[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] \
	|| die "working tree is not clean"

step "refreshing canonical refs"
git fetch origin main --tags --prune
candidate="$(git rev-parse HEAD)"
main_tip="$(git rev-parse origin/main)"
[[ "$candidate" == "$main_tip" ]] \
	|| die "HEAD ($candidate) is not the current origin/main tip ($main_tip)"

tag="v$version"
if git show-ref --verify --quiet "refs/tags/$tag" \
	|| [[ -n "$(git ls-remote --tags origin "refs/tags/$tag")" ]]; then
	die "tag already exists: $tag"
fi
if gh release view "$tag" --repo AntoineToussaint/lazybox >/dev/null 2>&1; then
	die "GitHub Release already exists: $tag"
fi

step "validating release identity"
manifest_version="$(sed -n '/^\[workspace.package\]$/,/^\[/s/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n1)"
[[ "$manifest_version" == "$version" ]] \
	|| die "Cargo.toml version is '$manifest_version', expected '$version'"
installer_version="$(sed -n 's/^release_version="\([^"]*\)"$/\1/p' crates/tui-boot/lazybox-tui-installer.sh)"
[[ "$installer_version" == "$version" ]] \
	|| die "installer version is '$installer_version', expected '$version'"
grep -q "^## \[$version\] - [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$" CHANGELOG.md \
	|| die "CHANGELOG.md has no dated [$version] section"

release_notes="$(awk -v wanted="## [$version] -" '
	index($0, wanted) == 1 { inside=1 }
	inside && seen && /^## \[/ { exit }
	inside { print; seen=1 }
' CHANGELOG.md)"
printf '%s\n' "$release_notes" | scripts/check-release-notes.sh

# Unreleased must be empty. Shipping notes above the release heading would make
# the tag and the curated GitHub Release disagree about what it contains.
unreleased_body="$(awk '/^## \[Unreleased\]/{inside=1; next} inside && /^## /{exit} inside{print}' CHANGELOG.md)"
if printf '%s\n' "$unreleased_body" | grep -Eq '^[-*] |^### '; then
	die "CHANGELOG.md still has Unreleased entries; fold them into [$version]"
fi

bad_workspace_versions="$(cargo metadata --locked --no-deps --format-version 1 \
	| jq -r --arg version "$version" '.packages[] | select((.name | startswith("lazybox-")) or (.name | startswith("libghostty-vt"))) | select(.version != $version) | "\(.name)=\(.version)"')"
[[ -z "$bad_workspace_versions" ]] \
	|| die "workspace packages do not match $version: $bad_workspace_versions"

bad_desktop_versions="$(awk -v wanted="$version" '
	/^name = "(lazybox-|libghostty-vt)/ {
		package=$0
		getline
		if (package != "name = \"lazybox-desktop\"" && $0 != "version = \"" wanted "\"") print package " " $0
	}
' apps/desktop/src-tauri/Cargo.lock)"
[[ -z "$bad_desktop_versions" ]] \
	|| die "desktop lockfile has stale lazybox versions: $bad_desktop_versions"

step "checking GitHub gates for $candidate"
required_workflows=("CI" "Performance benchmarks")
for workflow in "${required_workflows[@]}"; do
	conclusion="$(gh run list --repo AntoineToussaint/lazybox --commit "$candidate" \
		--workflow "$workflow" --limit 1 --json conclusion --jq '.[0].conclusion // "missing"')"
	[[ "$conclusion" == "success" ]] \
		|| die "$workflow is not green for $candidate (conclusion: $conclusion)"
done

step "preparing pinned build dependencies"
make setup

step "verifying generated contracts are current"
make desktop-contract
make web-control-contract
git diff --exit-code -- apps/desktop/src/generated crates/server/src/api_client_contract.json \
	|| die "generated contracts are stale"

step "running the local source gates"
make release-gates

step "building the exact release artifact offline"
make release
target_directory="$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.target_directory')"
[[ -n "$target_directory" && "$target_directory" != "null" ]] \
	|| die "cargo metadata did not report a target directory"
release_binary="${target_directory}/release/lazybox"
"$release_binary" --version | grep -F "$version" >/dev/null \
	|| die "release binary does not report version $version"

[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] \
	|| die "release checks changed the working tree"

echo "preflight passed: $tag at $candidate"
if [[ "$publish" != true ]]; then
	echo "dry run only: re-run with --publish --manual-checks-confirmed after completing the manual checklist"
	exit 0
fi

[[ "$manual_checks_confirmed" == true ]] \
	|| die "publishing requires --manual-checks-confirmed after completing docs/dev/release-checklist.md"

step "creating and pushing annotated tag $tag"
git tag -a "$tag" "$candidate" -m "lazybox $version"
if ! git push origin "refs/tags/$tag"; then
	git tag -d "$tag" >/dev/null
	die "tag push failed; removed the unpushed local tag"
fi

step "waiting for tag-triggered release workflows"
deadline=$((SECONDS + 1800))
while ((SECONDS < deadline)); do
	# shellcheck disable=SC2016 # jq program; $runs is a jq variable.
	release_state="$(gh run list --repo AntoineToussaint/lazybox --branch "$tag" --event push --limit 20 \
		--json name,status,conclusion --jq '
			[.[] | select(.name == "Release" or .name == "Release desktop")] as $runs |
			if ($runs | length) == 0 then "waiting"
			elif any($runs[]; .conclusion != "" and .conclusion != null and .conclusion != "success" and .conclusion != "skipped") then "failed"
			elif all($runs[]; .status == "completed") then "success"
			else "running" end')"
	case "$release_state" in
		success) break ;;
		failed) die "a tag-triggered release workflow failed; inspect: gh run list --branch $tag" ;;
	esac
	sleep 15
done
[[ "${release_state:-waiting}" == "success" ]] || die "timed out waiting for release workflows"

gh release view "$tag" --repo AntoineToussaint/lazybox >/dev/null \
	|| die "workflows completed but GitHub Release $tag is missing"
echo "released: https://github.com/AntoineToussaint/lazybox/releases/tag/$tag"
