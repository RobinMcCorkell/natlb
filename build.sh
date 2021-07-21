#!/bin/bash

set -euxo pipefail

cross build --release --target aarch64-unknown-linux-musl

c=$(buildah from --arch arm64 gcr.io/distroless/static)
buildah copy $c target/aarch64-unknown-linux-musl/release/natlb /
buildah config --cmd /natlb $c
buildah commit --rm $c registry.gitlab.com/robinmccorkell/natlb
buildah push registry.gitlab.com/robinmccorkell/natlb
