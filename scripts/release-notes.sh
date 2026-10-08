#!/usr/bin/env bash
# Print GitHub-style release notes for a tag: every non-merge commit since the
# previous release, fork commits first, then the compare link. GitHub's own
# generator picks the previous release but lists only merged PRs, which this
# repo rarely has. The upstream owner is named without an @ so a sync does not
# ping them, while other upstream contributors keep their mention.
set -euo pipefail

tag="$1"
repo="${GITHUB_REPOSITORY:-fcakyon/claude-code-with-codex}"
upstream_owner="raine"
compare="$(gh api "repos/$repo/releases/generate-notes" -f tag_name="$tag" --jq .body \
  | grep -o "https://github.com/$repo/compare/[^ ]*")"
fork_shas="$(gh api --paginate "repos/$repo/compare/$upstream_owner:main...$tag?per_page=100" \
  --jq '.commits[].sha' | jq -R . | jq -s .)"

echo "## What's Changed"
gh api "repos/$repo/compare/${compare##*/}" | jq -r --argjson fork "$fork_shas" --arg owner "$upstream_owner" '
  [.commits[] | select(.parents | length == 1)]
  | sort_by(.sha as $sha | $fork | index($sha) == null)
  | .[]
  | (.author.login // .commit.author.name) as $who
  | "* \(.commit.message | split("\n")[0]) by \(if $who == $owner then $who else "@\($who)" end) in \(.sha)"'
echo
echo "**Full Changelog**: $compare"
