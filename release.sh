#!/bin/bash
# Usage: ./release.sh 1.1.0
set -e

VERSION=$1
if [ -z "$VERSION" ]; then
  echo "Usage: ./release.sh <version>"
  exit 1
fi

# bump version in Cargo.toml
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml

# commit, tag, push — triggers GitHub Actions
git add .
git commit -m "release: v$VERSION"
git tag "v$VERSION"
git push && git push --tags

echo "Release v$VERSION pushed — GitHub Actions is now building all platform binaries."
echo "Track progress at: https://github.com/TheOriUHD/diskvis/actions"
