#!/usr/bin/env bash
# gx version string: <upstream-version>+gx.<release>
#
# <upstream-version> is read from the [package] section of
# crates/codegen/xai-grok-version/Cargo.toml (the lockstepped grok CLI
# version). <release> is read from scripts/gx/GX_RELEASE, a plain integer
# bumped once per gx-fork release cut.
#
# Usage: scripts/gx/version.sh
# Prints e.g. `1.0.8+gx.1` on stdout.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
repo_root="$(cd -- "${script_dir}/../.." >/dev/null 2>&1 && pwd -P)"

upstream_cargo_toml="${repo_root}/crates/codegen/xai-grok-version/Cargo.toml"
release_file="${script_dir}/GX_RELEASE"

if [[ ! -f "${upstream_cargo_toml}" ]]; then
    echo "gx/version.sh: missing ${upstream_cargo_toml}" >&2
    exit 1
fi

if [[ ! -f "${release_file}" ]]; then
    echo "gx/version.sh: missing ${release_file}" >&2
    exit 1
fi

# Extract the version field from within the [package] section only, so a
# same-named key in a later section (e.g. [dependencies]) can never be
# mistaken for the crate version.
upstream_version="$(awk '
    /^\[package\]/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ { print; exit }
' "${upstream_cargo_toml}" | sed -E 's/^version[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/')"

if [[ -z "${upstream_version}" ]]; then
    echo "gx/version.sh: could not parse [package] version from ${upstream_cargo_toml}" >&2
    exit 1
fi

# The result is concatenated into a semver string, so anything that is not a
# plain MAJOR.MINOR.PATCH would silently produce an invalid version.
if [[ ! "${upstream_version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "gx/version.sh: [package] version in ${upstream_cargo_toml} is not X.Y.Z: '${upstream_version}'" >&2
    exit 1
fi

gx_release="$(tr -d '[:space:]' <"${release_file}")"

if [[ -z "${gx_release}" ]]; then
    echo "gx/version.sh: ${release_file} is empty" >&2
    exit 1
fi

# Must be a bare integer: it becomes the `+gx.<release>` build-metadata
# identifier, and e.g. `1 # comment` would yield an unparseable version.
if [[ ! "${gx_release}" =~ ^[0-9]+$ ]]; then
    echo "gx/version.sh: ${release_file} must contain a plain integer, got: '${gx_release}'" >&2
    exit 1
fi

echo "${upstream_version}+gx.${gx_release}"
