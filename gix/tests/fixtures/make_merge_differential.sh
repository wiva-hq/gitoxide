#!/usr/bin/env bash
set -eu -o pipefail

# A corpus of merge cases recorded with real Git for comparison with `gix`.
#
# Histories are built from plumbing commands only, so symlinks, gitlinks, CRLF bytes and
# non-UTF-8 paths never touch the filesystem and behave alike on every platform.
# Each case is merged by `git merge-tree --write-tree` in both directions and with every
# variant it lists. `cases` records `<name> <direction> <variant> <ours> <theirs>`, and
# `results/<variant>` holds the `--stdin -z --messages` output of that variant's merges in order.
# Variants map to `gix` options as follows:
#   default      nothing changed
#   ours/theirs  `-X ours`/`-X theirs`, or `FileFavor::Ours`/`FileFavor::Theirs`
#   union        `* merge=union` appended to `info/attributes`, or `FileFavor::Union`
#   diff3/zdiff3 `merge.conflictStyle`
#   renormalize  `merge.renormalize=true`
#   directory-renames-true/-false  `merge.directoryRenames`
# Single blobs are merged with `git merge-file`, recorded in `blob-cases` as
# `<name> <variant> <status> <base> <ours> <theirs>` with output in `results/blob-<name>.<variant>`.

git init -q
git config merge.keep.driver true
attributes=.git/info/attributes
mkdir -p .git/info results blobs queue
cat >"$attributes" <<'EOF'
*.union merge=union
*.keep merge=keep
*.merge-binary merge=binary
*.unset-merge -merge
*.unknown-driver merge=no-such-driver
*.auto text=auto
*.eol-crlf text eol=crlf
EOF

blob () { git hash-object -w --stdin; }
lines () { printf '%s\n' "$@" | blob; }
crlf_lines () {
  local line
  for line in "$@"; do printf '%s\r\n' "$line"; done | blob
}
# Distinct lines keep files recognizable as renames after small edits.
counted () {
  local i
  for ((i = 1; i <= $2; i++)); do printf '%s line %d\n' "$1" "$i"; done | blob
}
# Replace line `$2` of blob `$1` with `$3`.
edit () { git cat-file blob "$1" | awk -v n="$2" -v r="$3" 'NR == n { print r; next } { print }' | blob; }

# `tree <mode> <id> <path>...` writes a tree from leaf entries.
tree () {
  local index=.git/corpus-index
  rm -f "$index"
  while test "$#" -gt 0; do
    printf '%s %s\t%s\0' "$1" "$2" "$3"
    shift 3
  done | GIT_INDEX_FILE=$index git update-index -z --add --index-info
  GIT_INDEX_FILE=$index git write-tree
}

# `history <name>` commits the BASE, OURS and THEIRS entry arrays as a fork from one base.
history () {
  local name=$1 base_commit_id
  base_commit_id=$(git commit-tree "$(tree ${BASE[@]+"${BASE[@]}"})" -m "$name base")
  git update-ref "refs/heads/$name-base" "$base_commit_id"
  git update-ref "refs/heads/$name-A" "$(git commit-tree "$(tree ${OURS[@]+"${OURS[@]}"})" -p "$base_commit_id" -m "$name A")"
  git update-ref "refs/heads/$name-B" "$(git commit-tree "$(tree ${THEIRS[@]+"${THEIRS[@]}"})" -p "$base_commit_id" -m "$name B")"
}

# `record <name> <variant>...` queues merges of `<name>-A` and `<name>-B` in both directions.
record () {
  local name=$1 direction ours theirs variant
  shift
  for direction in A-B B-A; do
    ours=$name-A theirs=$name-B
    if test "$direction" = B-A; then ours=$name-B theirs=$name-A; fi
    for variant in "$@"; do
      echo "$ours $theirs" >>"queue/$variant"
      echo "$name $direction $variant $ours $theirs" >>cases
    done
  done
}

# Merge all queued pairs of each variant with one Git invocation.
run_queued_merges () {
  local queue variant
  for queue in queue/*; do
    variant=${queue#queue/}
    local -a config=() options=()
    case "$variant" in
      default|union) ;;
      ours|theirs) options=(-X "$variant") ;;
      diff3|zdiff3) config=(-c "merge.conflictStyle=$variant") ;;
      renormalize) config=(-c merge.renormalize=true) ;;
      directory-renames-true) config=(-c merge.directoryRenames=true) ;;
      directory-renames-false) config=(-c merge.directoryRenames=false) ;;
      *) echo "unknown variant: $variant" >&2; return 1 ;;
    esac
    if test "$variant" = union; then
      cp "$attributes" "$attributes.saved"
      echo '* merge=union' >>"$attributes"
    fi
    git ${config[@]+"${config[@]}"} merge-tree --stdin -z --write-tree --messages --allow-unrelated-histories \
      ${options[@]+"${options[@]}"} <"$queue" >"results/$variant"
    if test "$variant" = union; then
      mv "$attributes.saved" "$attributes"
    fi
  done
}

# `blob_case <name> <base> <ours> <theirs>` merges three blobs with `git merge-file`.
blob_case () {
  local name=$1 variant status
  git cat-file blob "$2" >blobs/base
  git cat-file blob "$3" >blobs/ours
  git cat-file blob "$4" >blobs/theirs
  for variant in default ours theirs union diff3 zdiff3; do
    local -a options=()
    if test "$variant" != default; then options=("--$variant"); fi
    status=0
    git merge-file -p -L ours -L base -L theirs ${options[@]+"${options[@]}"} \
      blobs/ours blobs/base blobs/theirs >"results/blob-$name.$variant" || status=$?
    test "$status" -lt 128
    echo "$name $variant $status $2 $3 $4" >>blob-cases
  done
}

: >cases
: >blob-cases
std="default ours theirs union"

text=$(counted text 10)
other=$(counted other 10)
third=$(counted third 10)
gitlink_1=$(git commit-tree "$(tree)" -m gitlink-1)
gitlink_2=$(git commit-tree "$(tree)" -m gitlink-2)
gitlink_3=$(git commit-tree "$(tree)" -m gitlink-3)

### Content merges of one file.

BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 2 ours)" f) THEIRS=(100644 "$(edit "$text" 8 theirs)" f)
history content-clean && record content-clean $std

ours_blob=$(edit "$text" 5 ours) theirs_blob=$(edit "$text" 5 theirs)
BASE=(100644 "$text" f) OURS=(100644 "$ours_blob" f) THEIRS=(100644 "$theirs_blob" f)
history content-conflict && record content-conflict $std diff3 zdiff3
blob_case content-conflict "$text" "$ours_blob" "$theirs_blob"

ours_blob=$(edit "$(edit "$text" 4 ours)" 6 ours) theirs_blob=$(edit "$text" 5 theirs)
BASE=(100644 "$text" f) OURS=(100644 "$ours_blob" f) THEIRS=(100644 "$theirs_blob" f)
history content-adjacent && record content-adjacent $std diff3 zdiff3
blob_case content-adjacent "$text" "$ours_blob" "$theirs_blob"

ours_blob=$(edit "$(edit "$text" 3 same)" 7 ours) theirs_blob=$(edit "$(edit "$text" 3 same)" 7 theirs)
BASE=(100644 "$text" f) OURS=(100644 "$ours_blob" f) THEIRS=(100644 "$theirs_blob" f)
history content-partly-identical && record content-partly-identical $std zdiff3
blob_case content-partly-identical "$text" "$ours_blob" "$theirs_blob"

BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 3 same)" f) THEIRS=(100644 "$(edit "$text" 3 same)" f)
history content-identical && record content-identical $std

### Additions and deletions.

BASE=(100644 "$other" unrelated) OURS=(100644 "$other" unrelated 100644 "$text" new) THEIRS=(100644 "$other" unrelated 100644 "$text" new)
history add-add-same && record add-add-same $std

ours_blob=$(edit "$text" 3 ours) theirs_blob=$(edit "$text" 3 theirs)
BASE=(100644 "$other" unrelated) OURS=(100644 "$other" unrelated 100644 "$ours_blob" new) THEIRS=(100644 "$other" unrelated 100644 "$theirs_blob" new)
history add-add-different && record add-add-different $std diff3
blob_case add-add-different "$(printf '' | blob)" "$ours_blob" "$theirs_blob"

BASE=(100644 "$other" unrelated) OURS=(100644 "$other" unrelated 100644 "$text" new) THEIRS=(100644 "$other" unrelated 100755 "$text" new)
history add-add-mode && record add-add-mode $std

BASE=(100644 "$other" unrelated) OURS=(100644 "$other" unrelated 100644 "$text" new) THEIRS=(100644 "$other" unrelated 120000 "$(printf target | blob)" new)
history add-add-file-symlink && record add-add-file-symlink $std

BASE=(100644 "$text" f 100644 "$other" g) OURS=(100644 "$(edit "$text" 2 ours)" f 100644 "$other" g) THEIRS=(100644 "$other" g)
history modify-delete && record modify-delete $std

BASE=(100644 "$text" f 100644 "$other" g) OURS=(100644 "$other" g) THEIRS=(100644 "$other" g)
history delete-delete && record delete-delete $std

BASE=(100644 "$text" f 100644 "$other" g) OURS=(100644 "$text" f) THEIRS=(100644 "$(edit "$text" 2 theirs)" f)
history delete-other-modify-same && record delete-other-modify-same $std

### Renames.

BASE=(100644 "$text" f) OURS=(100644 "$text" g) THEIRS=(100644 "$(edit "$text" 3 theirs)" f)
history rename-edit && record rename-edit $std

BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 2 ours)" g) THEIRS=(100644 "$(edit "$text" 8 theirs)" f)
history rename-edit-both && record rename-edit-both $std

BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 5 ours)" g) THEIRS=(100644 "$(edit "$text" 5 theirs)" f)
history rename-edit-conflict && record rename-edit-conflict $std diff3

BASE=(100644 "$text" f) OURS=(100644 "$text" sub/g) THEIRS=(100755 "$text" f)
history rename-mode && record rename-mode $std

BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 2 ours)" g) THEIRS=(100644 "$(edit "$text" 9 theirs)" g)
history rename-rename-same && record rename-rename-same $std

BASE=(100644 "$text" f) OURS=(100644 "$text" g) THEIRS=(100644 "$text" h)
history rename-rename-divergent && record rename-rename-divergent $std

BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 2 ours)" g) THEIRS=(100644 "$(edit "$text" 2 theirs)" h)
history rename-rename-divergent-conflict && record rename-rename-divergent-conflict $std

BASE=(100644 "$text" f 100644 "$other" e) OURS=(100644 "$text" x 100644 "$other" e) THEIRS=(100644 "$text" f 100644 "$other" x)
history rename-rename-two-to-one && record rename-rename-two-to-one $std

BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 4 ours)" g) THEIRS=()
history rename-delete && record rename-delete $std

BASE=(100644 "$text" f) OURS=(100644 "$text" g) THEIRS=(100644 "$text" f 100644 "$third" g)
history rename-add && record rename-add $std

BASE=(100644 "$text" f 100644 "$other" g) OURS=(100644 "$text" g 100644 "$other" f) THEIRS=(100644 "$(edit "$text" 2 theirs)" f 100644 "$(edit "$other" 9 theirs)" g)
history rename-swap && record rename-swap $std

# The base of a file renamed alike on both sides is labelled with the destination.
BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 5 ours)" g) THEIRS=(100644 "$(edit "$text" 5 theirs)" g)
history rename-rename-same-conflict && record rename-rename-same-conflict default diff3

# `d/c` and `n` are identical, but only `d/c` changes on the other side, so Git doesn't consider
# `n` as source of the inexact rename to `d/n` even though its name matches.
BASE=(100644 "$text" d/c 100644 "$text" n) OURS=(100644 "$text" n) THEIRS=(100644 "$(edit "$text" 10 theirs)" d/n)
history rename-relevant-source && record rename-relevant-source $std

# B adds the destination of A's rename with the renamed content, which Git checks before merging the rename.
# Git keeps B's entry if B is *ours*, and reports conflicting content as clean merge if A is *ours*.
BASE=(100644 "$text" f) OURS=(100644 "$text" g) THEIRS=(100644 "$(edit "$text" 5 theirs)" f 100644 "$text" g)
history rename-add-identical && record rename-add-identical $std
BASE=(100644 "$text" f) OURS=(100644 "$(edit "$text" 5 ours)" g)
THEIRS=(100644 "$(edit "$text" 5 theirs)" f 100644 "$(edit "$text" 5 ours)" g)
history rename-add-identical-conflict && record rename-add-identical-conflict default

### Directory renames.

BASE=(100644 "$text" d/a 100644 "$other" d/b 100644 "$third" d/c)
OURS=(100644 "$text" e/a 100644 "$other" e/b 100644 "$third" e/c)
THEIRS=(100644 "$text" d/a 100644 "$other" d/b 100644 "$third" d/c 100644 "$(counted new 10)" d/new)
history directory-rename-addition && record directory-rename-addition $std directory-renames-true directory-renames-false

BASE=(100644 "$text" d/a 100644 "$other" d/b 100644 "$third" d/c)
OURS=(100644 "$text" e/a 100644 "$other" e/b 100644 "$third" e/c)
THEIRS=(100644 "$text" d/a 100644 "$(edit "$other" 4 theirs)" d/b 100644 "$third" d/c)
history directory-rename-edit && record directory-rename-edit $std directory-renames-true

BASE=(100644 "$text" d/a 100644 "$other" d/b)
OURS=(100644 "$text" x/a 100644 "$other" y/b)
THEIRS=(100644 "$text" d/a 100644 "$other" d/b 100644 "$third" d/new)
history directory-rename-split && record directory-rename-split $std directory-renames-true

BASE=(100644 "$text" d/a 100644 "$other" d/b)
OURS=(100644 "$text" e/a 100644 "$other" e/b)
THEIRS=(100644 "$text" d/a 100644 "$other" d/b 100644 "$third" d/sub/new)
history directory-rename-nested-addition && record directory-rename-nested-addition $std directory-renames-true

# A's directory rename `d/e` -> `x/y` moves B's rename of `d/e/a` below A's destination `x/y/a`.
# `d/b` keeps `d` relevant for directory renames. Git keeps A's file in the tree and B's destination only in the index.
BASE=(100644 "$text" d/e/a 100644 "$other" x/y/z)
OURS=(100644 "$text" x/y/a 100644 "$other" x/y/z)
THEIRS=(100644 "$text" d/e/a/n 100644 "$other" x/y/z 100644 "$third" d/b)
history rename-rename-nested && record rename-rename-nested default

### Type changes.

BASE=(100644 "$text" p 100644 "$other" q)
OURS=(100644 "$text" p/inner 100644 "$other" q)
THEIRS=(100644 "$(edit "$text" 2 theirs)" p 100644 "$other" q)
history file-to-directory && record file-to-directory $std

BASE=(100644 "$text" d/a 100644 "$other" d/b)
OURS=(100644 "$third" d)
THEIRS=(100644 "$(edit "$text" 2 theirs)" d/a 100644 "$other" d/b)
history directory-to-file && record directory-to-file $std

BASE=(100644 "$text" f)
OURS=(120000 "$(printf g | blob)" f)
THEIRS=(100644 "$(edit "$text" 2 theirs)" f)
history file-to-symlink && record file-to-symlink $std

BASE=(120000 "$(printf a | blob)" l) OURS=(120000 "$(printf b | blob)" l) THEIRS=(120000 "$(printf c | blob)" l)
history symlink-retarget && record symlink-retarget $std

BASE=(100644 "$text" f) OURS=(160000 "$gitlink_1" f) THEIRS=(100644 "$(edit "$text" 2 theirs)" f)
history file-to-gitlink && record file-to-gitlink $std

### Modes.

BASE=(100644 "$text" f) OURS=(100755 "$text" f) THEIRS=(100644 "$(edit "$text" 2 theirs)" f)
history mode-and-content && record mode-and-content $std

BASE=(100644 "$text" f) OURS=(100755 "$text" f) THEIRS=(100755 "$(edit "$text" 2 theirs)" f)
history mode-both && record mode-both $std

BASE=(100755 "$text" f) OURS=(100644 "$(edit "$text" 2 ours)" f) THEIRS=(100755 "$(edit "$text" 2 theirs)" f)
history mode-and-conflict && record mode-and-conflict $std

### Binary, empty and incomplete-line content.

binary=$(printf 'binary\0base\0data\n' | blob)
binary_ours=$(printf 'binary\0ours\0data\n' | blob)
binary_theirs=$(printf 'binary\0theirs\0data\n' | blob)
BASE=(100644 "$binary" b) OURS=(100644 "$binary_ours" b) THEIRS=(100644 "$binary_theirs" b)
history binary-conflict && record binary-conflict $std

BASE=(100644 "$binary" b 100644 "$text" f) OURS=(100644 "$binary_ours" b 100644 "$text" f) THEIRS=(100644 "$binary" b 100644 "$(edit "$text" 2 theirs)" f)
history binary-one-side && record binary-one-side $std

BASE=(100644 "$binary" b) OURS=(100644 "$text" b) THEIRS=(100644 "$binary_theirs" b)
history binary-to-text && record binary-to-text $std

empty=$(printf '' | blob)
ours_blob=$(lines ours one) theirs_blob=$(lines theirs one)
BASE=(100644 "$empty" e) OURS=(100644 "$ours_blob" e) THEIRS=(100644 "$theirs_blob" e)
history empty-base-conflict && record empty-base-conflict $std diff3
blob_case empty-base-conflict "$empty" "$ours_blob" "$theirs_blob"

BASE=(100644 "$text" f) OURS=(100644 "$empty" f) THEIRS=(100644 "$(edit "$text" 3 theirs)" f)
history truncate-vs-edit && record truncate-vs-edit $std
blob_case truncate-vs-edit "$text" "$empty" "$(edit "$text" 3 theirs)"

BASE=(100644 "$other" unrelated) OURS=(100644 "$other" unrelated 100644 "$empty" e) THEIRS=(100644 "$other" unrelated 100644 "$empty" e)
history add-add-empty && record add-add-empty $std

BASE=(100644 "$empty" e 100644 "$other" g) OURS=(100644 "$empty" renamed 100644 "$other" g) THEIRS=(100644 "$empty" e 100644 "$(edit "$other" 2 theirs)" g)
history rename-empty && record rename-empty $std

no_eol=$(printf 'a\nb\nc' | blob)
ours_blob=$(printf 'A\nb\nc' | blob) theirs_blob=$(printf 'a\nb\nc\nd' | blob)
BASE=(100644 "$no_eol" n) OURS=(100644 "$ours_blob" n) THEIRS=(100644 "$theirs_blob" n)
history missing-final-newline && record missing-final-newline $std
blob_case missing-final-newline "$no_eol" "$ours_blob" "$theirs_blob"

ours_blob=$(printf 'a\nb\nours' | blob) theirs_blob=$(printf 'a\nb\ntheirs' | blob)
BASE=(100644 "$no_eol" n) OURS=(100644 "$ours_blob" n) THEIRS=(100644 "$theirs_blob" n)
history missing-final-newline-conflict && record missing-final-newline-conflict $std diff3
blob_case missing-final-newline-conflict "$no_eol" "$ours_blob" "$theirs_blob"

### Line endings and renormalization.

crlf=$(crlf_lines one two three four five six seven eight)
ours_blob=$(crlf_lines one ours three four five six seven eight)
theirs_blob=$(crlf_lines one two three four five six theirs eight)
BASE=(100644 "$crlf" c) OURS=(100644 "$ours_blob" c) THEIRS=(100644 "$theirs_blob" c)
history crlf-clean && record crlf-clean $std
blob_case crlf-clean "$crlf" "$ours_blob" "$theirs_blob"

ours_blob=$(crlf_lines one two three ours five six seven eight)
theirs_blob=$(crlf_lines one two three theirs five six seven eight)
BASE=(100644 "$crlf" c) OURS=(100644 "$ours_blob" c) THEIRS=(100644 "$theirs_blob" c)
history crlf-conflict && record crlf-conflict $std diff3 zdiff3
blob_case crlf-conflict "$crlf" "$ours_blob" "$theirs_blob"

mixed=$(printf 'one\r\ntwo\nthree\r\nfour\nfive\r\n' | blob)
ours_blob=$(printf 'one\r\ntwo\nours\r\nfour\nfive\r\n' | blob)
theirs_blob=$(printf 'one\r\ntwo\ntheirs\nfour\nfive\r\n' | blob)
BASE=(100644 "$mixed" m) OURS=(100644 "$ours_blob" m) THEIRS=(100644 "$theirs_blob" m)
history mixed-eol-conflict && record mixed-eol-conflict $std diff3
blob_case mixed-eol-conflict "$mixed" "$ours_blob" "$theirs_blob"

lf=$(lines one two three four five six seven eight)
BASE=(100644 "$lf" x.auto)
OURS=(100644 "$(crlf_lines one two three four five six seven eight)" x.auto)
THEIRS=(100644 "$(lines one two three four five six theirs eight)" x.auto)
history renormalize-eol-switch && record renormalize-eol-switch $std renormalize

BASE=(100644 "$lf" x.auto)
OURS=(100644 "$(crlf_lines one ours three four five six seven eight)" x.auto)
THEIRS=(100644 "$(lines one two three four five six theirs eight)" x.auto)
history renormalize-eol-switch-and-edit && record renormalize-eol-switch-and-edit $std renormalize

BASE=(100644 "$lf" y.eol-crlf)
OURS=(100644 "$(crlf_lines one two ours four five six seven eight)" y.eol-crlf)
THEIRS=(100644 "$(lines one two three four five six theirs eight)" y.eol-crlf)
history renormalize-eol-attribute && record renormalize-eol-attribute $std renormalize

BASE=(100644 "$lf" x.auto)
OURS=(100644 "$(crlf_lines one two three four five six seven eight)" x.auto)
THEIRS=()
history renormalize-modify-delete && record renormalize-modify-delete $std renormalize

### Merge attributes, never combined with the union variant which overrides them.

for path in u.union k.keep b.merge-binary s.unset-merge z.unknown-driver; do
  name=attribute-${path#*.}
  BASE=(100644 "$text" "$path") OURS=(100644 "$(edit "$text" 5 ours)" "$path") THEIRS=(100644 "$(edit "$text" 5 theirs)" "$path")
  history "$name" && record "$name" default ours theirs diff3
  name=$name-clean
  BASE=(100644 "$text" "$path") OURS=(100644 "$(edit "$text" 2 ours)" "$path") THEIRS=(100644 "$(edit "$text" 8 theirs)" "$path")
  history "$name" && record "$name" default ours theirs
done

### Large files, unusual paths and deep trees.

large=$(seq 1 200000 | blob)
large_ours=$(seq 1 200000 | awk 'NR == 10 || NR == 100000 { print "ours"; next } { print }' | blob)
large_theirs=$(seq 1 200000 | awk 'NR == 199990 { print "theirs"; next } { print }' | blob)
large_conflict=$(seq 1 200000 | awk 'NR == 100000 { print "theirs"; next } { print }' | blob)
BASE=(100644 "$large" large) OURS=(100644 "$large_ours" large) THEIRS=(100644 "$large_theirs" large)
history large-clean && record large-clean default ours
BASE=(100644 "$large" large) OURS=(100644 "$large_ours" large) THEIRS=(100644 "$large_conflict" large)
history large-conflict && record large-conflict default ours theirs union

latin1=$'caf\xe9'
invalid=$'\xff\xfe-name'
BASE=(100644 "$text" "$latin1" 100644 "$other" "d/$invalid")
OURS=(100644 "$(edit "$text" 2 ours)" "$latin1" 100644 "$other" "e/$invalid")
THEIRS=(100644 "$(edit "$text" 8 theirs)" "$latin1" 100644 "$(edit "$other" 3 theirs)" "d/$invalid")
history non-utf8-paths && record non-utf8-paths $std
BASE=(100644 "$text" "$latin1") OURS=(100644 "$(edit "$text" 5 ours)" "$latin1") THEIRS=(100644 "$(edit "$text" 5 theirs)" "$latin1")
history non-utf8-path-conflict && record non-utf8-path-conflict $std

deep=$(printf 'level%d/' $(seq 1 40))
BASE=(100644 "$text" "${deep}file" 100644 "$other" "${deep}other")
OURS=(100644 "$(edit "$text" 2 ours)" "${deep}file" 100644 "$other" "moved/${deep}other")
THEIRS=(100644 "$(edit "$text" 8 theirs)" "${deep}file" 100644 "$(edit "$other" 5 theirs)" "${deep}other" 100644 "$third" "${deep}new")
history deep-tree && record deep-tree $std directory-renames-true

### Gitlinks whose submodule repository is unavailable.

BASE=(160000 "$gitlink_1" s) OURS=(160000 "$gitlink_2" s) THEIRS=(160000 "$gitlink_1" s)
history gitlink-one-side && record gitlink-one-side $std
BASE=(160000 "$gitlink_1" s) OURS=(160000 "$gitlink_2" s) THEIRS=(160000 "$gitlink_2" s)
history gitlink-both-same && record gitlink-both-same $std
BASE=(160000 "$gitlink_1" s) OURS=(160000 "$gitlink_2" s) THEIRS=(160000 "$gitlink_3" s)
history gitlink-both-different && record gitlink-both-different $std
BASE=(160000 "$gitlink_1" s) OURS=() THEIRS=(160000 "$gitlink_2" s)
history gitlink-delete-modify && record gitlink-delete-modify $std

### Histories with several merge bases.

# `merge_commit <tree> <parent>...` creates a commit with the given parents.
merge_commit () {
  local tree_id=$1 parent
  shift
  local -a parents=()
  for parent in "$@"; do parents+=(-p "$parent"); done
  git commit-tree "$tree_id" "${parents[@]}" -m merge
}

# Criss-cross: both sides merged X and Y, but resolved the conflict on line 2 differently.
base_id=$(git commit-tree "$(tree 100644 "$text" f)" -m criss-cross-base)
x_id=$(git commit-tree "$(tree 100644 "$(edit "$text" 2 x)" f)" -p "$base_id" -m X)
y_id=$(git commit-tree "$(tree 100644 "$(edit "$text" 2 y)" f)" -p "$base_id" -m Y)
git update-ref refs/heads/criss-cross-conflict-A "$(merge_commit "$(tree 100644 "$(edit "$text" 2 a)" f)" "$x_id" "$y_id")"
git update-ref refs/heads/criss-cross-conflict-B "$(merge_commit "$(tree 100644 "$(edit "$text" 2 b)" f)" "$y_id" "$x_id")"
record criss-cross-conflict $std diff3

# Criss-cross: the conflicting base hunk was resolved identically, then each side edited elsewhere.
resolved=$(edit "$text" 2 resolved)
git update-ref refs/heads/criss-cross-resolved-A "$(merge_commit "$(tree 100644 "$(edit "$resolved" 9 ours)" f)" "$x_id" "$y_id")"
git update-ref refs/heads/criss-cross-resolved-B "$(merge_commit "$(tree 100644 "$(edit "$resolved" 5 theirs)" f)" "$y_id" "$x_id")"
record criss-cross-resolved $std diff3

# Criss-cross: the bases disagree on deleting and modifying a file.
base_id=$(git commit-tree "$(tree 100644 "$text" f 100644 "$other" g)" -m criss-cross-delete-base)
x_id=$(git commit-tree "$(tree 100644 "$other" g)" -p "$base_id" -m X)
y_id=$(git commit-tree "$(tree 100644 "$(edit "$text" 4 y)" f 100644 "$other" g)" -p "$base_id" -m Y)
git update-ref refs/heads/criss-cross-modify-delete-A "$(merge_commit "$(tree 100644 "$(edit "$text" 4 y)" f 100644 "$(edit "$other" 2 ours)" g)" "$x_id" "$y_id")"
git update-ref refs/heads/criss-cross-modify-delete-B "$(merge_commit "$(tree 100644 "$(edit "$other" 9 theirs)" g)" "$y_id" "$x_id")"
record criss-cross-modify-delete $std

# Criss-cross: one base renamed the file the other edited.
base_id=$(git commit-tree "$(tree 100644 "$text" f)" -m criss-cross-rename-base)
x_id=$(git commit-tree "$(tree 100644 "$text" g)" -p "$base_id" -m X)
y_id=$(git commit-tree "$(tree 100644 "$(edit "$text" 3 y)" f)" -p "$base_id" -m Y)
git update-ref refs/heads/criss-cross-rename-A "$(merge_commit "$(tree 100644 "$(edit "$(edit "$text" 3 y)" 8 ours)" g)" "$x_id" "$y_id")"
git update-ref refs/heads/criss-cross-rename-B "$(merge_commit "$(tree 100644 "$(edit "$(edit "$text" 3 y)" 6 theirs)" g)" "$y_id" "$x_id")"
record criss-cross-rename $std

# Three merge bases, each changing a different line, merged by octopus commits in different orders.
base_id=$(git commit-tree "$(tree 100644 "$text" f)" -m three-bases-base)
x_id=$(git commit-tree "$(tree 100644 "$(edit "$text" 2 x)" f)" -p "$base_id" -m X)
y_id=$(git commit-tree "$(tree 100644 "$(edit "$text" 2 y)" f)" -p "$base_id" -m Y)
z_id=$(git commit-tree "$(tree 100644 "$(edit "$text" 9 z)" f)" -p "$base_id" -m Z)
git update-ref refs/heads/three-bases-A "$(merge_commit "$(tree 100644 "$(edit "$(edit "$text" 2 x)" 9 z)" f)" "$x_id" "$y_id" "$z_id")"
git update-ref refs/heads/three-bases-B "$(merge_commit "$(tree 100644 "$(edit "$(edit "$(edit "$text" 2 y)" 9 z)" 5 theirs)" f)" "$z_id" "$y_id" "$x_id")"
record three-bases $std diff3

### Unrelated histories.

git update-ref refs/heads/unrelated-disjoint-A "$(git commit-tree "$(tree 100644 "$text" a)" -m A)"
git update-ref refs/heads/unrelated-disjoint-B "$(git commit-tree "$(tree 100644 "$other" b)" -m B)"
record unrelated-disjoint $std
git update-ref refs/heads/unrelated-same-path-A "$(git commit-tree "$(tree 100644 "$(edit "$text" 4 ours)" f)" -m A)"
git update-ref refs/heads/unrelated-same-path-B "$(git commit-tree "$(tree 100644 "$(edit "$text" 4 theirs)" f)" -m B)"
record unrelated-same-path $std diff3
git update-ref refs/heads/unrelated-identical-A "$(git commit-tree "$(tree 100644 "$text" f 100644 "$other" a)" -m A)"
git update-ref refs/heads/unrelated-identical-B "$(git commit-tree "$(tree 100644 "$text" f 100644 "$third" b)" -m B)"
record unrelated-identical $std

run_queued_merges
rm -rf .git/corpus-index queue blobs
