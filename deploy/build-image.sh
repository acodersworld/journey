#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "Usage: deploy/build-image.sh <site|storage> [IMAGE_TAG]" >&2
    echo "       deploy/build-image.sh all" >&2
}

if (($# < 1 || $# > 2)); then
    usage
    exit 2
fi

mode=$1
case "$mode" in
    site)
        dockerfile=apps/site/Dockerfile
        default_tag=journal-site:latest
        ;;
    storage)
        dockerfile=apps/storage/Dockerfile
        default_tag=storage:latest
        ;;
    all)
        if (($# != 1)); then
            echo "the all mode builds both images with their default tags" >&2
            usage
            exit 2
        fi
        ;;
    *)
        usage
        exit 2
        ;;
esac

if [[ "$mode" != all ]]; then
    image_tag=${2:-$default_tag}
    if [[ -z "$image_tag" ]]; then
        echo "image tag must not be empty" >&2
        exit 2
    fi
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
commit=$(git -C "$repo_root" rev-parse --verify HEAD)
temp_parent=${TMPDIR:-/tmp}
temp_dir=$(mktemp -d "${temp_parent%/}/journey-build.XXXXXX")
cleanup() {
    rm -rf -- "$temp_dir"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

clone_dir=$temp_dir/source
git clone --no-local --depth 1 --no-tags --revision "$commit" "$repo_root" "$clone_dir"

clone_commit=$(git -C "$clone_dir" rev-parse --verify HEAD)
if [[ "$clone_commit" != "$commit" ]]; then
    echo "shallow clone has commit $clone_commit; expected $commit" >&2
    exit 1
fi

commit_count=$(git -C "$clone_dir" rev-list --count HEAD)
if [[ "$commit_count" != 1 ]]; then
    echo "shallow clone contains $commit_count commits; expected 1" >&2
    exit 1
fi

if [[ -n "$(git -C "$clone_dir" status --porcelain --untracked-files=all)" ]]; then
    echo "shallow clone working tree is not clean" >&2
    exit 1
fi

build_image() {
    docker build \
        --build-arg "BUILD_COMMIT=$commit" \
        --tag "$2" \
        --file "$clone_dir/$1" \
        "$clone_dir"
}

if [[ "$mode" == all ]]; then
    build_image apps/site/Dockerfile journal-site:latest
    build_image apps/storage/Dockerfile storage:latest
else
    build_image "$dockerfile" "$image_tag"
fi
