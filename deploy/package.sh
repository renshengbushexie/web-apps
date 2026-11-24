#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if [[ -n $(git status --porcelain --untracked-files=normal) ]]; then
    echo "Build from a clean checkout to identify the release by its commit" >&2
    exit 1
fi
version=$(git rev-parse HEAD)
image=rust-knowledge-build:$version
docker build --platform linux/amd64 --tag "$image" .
mkdir -p dist
stage=$(mktemp -d)
cid=$(docker create --platform linux/amd64 "$image")
cleanup() {
    docker rm "$cid" >/dev/null
    rm -f -- "$stage/kb-api" "$stage/kb-sync" "$stage/install.sh" "$stage/kb-api.service" \
        "$stage/kb-sync.service" "$stage/nginx-locations.conf" "$stage/VERSION" "$stage/docs/openapi.json"
    rmdir -- "$stage/docs" "$stage"
}
trap cleanup EXIT
mkdir "$stage/docs"
for binary in kb-api kb-sync; do
    docker cp "$cid:/usr/local/bin/$binary" "$stage/$binary"
done
cp deploy/install.sh deploy/kb-api.service deploy/kb-sync.service deploy/nginx-locations.conf "$stage/"
cp docs/openapi.json "$stage/docs/"
printf '%s\n' "$version" > "$stage/VERSION"
archive=rust-knowledge-linux-amd64.tar.gz
tar -C "$stage" -czf "dist/$archive" VERSION install.sh kb-api kb-sync kb-api.service kb-sync.service nginx-locations.conf docs/openapi.json
(cd dist && sha256sum "$archive" > SHA256SUMS)
