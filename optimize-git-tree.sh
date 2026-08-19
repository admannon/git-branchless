#!/usr/bin/env bash
set -euo pipefail

BASE_ARG=""
DRY_RUN=0
MOVE_TAGS=0
MAX_ROUNDS=0

POSITIONAL_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)
      BASE_ARG="$2"
      shift 2
      ;;
    --base=*)
      BASE_ARG="${1#*=}"
      shift
      ;;
    --dry-run|-n)
      DRY_RUN=1
      shift
      ;;
    --move-tags)
      MOVE_TAGS=1
      shift
      ;;
    --max-rounds)
      MAX_ROUNDS="$2"
      shift 2
      ;;
    --max-rounds=*)
      MAX_ROUNDS="${1#*=}"
      shift
      ;;
    *)
      POSITIONAL_ARGS+=("$1")
      shift
      ;;
  esac
done

if [[ -n "${POSITIONAL_ARGS[0]:-}" ]]; then
  ROOT_REF="${POSITIONAL_ARGS[0]}"
elif [[ -n "$BASE_ARG" ]]; then
  ROOT_REF="$BASE_ARG"
else
  ROOT_REF="HEAD"
fi

if ! git diff-index --quiet HEAD -- 2>/dev/null; then
  echo "error: worktree must be clean" >&2
  git_err() { return 1; }
  git_err
fi

ROOT_OID=$(git rev-parse --verify "${ROOT_REF}^{commit}" 2>/dev/null || true)
if [[ -z "$ROOT_OID" ]]; then
  echo "error: invalid root commit: $ROOT_REF" >&2
  git_err() { return 1; }
  git_err
fi

# Filter refs below root
ANY_DESC=0
ALL_REFS=$(git for-each-ref --format="%(refname) %(objectname)" refs/heads refs/tags)
while read -r refname refoid; do
  [[ -z "$refname" ]] && continue
  if git merge-base --is-ancestor "$ROOT_OID" "$refoid" 2>/dev/null; then
    ANY_DESC=1
  fi
done <<< "$ALL_REFS"

if [[ $ANY_DESC -eq 0 ]]; then
  echo "error: no refs descend from root $ROOT_REF ($ROOT_OID)" >&2
  git_err() { return 1; }
  git_err
fi

python3 - "$ROOT_OID" "$DRY_RUN" "$MOVE_TAGS" "$MAX_ROUNDS" << "PYEOF"
import sys
import subprocess
import os

root_oid = sys.argv[1]
dry_run = sys.argv[2] == "1"
move_tags = sys.argv[3] == "1"
max_rounds = int(sys.argv[4])

env = os.environ.copy()
env["GIT_EDITOR"] = "true"
env["GIT_SEQUENCE_EDITOR"] = "true"
env["GIT_TERMINAL_PROMPT"] = "0"

def run_cmd(cmd, check=True):
    res = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    if check and res.returncode != 0:
        sys.stderr.write(f"Command failed: {cmd}\n{res.stderr}\n")
        sys.exit(1)
    return res.stdout.strip()

def is_ancestor(a, b):
    res = subprocess.run(["git", "merge-base", "--is-ancestor", a, b], env=env)
    return res.returncode == 0

def get_all_refs():
    out = run_cmd(["git", "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads", "refs/tags"])
    refs = {}
    for line in out.splitlines():
        if line:
            rname, roid = line.split()
            if is_ancestor(root_oid, roid):
                refs[rname] = roid
    return refs

initial_refs = get_all_refs()
initial_tags = {r: oid for r, oid in initial_refs.items() if r.startswith("refs/tags/")}

def get_reachable_commits():
    out = run_cmd(["git", "for-each-ref", "--format=%(refname)", "refs/heads", "refs/tags"])
    refnames = [r for r in out.splitlines() if r and not r.startswith("refs/heads/gto-tag-tmp/")]
    valid_refs = [r for r in refnames if is_ancestor(root_oid, run_cmd(["git", "rev-parse", "--verify", r]))]
    if not valid_refs:
        return []
    revs = run_cmd(["git", "rev-list", "--topo-order"] + valid_refs + ["^" + root_oid])
    commits = [c for c in revs.splitlines() if c]
    if root_oid not in commits:
        commits.insert(0, root_oid)
    return commits

def get_commit_info(oid):
    tree_hash = run_cmd(["git", "rev-parse", f"{oid}^{{tree}}"])
    if oid == root_oid:
        depth = 0
    else:
        depth = int(run_cmd(["git", "rev-list", "--count", f"{root_oid}..{oid}"]))
    cdate = int(run_cmd(["git", "log", "-1", "--format=%ct", oid]))
    return tree_hash, depth, cdate, oid

def select_canonicals(commits):
    groups = {}
    commit_info = {}
    for c in commits:
        thash, depth, cdate, oid = get_commit_info(c)
        if thash not in groups:
            groups[thash] = []
        groups[thash].append(oid)
        commit_info[oid] = (depth, cdate, oid)

    canonicals = {}
    for thash, members in groups.items():
        sorted_members = sorted(members, key=lambda o: commit_info[o])
        canonical_oid = sorted_members[0]
        for o in members:
            canonicals[o] = canonical_oid
    return canonicals, groups

plan_actions = []

def rebase_branch_onto(target_branch_ref, old_base_oid, new_base_oid):
    target_oid = run_cmd(["git", "rev-parse", "--verify", target_branch_ref])
    revs = run_cmd(["git", "rev-list", "--reverse", f"{old_base_oid}..{target_oid}"]).splitlines()
    revs = [r for r in revs if r]
    curr_parent = new_base_oid
    for r in revs:
        tree = run_cmd(["git", "rev-parse", f"{r}^{{tree}}"])
        msg = run_cmd(["git", "log", "-1", "--format=%B", r])
        curr_parent = run_cmd(["git", "commit-tree", tree, "-p", curr_parent, "-m", msg])

    bname = target_branch_ref.replace("refs/heads/", "")
    run_cmd(["git", "update-ref", f"refs/heads/{bname}", curr_parent])

initial_commits = get_reachable_commits()
initial_canonicals, initial_tree_groups = select_canonicals(initial_commits)
dup_targets = {}
for thash, members in initial_tree_groups.items():
    if len(members) > 1:
        for m in members:
            if m != initial_canonicals[m]:
                dup_targets[m] = initial_canonicals[m]

round_num = 0
while True:
    round_num += 1
    if max_rounds > 0 and round_num > max_rounds:
        sys.stderr.write("error: exceeded max rounds\n")
        if not dry_run:
            for rname, roid in initial_refs.items():
                run_cmd(["git", "update-ref", rname, roid], check=False)
        sys.exit(1)

    commits = get_reachable_commits()
    proc_commits = [c for c in commits if c != root_oid]
    if not proc_commits:
        break

    canonicals, tree_groups = select_canonicals(commits)

    head_refs = {}
    for line in run_cmd(["git", "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads"]).splitlines():
        if line:
            rname, roid = line.split()
            if is_ancestor(root_oid, roid):
                if roid not in head_refs:
                    head_refs[roid] = []
                head_refs[roid].append(rname)

    added_temp = False
    for thash, members in tree_groups.items():
        if len(members) > 1:
            for m in members:
                if m == root_oid:
                    continue
                if m not in head_refs or len(head_refs[m]) == 0:
                    temp_ref = f"refs/heads/gto/{m}"
                    if not dry_run:
                        run_cmd(["git", "branch", f"gto/{m}", m], check=False)
                    plan_actions.append(f"Step 0: create temp branch {temp_ref} for {m}")
                    added_temp = True
                    if m not in head_refs:
                        head_refs[m] = []
                    head_refs[m].append(temp_ref)

    if added_temp:
        commits = get_reachable_commits()
        proc_commits = [c for c in commits if c != root_oid]
        canonicals, tree_groups = select_canonicals(commits)

    changed_in_round = False

    while True:
        p1_changed = False
        commits = get_reachable_commits()
        proc_commits = [c for c in commits if c != root_oid]
        canonicals, tree_groups = select_canonicals(commits)

        for c_prime in proc_commits:
            canonical_c = canonicals[c_prime]
            if c_prime == canonical_c:
                continue
            if not is_ancestor(canonical_c, c_prime):
                branch_lines = run_cmd(["git", "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads"]).splitlines()
                for bline in branch_lines:
                    if not bline:
                        continue
                    rname, roid = bline.split()
                    if rname.startswith("refs/heads/gto-keep/") or not is_ancestor(root_oid, roid):
                        continue
                    if is_ancestor(c_prime, roid):
                        keep_ref = f"refs/heads/gto-keep/{c_prime}"
                        if not dry_run and not run_cmd(["git", "rev-parse", "--verify", keep_ref], check=False):
                            run_cmd(["git", "branch", f"gto-keep/{c_prime}", c_prime], check=False)
                        plan_actions.append(f"Pattern 1: preserve chain {keep_ref} at {c_prime}")

                        if c_prime != roid:
                            plan_actions.append(f"Pattern 1: rebase {rname} onto {canonical_c} (from {c_prime})")
                            if not dry_run:
                                rebase_branch_onto(rname, c_prime, canonical_c)
                        p1_changed = True
                        changed_in_round = True
                        break
                if p1_changed:
                    break
        if not p1_changed:
            break

    while True:
        p2_changed = False
        commits = get_reachable_commits()
        proc_commits = [c for c in commits if c != root_oid]
        canonicals, tree_groups = select_canonicals(commits)

        for Y in proc_commits:
            canonical_c = canonicals[Y]
            if Y == canonical_c:
                continue
            if is_ancestor(canonical_c, Y):
                branch_lines = run_cmd(["git", "for-each-ref", "--format=%(refname) %(objectname)", "refs/heads"]).splitlines()
                for bline in branch_lines:
                    if not bline:
                        continue
                    rname, roid = bline.split()
                    if rname.startswith("refs/heads/gto-keep/") or not is_ancestor(root_oid, roid):
                        continue
                    if is_ancestor(Y, roid):
                        keep_ref = f"refs/heads/gto-keep/{Y}"
                        if not dry_run and not run_cmd(["git", "rev-parse", "--verify", keep_ref], check=False):
                            run_cmd(["git", "branch", f"gto-keep/{Y}", Y], check=False)
                        plan_actions.append(f"Pattern 2: preserve chain {keep_ref} at {Y}")

                        if Y != roid:
                            plan_actions.append(f"Pattern 2: rebase {rname} onto {canonical_c} (from {Y})")
                            if not dry_run:
                                rebase_branch_onto(rname, Y, canonical_c)
                        p2_changed = True
                        changed_in_round = True
                        break
                if p2_changed:
                    break
        if not p2_changed:
            break

    if not changed_in_round:
        break

if not dry_run:
    if move_tags:
        for tag_ref, tag_oid in initial_tags.items():
            tname = tag_ref.replace("refs/tags/", "")
            if tag_oid in dup_targets:
                new_target = dup_targets[tag_oid]
                run_cmd(["git", "tag", "-f", tname, new_target], check=False)
    else:
        for tag_ref, tag_oid in initial_tags.items():
            if tag_oid in dup_targets:
                print(f"{tag_ref} left")

if dry_run:
    print("plan:")
    for act in plan_actions:
        print(f"  {act}")

if not dry_run:
    all_heads = run_cmd(["git", "for-each-ref", "--format=%(refname)", "refs/heads/gto"]).splitlines()
    for h in all_heads:
        if h and not h.startswith("refs/heads/gto-keep/"):
            bname = h.replace("refs/heads/", "")
            run_cmd(["git", "branch", "-D", bname], check=False)

PYEOF
