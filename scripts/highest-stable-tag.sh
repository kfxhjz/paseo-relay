#!/usr/bin/env bash
# Print the highest stable release tag (vX.Y.Z, no prerelease suffix) whose
# commit is reachable from origin/main, without the leading "v".
#
# The release workflow uses this as the definition of "current latest": a
# stable tag may move the floating `latest` image tag only if no higher stable
# release tag exists on main. Prerelease tags (vX.Y.Z-rc.1) and tags that point
# at commits outside main (for example a typo tag pushed to an old branch) are
# ignored, so they cannot freeze or hijack `latest`.
#
# Prints nothing only when no such tag exists. Requires that refs/tags/* and
# refs/remotes/origin/main are up to date (the caller fetches them).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Keep the command substitution so a failure to list refs aborts the script.
# Read the result through a here-string rather than a pipeline: breaking out of
# a pipeline early would close its read end and could fail with SIGPIPE under
# `pipefail`.
if ! git rev-parse --verify --quiet 'refs/remotes/origin/main^{commit}' >/dev/null; then
  echo "origin/main is not available; fetch it before resolving the highest tag" >&2
  exit 2
fi
refs="$(git for-each-ref --sort=-v:refname --format='%(refname:strip=2)' refs/tags)"
while IFS= read -r ref; do
  if [[ ! "${ref}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    continue
  fi
  version="${ref#v}"
  if git merge-base --is-ancestor "refs/tags/v${version}^{commit}" refs/remotes/origin/main; then
    printf '%s\n' "${version}"
    break
  else
    status=$?
    # 1 means "not an ancestor"; anything else is a repository/read error and
    # must not look like "no higher tag exists" to the release workflow.
    if [ "${status}" -ne 1 ]; then
      echo "git merge-base failed for refs/tags/v${version} (status ${status})" >&2
      exit "${status}"
    fi
  fi
done <<< "${refs}"
