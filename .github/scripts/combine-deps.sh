#!/usr/bin/env bash
# combine-deps — fold every open Dependabot pull request into a single one.
#
# Dependabot groups within an ecosystem but never across them, so even a
# correctly grouped repository still opens one pull request per ecosystem.
# This replays all of them onto one branch and closes the originals against it.
#
# Re-running is safe: the branch is rebuilt from the base branch every time, so
# a pull request that has since merged or closed simply drops out.
#
# A repository with artefacts that must be regenerated after a bump (a lockfile
# in a second module, a checked-in hash manifest) puts that in
# .github/scripts/combine-deps-local.sh, which runs after the replay and before
# the pull request is opened. Anything it commits rides along.
#
# One caveat before you close the combined pull request: closing a Dependabot
# pull request tells Dependabot not to re-open one for that same version.
# Merging moves the dependency, so the next pass proposes the next version and
# nothing is lost. Abandoning it instead forfeits that batch until a newer
# version ships.
set -euo pipefail

BRANCH="deps/combined"
BASE="$(gh repo view --json defaultBranchRef --jq .defaultBranchRef.name)"

git fetch --quiet origin "$BASE"

mapfile -t ROWS < <(
  gh pr list --state open --author "app/dependabot" --base "$BASE" \
    --limit 100 --json number,title --jq '.[] | [.number, .title] | @tsv' | sort -n
)

if [ "${#ROWS[@]}" -eq 0 ]; then
  echo "combine-deps: no open Dependabot pull requests; nothing to combine."
  exit 0
fi

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git checkout --quiet -B "$BRANCH" "origin/$BASE"

combined=()
skipped=()

for row in "${ROWS[@]}"; do
  number="${row%%$'\t'*}"
  title="${row#*$'\t'}"

  git fetch --quiet origin "refs/pull/${number}/head:refs/combine/${number}"
  base_point="$(git merge-base "origin/$BASE" "refs/combine/${number}")"

  if git cherry-pick --allow-empty "${base_point}..refs/combine/${number}" >/dev/null 2>&1; then
    combined+=("${number}"$'\t'"${title}")
    echo "combine-deps: replayed #${number} — ${title}"
  else
    git cherry-pick --abort >/dev/null 2>&1 || true
    skipped+=("${number}"$'\t'"${title}")
    echo "combine-deps: SKIPPED #${number} (conflicts with an earlier bump) — ${title}"
  fi
done

if [ "${#combined[@]}" -eq 0 ]; then
  echo "combine-deps: every pull request conflicted; leaving them all open."
  exit 0
fi

if [ -x .github/scripts/combine-deps-local.sh ]; then
  echo "combine-deps: running repository-local post-processing."
  .github/scripts/combine-deps-local.sh
fi

body_file="$(mktemp)"
{
  echo "Every open Dependabot pull request, replayed onto one branch by"
  echo "\`.github/workflows/combine-deps.yml\` so the week's dependency work is"
  echo "reviewed and merged once instead of per ecosystem."
  echo
  echo "### Combined"
  echo
  for row in "${combined[@]}"; do
    echo "- #${row%%$'\t'*} — ${row#*$'\t'}"
  done
  if [ "${#skipped[@]}" -gt 0 ]; then
    echo
    echo "### Left open (conflicted on replay, merge these by hand)"
    echo
    for row in "${skipped[@]}"; do
      echo "- #${row%%$'\t'*} — ${row#*$'\t'}"
    done
  fi
} > "$body_file"

git push --quiet --force-with-lease origin "$BRANCH"

existing="$(gh pr list --state open --head "$BRANCH" --json number --jq '.[0].number // empty')"
if [ -n "$existing" ]; then
  gh pr edit "$existing" --body-file "$body_file"
  combined_pr="$existing"
else
  gh pr create --base "$BASE" --head "$BRANCH" \
    --title "build(deps): combined dependency updates" --body-file "$body_file"
  combined_pr="$(gh pr list --state open --head "$BRANCH" --json number --jq '.[0].number')"
fi

for row in "${combined[@]}"; do
  number="${row%%$'\t'*}"
  gh pr comment "$number" --body "Superseded by #${combined_pr}, which carries this bump together with the rest of the week's dependency updates."
  gh pr close "$number"
done

echo "combine-deps: combined ${#combined[@]} pull request(s) into #${combined_pr}."
