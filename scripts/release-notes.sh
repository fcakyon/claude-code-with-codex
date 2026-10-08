#!/usr/bin/env bash
# Print GitHub-style release notes for a tag: every non-merge commit since the
# previous release, then the compare link. GitHub's own generator picks the
# previous release but lists only merged PRs, which this repo rarely has.
set -euo pipefail

tag="$1"
repo="${GITHUB_REPOSITORY:-fcakyon/claude-code-with-codex}"
compare="$(gh api "repos/$repo/releases/generate-notes" -f tag_name="$tag" --jq .body \
  | grep -o "https://github.com/$repo/compare/[^ ]*")"

echo "## What's Changed"
gh api "repos/$repo/compare/${compare##*/}" --jq '.commits[]
  | select(.parents | length == 1)
  | "* \(.commit.message | split("\n")[0]) by @\(.author.login // .commit.author.name) in \(.sha)"'
echo
echo "**Full Changelog**: $compare"
