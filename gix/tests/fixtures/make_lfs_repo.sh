#!/usr/bin/env bash
set -eu -o pipefail

# A repository whose `*.bin` files are LFS pointers, written as text so `git-lfs` isn't needed.
# The objects they point to are in no LFS store, and there is no remote.
#
# * The first commit adds `first.bin` with pointer A and `small.txt`, which isn't a pointer.
# * The second commit adds `dir/copy.bin`, the same blob as `first.bin`, and `dir/second.bin` with pointer B.
#
# The nested repository `lfsconfig` has a remote and an `.lfsconfig` file in its worktree naming the LFS server.
pointer() {
  printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize %s\n' "$1" "$2"
}
A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

git init -q
echo '*.bin filter=lfs diff=lfs merge=lfs -text' > .gitattributes
pointer $A 10 > first.bin
echo 'not a pointer' > small.txt
git add .gitattributes first.bin small.txt
git commit -qm "first"

mkdir dir
pointer $A 10 > dir/copy.bin
pointer $B 20 > dir/second.bin
git add dir
git commit -qm "second"

git init -q lfsconfig
(cd lfsconfig
  git remote add origin https://example.com/org/repo.git
  printf '[lfs]\n\turl = https://lfs.example.com/from-lfsconfig\n' > .lfsconfig
)
