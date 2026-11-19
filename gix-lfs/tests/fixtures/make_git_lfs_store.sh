#!/usr/bin/env bash
set -eu -o pipefail

# Let `git-lfs` clean a file into its store so tests can compare the pointer it writes and the location of
# the object with what `gix-lfs` produces for the same content.
# Without `git-lfs`, only the content is written along with the `git-lfs-unavailable` marker.
printf 'content stored by git-lfs\n%.0s' {1..100} > content

if ! git lfs version >/dev/null 2>&1; then
  touch git-lfs-unavailable
  exit 0
fi

git init -q repo
(cd repo
  git lfs install --local
  git lfs track '*.bin'
  cp ../content file.bin
  git add .gitattributes file.bin
  git commit -qm "add a file tracked by git-lfs"
  git cat-file blob :file.bin > ../pointer
)
