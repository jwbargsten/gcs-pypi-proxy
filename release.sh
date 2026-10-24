#!/usr/bin/env bash

set -o errexit
set -o pipefail
set -o nounset

TAG=${1:-}
BRANCH=main

die() {
  echo "$*" >&2
  exit 1
}

confirm() {
  # https://stackoverflow.com/questions/1885525/how-do-i-prompt-a-user-for-confirmation-in-bash-script
  read -p "${1:-Are you sure?} (y/n) " -n 1 -r
  echo
  [[ $REPLY =~ ^[Yy]$ ]]
}

confirm_or_exit() {
  if ! confirm "$@"; then
    exit 0
  fi
}

[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: $0 vX.Y.Z"
VERSION=${TAG#v}

# maturin takes the wheel version from Cargo.toml, not from the tag, so the two have to agree
CARGO_VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[[ "$CARGO_VERSION" == "$VERSION" ]] ||
  die "Cargo.toml has version $CARGO_VERSION, not $VERSION: bump it (and Cargo.lock), commit and push first"

[[ -z "$(git status --porcelain)" ]] || die "working tree is not clean"
[[ "$(git branch --show-current)" == "$BRANCH" ]] || die "not on $BRANCH"
git fetch --quiet origin "$BRANCH"
[[ "$(git rev-parse HEAD)" == "$(git rev-parse "origin/$BRANCH")" ]] || die "$BRANCH is not in sync with origin/$BRANCH"
if git rev-parse --quiet --verify "refs/tags/$TAG" >/dev/null; then
  die "tag $TAG already exists locally"
fi
rc=0
git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null || rc=$?
[[ $rc -eq 2 ]] || die "tag $TAG already exists on origin, or origin is unreachable"

confirm_or_exit "Release $TAG from $(git rev-parse --short HEAD)?"

# The version comes from Cargo.toml, so unlike with setuptools_scm the tag is not needed to build.
make wheel
compgen -G "dist/gcs_pypi_proxy-$VERSION-*.whl" >/dev/null || die "no $VERSION wheels in dist/"
ls -1 dist/*.whl
confirm_or_exit "Publish $TAG to PyPI?"

# Tag first: a pushed tag without an upload is easy to fix, an upload without a tag is not.
git tag -a "$TAG" -m "$TAG"
git push origin "$TAG" || die "pushing tag $TAG failed, remove it with 'git tag -d $TAG' and retry"

make publish || die "upload failed, tag $TAG is already pushed: fix the cause and retry with 'make publish'"

# if you ever need to delete a tag: git tag -d v0.0.1 && git push origin :refs/tags/v0.0.1
