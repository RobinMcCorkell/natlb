#!/bin/bash

set -euxo pipefail

cross build --release --target aarch64-unknown-linux-musl
version=$(
    cargo metadata --format-version 1 \
        | jq -r '.packages[] | select(.name=="natlb") | .version'
)

c=$(buildah from --arch arm64 gcr.io/distroless/static)
buildah copy $c target/aarch64-unknown-linux-musl/release/natlb /
buildah config --cmd /natlb $c
buildah commit --rm $c registry.gitlab.com/robinmccorkell/natlb:${version}
buildah push registry.gitlab.com/robinmccorkell/natlb:${version}
