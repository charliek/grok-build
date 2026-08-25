#!/usr/bin/env bash
# Build the gx binary: a renamed release artifact of the existing
# xai-grok-pager-bin crate (bin name xai-grok-pager). No crate renames --
# this script just stamps the gx version via GROK_VERSION and copies the
# resulting binary to `gx`, right next to the artifact cargo produced.
#
# Usage: scripts/gx/build.sh [--out <path>]
#
#   --out <path>   Also copy the built binary to <path> (e.g. an install
#                   destination like ~/.local/bin/gx). Parent directories
#                   are created if needed.
#
# Can be invoked from anywhere; the repo root is resolved from this
# script's own location.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
repo_root="$(cd -- "${script_dir}/../.." >/dev/null 2>&1 && pwd -P)"

gx_bin_target="xai-grok-pager"

out_path=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --out)
            if [[ $# -lt 2 ]]; then
                echo "gx/build.sh: --out requires a path argument" >&2
                exit 1
            fi
            if [[ -z "$2" ]]; then
                echo "gx/build.sh: --out requires a non-empty path argument" >&2
                exit 1
            fi
            out_path="$2"
            shift 2
            ;;
        --out=*)
            out_path="${1#--out=}"
            if [[ -z "${out_path}" ]]; then
                echo "gx/build.sh: --out= requires a non-empty path argument" >&2
                exit 1
            fi
            shift
            ;;
        *)
            echo "gx/build.sh: unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

gx_version="$("${script_dir}/version.sh")"

echo "gx/build.sh: building gx ${gx_version}" >&2

# Ask cargo where it put the artifact rather than guessing `target/release/`:
# CARGO_TARGET_DIR, `build.target-dir` in .cargo/config.toml, or `--target`
# all move the output, and guessing would silently copy a stale binary.
# `--message-format=json-render-diagnostics` keeps human-readable errors and
# warnings streaming on stderr while the machine-readable messages (which
# carry `.executable`) go to stdout for the parser below.
built_bin="$(
    cd -- "${repo_root}" &&
    GROK_VERSION="${gx_version}" cargo build \
        -p xai-grok-pager-bin --release \
        --message-format=json-render-diagnostics \
        | GX_BIN_TARGET="${gx_bin_target}" python3 -c '
import json, os, sys

want = os.environ["GX_BIN_TARGET"]
found = None
for line in sys.stdin:
    line = line.strip()
    if not line or not line.startswith("{"):
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if msg.get("reason") != "compiler-artifact":
        continue
    if msg.get("target", {}).get("name") != want:
        continue
    exe = msg.get("executable")
    if exe:
        found = exe
if found:
    print(found)
'
)"

if [[ -z "${built_bin}" ]]; then
    echo "gx/build.sh: cargo reported no executable artifact for target '${gx_bin_target}'" >&2
    exit 1
fi

if [[ ! -f "${built_bin}" ]]; then
    echo "gx/build.sh: cargo-reported artifact missing: ${built_bin}" >&2
    exit 1
fi

# gx lands next to the real artifact, whatever target dir cargo used.
gx_bin="$(dirname -- "${built_bin}")/gx"

cp -- "${built_bin}" "${gx_bin}"
chmod +x "${gx_bin}"

if [[ -n "${out_path}" ]]; then
    out_dir="$(dirname -- "${out_path}")"
    mkdir -p -- "${out_dir}"
    cp -- "${gx_bin}" "${out_path}"
    chmod +x "${out_path}"
    echo "gx/build.sh: installed ${out_path}" >&2
fi

echo "${gx_bin}"
echo "gx/build.sh: artifact ${built_bin}" >&2
echo "gx/build.sh: gx ${gx_bin}" >&2
echo "gx/build.sh: version ${gx_version}" >&2
