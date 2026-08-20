//! Implementation of `git branchless optimize-history` natively using `git2` and `lib::git::Repo`.

use std::collections::{BTreeMap, HashMap};




use git_branchless_opts::Revset;
use lib::core::effects::Effects;
use lib::git::git2::{Oid, Sort};
use lib::git::{git2, GitRunInfo, Repo};
use lib::util::{ExitCode, EyreExitOr};

fn is_ancestor_git2(raw_repo: &git2::Repository, a: Oid, b: Oid) -> bool {
    if a == b {
        return true;
    }
    raw_repo.graph_descendant_of(b, a).unwrap_or(false)
}

fn get_all_local_refs_git2(
    raw_repo: &git2::Repository,
) -> Result<BTreeMap<String, Oid>, git2::Error> {
    let mut map = BTreeMap::new();
    let references = raw_repo.references()?;
    for reference in references {
        let reference = reference?;
        if let Some(name) = reference.name() {
            if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
                if let Some(target) = reference.target() {
                    map.insert(name.to_string(), target);
                } else if let Ok(peeled) = reference.peel_to_commit() {
                    map.insert(name.to_string(), peeled.id());
                }
            }
        }
    }
    Ok(map)
}

fn get_reachable_commits_git2(
    raw_repo: &git2::Repository,
    root_oid: Oid,
) -> Result<Vec<Oid>, git2::Error> {
    let refs = get_all_local_refs_git2(raw_repo)?;
    if refs.is_empty() {
        return Ok(vec![]);
    }

    let mut revwalk = raw_repo.revwalk()?;
    revwalk.set_sorting(Sort::TOPOLOGICAL)?;

    for name in refs.keys() {
        if !name.starts_with("refs/heads/gto-tag-tmp/") && !name.starts_with("refs/tags/") {
            revwalk.push_ref(name)?;
        }
    }
    revwalk.hide(root_oid)?;

    let mut commits: Vec<Oid> = revwalk.collect::<Result<Vec<_>, _>>()?;
    if !commits.contains(&root_oid) {
        commits.insert(0, root_oid);
    }
    Ok(commits)
}

struct CommitInfo {
    tree_id: Oid,
    depth: usize,
    cdate: i64,
    oid: Oid,
}

fn get_commit_info_git2(
    raw_repo: &git2::Repository,
    oid: Oid,
    root_oid: Oid,
    depth_cache: &mut HashMap<Oid, usize>,
) -> Result<CommitInfo, git2::Error> {
    let commit = raw_repo.find_commit(oid)?;
    let tree_id = commit.tree_id();
    let cdate = commit.time().seconds();

    let depth = if oid == root_oid {
        0
    } else if let Some(&d) = depth_cache.get(&oid) {
        d
    } else {
        let mut revwalk = raw_repo.revwalk()?;
        revwalk.push(oid)?;
        revwalk.hide(root_oid)?;
        let d = revwalk.count();
        depth_cache.insert(oid, d);
        d
    };

    Ok(CommitInfo {
        tree_id,
        depth,
        cdate,
        oid,
    })
}

fn select_canonicals_git2(
    raw_repo: &git2::Repository,
    commits: &[Oid],
    root_oid: Oid,
    depth_cache: &mut HashMap<Oid, usize>,
) -> Result<(HashMap<Oid, Oid>, HashMap<Oid, Vec<Oid>>), git2::Error> {
    let mut groups: HashMap<Oid, Vec<Oid>> = HashMap::new();
    let mut commit_info: HashMap<Oid, (usize, i64, Oid)> = HashMap::new();

    for &c in commits {
        let info = get_commit_info_git2(raw_repo, c, root_oid, depth_cache)?;
        groups.entry(info.tree_id).or_default().push(c);
        commit_info.insert(c, (info.depth, info.cdate, info.oid));
    }

    let mut canonicals = HashMap::new();
    for (_thash, members) in &groups {
        let mut sorted_m = members.clone();
        sorted_m.sort_by(|a, b| {
            let info_a = &commit_info[a];
            let info_b = &commit_info[b];
            info_a.0
                .cmp(&info_b.0)
                .then_with(|| info_a.1.cmp(&info_b.1))
                .then_with(|| info_a.2.cmp(&info_b.2))
        });
        let canonical_oid = sorted_m[0];
        for &m in members {
            canonicals.insert(m, canonical_oid);
        }
    }

    Ok((canonicals, groups))
}

fn rebase_branch_onto_git2(
    raw_repo: &git2::Repository,
    target_branch_ref: &str,
    old_base_oid: Oid,
    new_base_oid: Oid,
    old_to_new: &mut HashMap<Oid, Oid>,
) -> Result<(), git2::Error> {
    let target_oid = raw_repo
        .find_reference(target_branch_ref)?
        .peel_to_commit()?
        .id();

    let mut revwalk = raw_repo.revwalk()?;
    revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    revwalk.push(target_oid)?;
    revwalk.hide(old_base_oid)?;

    let revs: Vec<Oid> = revwalk.collect::<Result<Vec<_>, _>>()?;

    let signature = raw_repo.signature()?;
    let mut curr_parent_oid = new_base_oid;

    for r_oid in revs {
        let commit = raw_repo.find_commit(r_oid)?;
        let tree = commit.tree()?;
        let msg = commit.message().unwrap_or("");
        let parent_commit = raw_repo.find_commit(curr_parent_oid)?;

        curr_parent_oid = raw_repo.commit(
            None,
            &signature,
            &signature,
            msg,
            &tree,
            &[&parent_commit],
        )?;
        old_to_new.insert(r_oid, curr_parent_oid);
    }

    raw_repo.reference(target_branch_ref, curr_parent_oid, true, "rebase_branch_onto")?;
    Ok(())
}

/// Run the optimize-history command natively in Rust using git2 / Repo.
pub fn optimize_history(
    _effects: &Effects,
    _git_run_info: &GitRunInfo,
    base: Option<Revset>,
    root: Option<Revset>,
    dry_run: bool,
    move_tags: bool,
    max_rounds: Option<usize>,
) -> EyreExitOr<()> {



    let repo = Repo::from_current_dir()?;
    let raw_repo = repo.raw_repo();

    let root_ref_str = root.or(base).map(|r| r.0).unwrap_or_else(|| "HEAD".to_string());

    println!("Performing pre-flight checks...");

    // Pre-flight check 1: clean worktree
    let mut status_options = git2::StatusOptions::new();
    status_options.include_untracked(true);
    let statuses = raw_repo.statuses(Some(&mut status_options))?;
    if !statuses.is_empty() {
        eprintln!("error: worktree must be clean");
        return Ok(Err(ExitCode(1)));
    }

    let root_obj = match raw_repo.revparse_single(&root_ref_str) {
        Ok(obj) => match obj.peel_to_commit() {
            Ok(c) => c,
            Err(_) => {
                eprintln!("error: invalid root commit: {root_ref_str}");
                return Ok(Err(ExitCode(1)));
            }
        },
        Err(_) => {
            eprintln!("error: invalid root commit: {root_ref_str}");
            return Ok(Err(ExitCode(1)));
        }
    };
    let root_oid = root_obj.id();

    // Filter local refs to only those that descend from root_oid
    let raw_all_refs = get_all_local_refs_git2(raw_repo)?;
    let all_refs: BTreeMap<String, Oid> = raw_all_refs
        .into_iter()
        .filter(|(_, refoid)| is_ancestor_git2(raw_repo, root_oid, *refoid))
        .collect();

    if all_refs.is_empty() {
        eprintln!("error: no local refs descend from root {root_ref_str} ({root_oid})");
        return Ok(Err(ExitCode(1)));
    }

    println!("Optimizing sub-tree history above root {root_ref_str} ({root_oid})...");

    let mut depth_cache = HashMap::new();

    let initial_refs = all_refs.clone();
    let initial_tags: BTreeMap<String, Oid> = initial_refs
        .iter()
        .filter(|(r, _)| r.starts_with("refs/tags/"))
        .map(|(r, &o)| (r.clone(), o))
        .collect();

    let initial_commits = get_reachable_commits_git2(raw_repo, root_oid)?;
    let (initial_canonicals, initial_tree_groups) = select_canonicals_git2(raw_repo, &initial_commits, root_oid, &mut depth_cache)?;

    let mut dup_targets = HashMap::new();
    for (_thash, members) in &initial_tree_groups {
        if members.len() > 1 {
            for &m in members {
                if m != initial_canonicals[&m] {
                    dup_targets.insert(m, initial_canonicals[&m]);
                }
            }
        }
    }

    let max_rounds_num = max_rounds.unwrap_or(0);
    let mut plan_actions = Vec::new();
    let mut old_to_new = HashMap::new();

    let mut round_num = 0;
    loop {
        round_num += 1;
        println!("Round {round_num}: enumerating commits and canonical groups...");
        if max_rounds_num > 0 && round_num > max_rounds_num {
            eprintln!("error: exceeded max rounds ({max_rounds_num})");
            if !dry_run {
                for (rname, &roid) in &initial_refs {
                    let _ = raw_repo.reference(rname, roid, true, "rollback");
                }
            }
            return Ok(Err(ExitCode(1)));
        }

        let commits = get_reachable_commits_git2(raw_repo, root_oid)?;
        let proc_commits: Vec<Oid> = commits.into_iter().filter(|&c| c != root_oid).collect();
        if proc_commits.is_empty() {
            println!("No reachable commits to optimize.");
            break;
        }

        let reachable = get_reachable_commits_git2(raw_repo, root_oid)?;
        let (round_cans, tree_groups) = select_canonicals_git2(raw_repo, &reachable, root_oid, &mut depth_cache)?;

        // Step 0: branch the duplicates that lack a branch ref
        let current_refs = get_all_local_refs_git2(raw_repo)?;
        let mut head_refs: HashMap<Oid, Vec<String>> = HashMap::new();
        for (rname, &roid) in &current_refs {
            if rname.starts_with("refs/heads/") {
                head_refs.entry(roid).or_default().push(rname.clone());
            }
        }

        let mut added_temp = false;
        for (_thash, members) in &tree_groups {
            if members.len() > 1 {
                for &m in members {
                    if m == root_oid {
                        continue;
                    }
                    if !head_refs.contains_key(&m) || head_refs[&m].is_empty() {
                        let temp_ref = format!("refs/heads/gto/{m}");
                        println!("Step 0: creating temporary branch {temp_ref} for duplicate commit {m}");
                        if !dry_run {
                            let commit_obj = raw_repo.find_commit(m)?;
                            let _ = raw_repo.branch(&format!("gto/{m}"), &commit_obj, false);
                            added_temp = true;
                        }
                        plan_actions.push(format!("Step 0: create temp branch {temp_ref} for {m}"));
                        head_refs.entry(m).or_default().push(temp_ref);
                    }
                }
            }
        }

        let mut changed_in_round = false;

        // Pattern 1 loop to fixpoint
        let mut p1_pass = 0;
        loop {
            p1_pass += 1;
            let mut p1_changed = false;
            let current_commits = get_reachable_commits_git2(raw_repo, root_oid)?;
            let proc_c: Vec<Oid> = current_commits.into_iter().filter(|&c| c != root_oid).collect();

            for &c_prime in &proc_c {
                let Some(&canonical_c) = round_cans.get(&c_prime) else {
                    continue;
                };
                if c_prime == canonical_c {
                    continue;
                }
                if !is_ancestor_git2(raw_repo, canonical_c, c_prime) {
                    let keep_ref = format!("refs/heads/gto-keep/{c_prime}");
                    if !dry_run && raw_repo.find_reference(&keep_ref).is_err() {
                        let commit_obj = raw_repo.find_commit(c_prime)?;
                        let _ = raw_repo.branch(&format!("gto-keep/{c_prime}"), &commit_obj, false);
                    }
                    plan_actions.push(format!("Pattern 1: preserve chain {keep_ref} at {c_prime}"));

                    let branch_refs = get_all_local_refs_git2(raw_repo)?;
                    for (rname, &roid) in &branch_refs {
                        if !rname.starts_with("refs/heads/")
                            || rname.starts_with("refs/heads/gto-keep/")
                            || rname.starts_with("refs/heads/gto/")
                        {
                            continue;
                        }
                        if is_ancestor_git2(raw_repo, c_prime, roid) {
                            if c_prime != roid {
                                println!("Round {round_num} Pass 1 (pass {p1_pass}): rebasing {rname} onto canonical {canonical_c}");
                                plan_actions.push(format!("Pattern 1: rebase {rname} onto {canonical_c} (from {c_prime})"));

                                if !dry_run {
                                    let _ = rebase_branch_onto_git2(raw_repo, rname, c_prime, canonical_c, &mut old_to_new);
                                    p1_changed = true;
                                    changed_in_round = true;
                                }
                            }
                        }
                    }
                }
            }
            if !p1_changed {
                break;
            }
        }

        // Pattern 2 loop to fixpoint
        let mut p2_pass = 0;
        loop {
            p2_pass += 1;
            let mut p2_changed = false;
            let current_commits = get_reachable_commits_git2(raw_repo, root_oid)?;
            let proc_c: Vec<Oid> = current_commits.into_iter().filter(|&c| c != root_oid).collect();

            for &y in &proc_c {
                let Some(&canonical_c) = round_cans.get(&y) else {
                    continue;
                };
                if y == canonical_c {
                    continue;
                }
                if is_ancestor_git2(raw_repo, canonical_c, y) {
                    let keep_ref = format!("refs/heads/gto-keep/{y}");
                    if !dry_run && raw_repo.find_reference(&keep_ref).is_err() {
                        let commit_obj = raw_repo.find_commit(y)?;
                        let _ = raw_repo.branch(&format!("gto-keep/{y}"), &commit_obj, false);
                    }
                    plan_actions.push(format!("Pattern 2: preserve chain {keep_ref} at {y}"));

                    let branch_refs = get_all_local_refs_git2(raw_repo)?;
                    for (rname, &roid) in &branch_refs {
                        if !rname.starts_with("refs/heads/")
                            || rname.starts_with("refs/heads/gto-keep/")
                            || rname.starts_with("refs/heads/gto/")
                        {
                            continue;
                        }
                        if is_ancestor_git2(raw_repo, y, roid) {
                            if y != roid {
                                println!("Round {round_num} Pass 2 (pass {p2_pass}): shallowing {rname} onto canonical {canonical_c}");
                                plan_actions.push(format!("Pattern 2: rebase {rname} onto {canonical_c} (from {y})"));

                                if !dry_run {
                                    let _ = rebase_branch_onto_git2(raw_repo, rname, y, canonical_c, &mut old_to_new);
                                    p2_changed = true;
                                    changed_in_round = true;
                                }
                            }
                        }
                    }
                }
            }
            if !p2_changed {
                break;
            }
        }

        if !changed_in_round && !added_temp {
            println!("Fixpoint reached in round {round_num}. Optimization complete.");
            break;
        }
    }

    // Tag handling
    if !dry_run {
        if move_tags {
            println!("Updating tags to point to canonical commits...");
            for (tag_ref, &tag_oid) in &initial_tags {
                let tname = tag_ref.trim_start_matches("refs/tags/");
                let mut cur = tag_oid;
                while let Some(&next) = old_to_new.get(&cur) {
                    if next == cur {
                        break;
                    }
                    cur = next;
                }
                let final_target = if cur != tag_oid {
                    cur
                } else if let Some(&dt) = dup_targets.get(&tag_oid) {
                    dt
                } else {
                    continue;
                };
                let _ = raw_repo.tag_lightweight(tname, &raw_repo.find_object(final_target, None)?, true);
            }
        } else {
            for (tag_ref, &tag_oid) in &initial_tags {
                if dup_targets.contains_key(&tag_oid) {
                    println!("{tag_ref} left");
                }
            }
        }
    }

    if dry_run {
        println!("plan:");
        for act in &plan_actions {
            println!("  {act}");
        }
        for (rname, &roid) in &initial_refs {
            let _ = raw_repo.reference(rname, roid, true, "rollback_dry_run");
        }
    }

    // Delete temporary branches refs/heads/gto/* (excluding gto-keep)
    if !dry_run {
        println!("Cleaning up temporary branches...");
        if let Ok(references) = raw_repo.references() {
            for reference in references.flatten() {
                if let Some(name) = reference.name() {
                    if name.starts_with("refs/heads/gto/") && !name.starts_with("refs/heads/gto-keep/") {
                        if let Ok(mut branch) = raw_repo.find_branch(name.trim_start_matches("refs/heads/"), git2::BranchType::Local) {
                            let _ = branch.delete();
                        }
                    }
                }
            }
        }
    }

    println!("Finished sub-tree history optimization.");
    Ok(Ok(()))
}
