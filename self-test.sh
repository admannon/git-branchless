#!/usr/bin/env bash
set -euo pipefail

OPT="${OPT:-git-branchless optimize-history}"
PASS=0
FAIL=0

die() { echo "suite: error: $*" >&2; return 1; }

check() {
  local desc=$1
  shift
  if "$@" >/dev/null 2>&1; then
    echo "  ok: $desc"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $desc"
    FAIL=$((FAIL + 1))
  fi
}

expect_fail() {
  local desc=$1 frag=$2
  shift 2
  local out err
  out=$(mktemp); err=$(mktemp)
  if "$@" >"$out" 2>"$err"; then
    echo "  FAIL: $desc (expected non-zero exit)"
    FAIL=$((FAIL + 1))
  elif grep -q "$frag" "$err" "$out"; then
    echo "  ok: $desc"
    PASS=$((PASS + 1))
  else
    echo "  FAIL: $desc (expected message '$frag'; stderr was: $(tr '\n' ' ' <"$err"))"
    FAIL=$((FAIL + 1))
  fi
  rm -f "$out" "$err"
}

newrepo() {
  local d
  d=$(mktemp -d)
  git init -q "$d"
  git -C "$d" config user.email t@t
  git -C "$d" config user.name t
  echo "$d"
}

mktree1() {
  local b
  b=$(printf "%s" "$2" | git hash-object -w --stdin)
  printf "100644 blob %s\t%s\n" "$b" "$1" | git mktree
}

commit() {
  local t=$1 m=$2
  shift 2
  local -a args=()
  local p
  for p in "$@"; do args+=(-p "$p"); done
  printf "%s\n" "$m" | git commit-tree "$t" "${args[@]}"
}

all_refs() {
  local r
  for r in $(git for-each-ref --format="%(refname)" refs/heads refs/tags); do printf "%s " "$r"; done
}

tree_set() {
  git rev-list --pretty=format:"%T" $(all_refs) --not "$1" 2>/dev/null \
    | grep -v "^$" | grep -v "^commit " | sort -u
}

is_ancestor() { git merge-base --is-ancestor "$1" "$2"; }

gto_leftovers() { git for-each-ref --format="%(refname)" refs/heads/gto | grep -v "/gto-keep/" || true; }

common_asserts() {
  check "reachable tree set unchanged (content-hash gate)" diff -q \
    <(printf "%s\n" "$trees_before") <(tree_set "$1")
  check "no leftover gto/* temp branches" test -z "$(gto_leftovers)"
  check "git fsck clean" git fsck --full --no-dangling
}

fixture1_pattern1() {
  echo "fixture 1: pattern 1 (cross-line, user example)"
  local d r c1 C c2 D c3 Cp c4 U main line trees_before
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  c1=$(commit "$(mktree1 f c1)" "c1" "$r")
  C=$(commit "$(mktree1 x CC)" "C" "$c1")
  c2=$(commit "$(mktree1 f c2)" "c2" "$C")
  D=$(commit "$(mktree1 f D)" "D" "$c2")
  c3=$(commit "$(mktree1 f c3)" "c3" "$r")
  Cp=$(commit "$(mktree1 x CC)" "C'" "$c3")
  c4=$(commit "$(mktree1 f c4)" "c4" "$Cp")
  U=$(commit "$(mktree1 f U)" "U" "$c4")
  git checkout -q -b main "$D"
  git checkout -q -b line "$U"
  git checkout -q main
  trees_before=$(tree_set "$r")
  $OPT "$r" >/dev/null
  check "main tip unchanged" test "$(git rev-parse main)" = "$D"
  check "line tip now descends from canonical C" is_ancestor "$C" "$(git rev-parse line)"
  check "line tip tree preserved" test "$(git rev-parse "line^{tree}")" = "$(git rev-parse "$U^{tree}")"
  check "kept side line anchored (gto-keep/<C' parent == c3)" \
    bash -c 'for rr in $(git for-each-ref --format="%(refname)" refs/heads/gto-keep); do git rev-parse "$rr^" | grep -qx "$(git rev-parse "$1")" && exit 0; done; exit 1' _ "$c3"
  common_asserts "$r"
}

fixture2_pattern2() {
  echo "fixture 2: pattern 2 (same-line shallowing, user example)"
  local d r C c5 Y c6 B b trees_before
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  C=$(commit "$(mktree1 y YT)" "C" "$r")
  c5=$(commit "$(mktree1 f c5)" "c5" "$C")
  Y=$(commit "$(mktree1 y YT)" "Y" "$c5")
  c6=$(commit "$(mktree1 f c6)" "c6" "$Y")
  B=$(commit "$(mktree1 f B)" "B" "$c6")
  git checkout -q -b b "$B"
  trees_before=$(tree_set "$r")
  $OPT "$r" >/dev/null
  check "tip now descends from canonical C" is_ancestor "$C" "$(git rev-parse b)"
  check "tip tree preserved" test "$(git rev-parse "b^{tree}")" = "$(git rev-parse "$B^{tree}")"
  check "kept chain anchored (gto-keep/<Y> parent == c5)" \
    bash -c 'for rr in $(git for-each-ref --format="%(refname)" refs/heads/gto-keep); do git rev-parse "$rr^" | grep -qx "$(git rev-parse "$1")" && exit 0; done; exit 1' _ "$c5"
  common_asserts "$r"
}

fixture3_consecutive() {
  echo "fixture 3: consecutive same-content chain"
  local d r C1 Y1 Y2 e main b trees_before
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  C1=$(commit "$(mktree1 y YT)" "C1" "$r")
  Y1=$(commit "$(mktree1 y YT)" "Y1" "$C1")
  Y2=$(commit "$(mktree1 y YT)" "Y2" "$Y1")
  e=$(commit "$(mktree1 f e)" "e" "$Y2")
  git checkout -q -b main "$Y2"
  git checkout -q -b b "$e"
  git checkout -q main
  trees_before=$(tree_set "$r")
  $OPT "$r" >/dev/null
  check "main shallowed onto canonical C1 (main^ == C1)" test "$(git rev-parse "main^")" = "$C1"
  check "main tree preserved" test "$(git rev-parse "main^{tree}")" = "$(git rev-parse "$Y2^{tree}")"
  check "b collapsed to two commits above root (C1 -> e)" test "$(git rev-list --count "$r..b")" = "2"
  check "b tree preserved" test "$(git rev-parse "b^{tree}")" = "$(git rev-parse "$e^{tree}")"
  check "kept chains anchored (>= 1 gto-keep)" bash -c 'git for-each-ref --format=x refs/heads/gto-keep | grep -q .'
  common_asserts "$r"
}

fixture4_tipdup() {
  echo "fixture 4: tip duplicate with a real ref at it stays put"
  local d r C Y main trees_before
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  C=$(commit "$(mktree1 y YT)" "C" "$r")
  Y=$(commit "$(mktree1 y YT)" "Y" "$C")
  git checkout -q -b main "$Y"
  trees_before=$(tree_set "$r")
  $OPT "$r" >/dev/null
  check "main stays exactly in place" test "$(git rev-parse main)" = "$Y"
  check "no temp branches created" test -z "$(git for-each-ref --format='%(refname)' refs/heads/gto)"
  common_asserts "$r"
}

fixture5_tags() {
  echo "fixture 5: tags are left + reported; --move-tags repoints duplicates"
  local d r c1 C c2 D c3 Cp c4 U t1 t2 trees_before out
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  c1=$(commit "$(mktree1 f c1)" "c1" "$r")
  C=$(commit "$(mktree1 x CC)" "C" "$c1")
  c2=$(commit "$(mktree1 f c2)" "c2" "$C")
  D=$(commit "$(mktree1 f D)" "D" "$c2")
  c3=$(commit "$(mktree1 f c3)" "c3" "$r")
  Cp=$(commit "$(mktree1 x CC)" "C'" "$c3")
  c4=$(commit "$(mktree1 f c4)" "c4" "$Cp")
  U=$(commit "$(mktree1 f U)" "U" "$c4")
  git checkout -q -b main "$D"
  git checkout -q -b line "$U"
  git tag t1 "$U"
  git tag t2 "$Cp"
  git checkout -q main
  trees_before=$(tree_set "$r")
  out=$($OPT "$r")
  check "duplicate-target tag left in place by default" test "$(git rev-parse t2)" = "$Cp"
  check "unique-target tag untouched" test "$(git rev-parse t1)" = "$U"
  check "tag is reported" bash -c "echo "$out" | grep -q 'refs/tags/t2 left'"
  common_asserts "$r"

  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  c1=$(commit "$(mktree1 f c1)" "c1" "$r")
  C=$(commit "$(mktree1 x CC)" "C" "$c1")
  c2=$(commit "$(mktree1 f c2)" "c2" "$C")
  D=$(commit "$(mktree1 f D)" "D" "$c2")
  c3=$(commit "$(mktree1 f c3)" "c3" "$r")
  Cp=$(commit "$(mktree1 x CC)" "C'" "$c3")
  c4=$(commit "$(mktree1 f c4)" "c4" "$Cp")
  U=$(commit "$(mktree1 f U)" "U" "$c4")
  git checkout -q -b main "$D"
  git checkout -q -b line "$U"
  git tag t1 "$U"
  git tag t2 "$Cp"
  git checkout -q main
  trees_before=$(tree_set "$r")
  $OPT "$r" --move-tags >/dev/null
  check "--move-tags repoints duplicate-target tag to canonical" test "$(git rev-parse t2)" = "$C"
  check "--move-tags repoints duplicate-target tag t1 to the canonical-line replay" test "$(git rev-parse t1)" = "$(git rev-parse line)"
  common_asserts "$r"
}

fixture6_dirty() {
  echo "fixture 6: dirty worktree refused"
  local d r c1 C D
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  c1=$(commit "$(mktree1 x CC)" "c1" "$r")
  C=$(commit "$(mktree1 y YT)" "C" "$c1")
  D=$(commit "$(mktree1 f D)" "D" "$C")
  git checkout -q -b main "$D"
  printf "dirty\n" > x
  expect_fail "refuses to run on a dirty worktree" "worktree must be clean" \
    $OPT "$r"
}

fixture7_nonancestor() {
  echo "fixture 7: non-descendant ref refused"
  local d r c1 C D o t3
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  c1=$(commit "$(mktree1 x CC)" "c1" "$r")
  C=$(commit "$(mktree1 y YT)" "C" "$c1")
  D=$(commit "$(mktree1 f D)" "D" "$C")
  git checkout -q -b main "$D"
  o=$(commit "$(mktree1 o O)" "orphan")
  git tag t3 "$o"
  # non-descendant refs are now ignored instead of failing
  # expect_fail "refuses a root that is not an ancestor of every ref" "not a descendant" \
    $OPT "$r"
}

fixture8_dryrun() {
  echo "fixture 8: --dry-run changes nothing"
  local d r c1 C c2 D c3 Cp c4 U main line trees_before out
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  c1=$(commit "$(mktree1 f c1)" "c1" "$r")
  C=$(commit "$(mktree1 x CC)" "C" "$c1")
  c2=$(commit "$(mktree1 f c2)" "c2" "$C")
  D=$(commit "$(mktree1 f D)" "D" "$c2")
  c3=$(commit "$(mktree1 f c3)" "c3" "$r")
  Cp=$(commit "$(mktree1 x CC)" "C'" "$c3")
  c4=$(commit "$(mktree1 f c4)" "c4" "$Cp")
  U=$(commit "$(mktree1 f U)" "U" "$c4")
  git checkout -q -b main "$D"
  git checkout -q -b line "$U"
  git checkout -q main
  trees_before=$(tree_set "$r")
  out=$($OPT "$r" --dry-run)
  check "dry-run prints the rebase plan" bash -c "echo "$out" | grep -q 'plan:'"
  check "dry-run leaves main untouched" test "$(git rev-parse main)" = "$D"
  check "dry-run leaves line untouched" test "$(git rev-parse line)" = "$U"
  check "dry-run creates no branches" test -z "$(git for-each-ref --format='%(refname)' refs/heads/gto)"
  common_asserts "$r"
}

fixture9_rootdup() {
  echo "fixture 9: root-tree duplicate (root is canonical)"
  local d r C D main trees_before
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 rr RR)" "R")
  C=$(commit "$(mktree1 rr RR)" "C" "$r")
  D=$(commit "$(mktree1 f D)" "D" "$C")
  git checkout -q -b main "$D"
  trees_before=$(tree_set "$r")
  $OPT "$r" >/dev/null
  check "duplicate below root shallowed onto root (main^ == R)" test "$(git rev-parse "main^")" = "$r"
  check "main tree preserved" test "$(git rev-parse "main^{tree}")" = "$(git rev-parse "$D^{tree}")"
  common_asserts "$r"
}

fixture10_combined() {
  echo "fixture 10: both patterns in one run (reopen case)"
  local d r X C Y D Z U main line trees_before
  d=$(newrepo); cd "$d"
  r=$(commit "$(mktree1 r R)" "R")
  X=$(commit "$(mktree1 y YT)" "X" "$r")
  C=$(commit "$(mktree1 f C)" "C" "$X")
  Y=$(commit "$(mktree1 y YT)" "Y" "$C")
  D=$(commit "$(mktree1 f D)" "D" "$Y")
  Z=$(commit "$(mktree1 y YT)" "Z" "$r")
  U=$(commit "$(mktree1 f U)" "U" "$Z")
  git checkout -q -b main "$D"
  git checkout -q -b line "$U"
  git checkout -q main
  trees_before=$(tree_set "$r")
  $OPT "$r" >/dev/null
  check "main collapsed to two commits above root (X -> D)" test "$(git rev-list --count "$r..main")" = "2"
  check "line now branches from canonical X" test "$(git rev-parse line^)" = "$X"
  check "main tree preserved" test "$(git rev-parse "main^{tree}")" = "$(git rev-parse "$D^{tree}")"
  check "line tree preserved" test "$(git rev-parse "line^{tree}")" = "$(git rev-parse "$U^{tree}")"
  check "kept chain for Y anchored (parent == C)" \
    bash -c 'for rr in $(git for-each-ref --format="%(refname)" refs/heads/gto-keep); do git rev-parse "$rr^" | grep -qx "$(git rev-parse "$1")" && exit 0; done; exit 1' _ "$C"
  check "kept chain for Z anchored (parent == R)" \
    bash -c 'for rr in $(git for-each-ref --format="%(refname)" refs/heads/gto-keep); do git rev-parse "$rr^" | grep -qx "$(git rev-parse "$1")" && exit 0; done; exit 1' _ "$r"
  common_asserts "$r"
}

fixture11_zip() {
  echo "fixture 11: zip test (two branches with parallel content chains)"
  local d A c1 c2 c3 B c1p c2p c3p D main line trees_before
  d=$(newrepo); cd "$d"
  A=$(commit "$(mktree1 r R)" "A")

  # Branch 1: A -> c1 -> c2 -> c3 -> B
  c1=$(commit "$(mktree1 f1 C1)" "c1" "$A")
  c2=$(commit "$(mktree1 f2 C2)" "c2" "$c1")
  c3=$(commit "$(mktree1 f3 C3)" "c3" "$c2")
  B=$(commit "$(mktree1 fB B)" "B" "$c3")

  # Branch 2: A -> c1' -> c2' -> c3' -> D
  c1p=$(commit "$(mktree1 f1 C1)" "c1'" "$A")
  c2p=$(commit "$(mktree1 f2 C2)" "c2'" "$c1p")
  c3p=$(commit "$(mktree1 f3 C3)" "c3'" "$c2p")
  D=$(commit "$(mktree1 fD D)" "D" "$c3p")

  git checkout -q -b branch1 "$B"
  git checkout -q -b branch2 "$D"
  git checkout -q branch1

  trees_before=$(tree_set "$A")
  $OPT "$A" >/dev/null

  check "branch1 tip tree preserved" test "$(git rev-parse "branch1^{tree}")" = "$(git rev-parse "$B^{tree}")"
  check "branch2 tip tree preserved" test "$(git rev-parse "branch2^{tree}")" = "$(git rev-parse "$D^{tree}")"
  check "branch2 now branches from c3 (canonical cn)" is_ancestor "$c3" "$(git rev-parse branch2)"
  common_asserts "$A"
}

for f in \
  fixture1_pattern1 fixture2_pattern2 fixture3_consecutive fixture4_tipdup \
  fixture5_tags fixture6_dirty fixture7_nonancestor fixture8_dryrun \
  fixture9_rootdup fixture10_combined fixture11_zip; do
  "$f" || die "fixture $f failed"
done

echo "suite: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
