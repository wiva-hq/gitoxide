#!/usr/bin/env bash
set -eu -o pipefail

# `client` pushes to bare repositories that all start out with the same state:
#   - `main` at commit `two`, `other` at commit `one`
#   - the annotated tag `v1` and the lightweight tag `light`, both at `one`
#
# In `client`, `main` is at `three`, so pushing it is a fast-forward, while `rewritten` branches off `one`
# and can't replace `main` without force. The lightweight tag `v2` at `three` exists only in `client`.
# Its remote-tracking references match the servers, but `remote.origin.url` is removed as tests reach
# the servers through different URLs.
#
# - `server.git` accepts everything.
# - `hooked.git` has a `pre-receive` hook that declines everything with the message 'main is protected'.
# - `no-deletes.git` refuses deletions with `receive.denyDeletes`, which fails atomic pushes that delete.
#
# `bin/ssh` stands in for `ssh` and runs the command meant for the remote host locally.

git init -q --bare server.git
git init -q client
(
  cd client
  echo one >file
  git add file
  git commit -q -m one
  git tag -a -m "first release" v1
  git tag light
  git branch other
  echo two >>file
  git commit -q -am two
  git remote add origin ../server.git
  git push -q origin main other v1 light

  git checkout -q -b rewritten other
  echo rewritten >>file
  git commit -q -am rewritten
  git checkout -q main
  echo three >>file
  git commit -q -am three
  git tag v2
  git config --unset remote.origin.url
)

git clone -q --bare server.git hooked.git
cat >hooked.git/hooks/pre-receive <<'HOOK'
#!/bin/sh
echo "main is protected" >&2
exit 1
HOOK
chmod +x hooked.git/hooks/pre-receive

git clone -q --bare server.git no-deletes.git
git -C no-deletes.git config receive.denyDeletes true

mkdir bin
cat >bin/ssh <<'SSH'
#!/bin/sh
# Skip options and the host, then run the remaining arguments as command like `sshd` would.
while [ "$1" = "-o" ]; do shift 2; done
case "$1" in -p*) shift;; esac
shift
PATH="$(git --exec-path):$PATH" exec sh -c "$*"
SSH
chmod +x bin/ssh
