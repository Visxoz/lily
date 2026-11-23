#!/usr/bin/env bash
# Release notes from the commits between two releases: feat under Top
# Features, fix and perf under Changelog. Other types, and the version bump,
# are left out, as they change nothing a user sees.
#
#   release-notes.sh <previous-tag> <tag> [previous-ref] [ref]
#
# The refs default to the tags.
set -euo pipefail

previous=$1
tag=$2
previous_ref=${3:-$previous}
ref=${4:-$tag}
repo=${GITHUB_REPOSITORY:-Visxoz/lily}

# Outside the sections' pipelines, so a bad ref still fails the release.
subjects=$(git log --format='%s' --reverse "$previous_ref..$ref")

# Each subject as a sentence, with its type taken off and a full stop added.
found=
section() {
    local lines
    lines=$(printf '%s\n' "$subjects" | grep -E "^($2): " \
        | sed -E -e 's/^[a-z]+: /- /' -e 's/([^.!?])$/\1./' || true)
    if [ -n "$lines" ]; then
        found=1
        printf '## %s\n\n%s\n\n' "$1" "$lines"
    fi
}

# From the release's own tag, so old notes keep the banner they shipped with.
printf '<img src="https://raw.githubusercontent.com/%s/%s/docs/banner.svg" width="100%%" alt="Lily: a native YouTube Music client for Windows">\n\n' "$repo" "$tag"

section 'Top Features' feat
section Changelog 'fix|perf'
if [ -z "$found" ]; then
    printf 'Changes behind the scenes, with nothing new to see.\n\n'
fi

cat <<EOF
Full list of changes: https://github.com/$repo/compare/$previous...$tag

## Install

**Windows 10 or later** (64-bit): Already have Lily? It offers ${tag#v} itself, in the card above the player bar or under **Settings ▸ About ▸ Updates**, and your sign-in, library and downloads stay. New to Lily? Download \`lily_${tag#v}_x64-setup.exe\` below to install it, or \`lily_${tag#v}_x64-portable.exe\` to run it from anywhere without installing.

The app isn't code-signed yet, so if Windows says it protected your PC, choose **More info**, then **Run anyway**. If Smart App Control blocks it, right-click the file, choose **Properties**, tick **Unblock**, and run it again.

**Found a bug?** [Open an issue](https://github.com/$repo/issues/new).
EOF
