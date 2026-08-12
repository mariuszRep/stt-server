#!/usr/bin/env bash
# Audits a release artifact directory before it's uploaded: no .projectflows
# goal files, no whisper-vibes/App source, no credentials or .env files.
# Modeled on stt-sdk/scripts/verify-consumer.mjs's tarball audit — same
# "prove a release artifact is clean before it ships" idea, adapted for a
# directory of platform binaries instead of an npm tarball.

set -euo pipefail

DIR="${1:?usage: verify-release-artifact.sh <artifact-directory>}"

if [ ! -d "$DIR" ]; then
    echo "ERROR: $DIR is not a directory"
    exit 1
fi

fail=0

check_absent() {
    local pattern="$1"
    local reason="$2"
    if find "$DIR" -iname "$pattern" | grep -q .; then
        echo "FAIL: found files matching '$pattern' ($reason):"
        find "$DIR" -iname "$pattern"
        fail=1
    fi
}

check_absent ".projectflows" "goal-tracking files should never ship in a release"
check_absent "*.env" "environment files may contain secrets"
check_absent ".env.local" "environment files may contain secrets"
check_absent "*credentials*" "credential-shaped filenames"
check_absent "whisper-vibes" "App source must never ship inside stt-server releases"

# Grep-based secondary check: look for the App's own package identifiers
# inside any text files, in case source got flattened/copied without the
# giveaway directory name surviving.
if grep -RIl "whisper-vibes" "$DIR" 2>/dev/null | grep -q .; then
    echo "FAIL: found references to 'whisper-vibes' inside artifact contents:"
    grep -RIl "whisper-vibes" "$DIR" 2>/dev/null
    fail=1
fi

if [ "$fail" -eq 0 ]; then
    echo "PASS: $DIR contains no goal files, App source, or obvious credentials."
fi

exit "$fail"
