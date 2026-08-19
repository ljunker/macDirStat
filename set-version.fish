#!/usr/bin/env fish

set VERSION $argv[1]

if test -z "$VERSION"
    echo "Usage: ./set-version.fish v0.3.3"
    exit 1
end

set CARGO_VERSION (string replace -r '^v' '' $VERSION)

echo $VERSION > VERSION

sed -i '' -E "1,10 s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$CARGO_VERSION\"/" Cargo.toml
