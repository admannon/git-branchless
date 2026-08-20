//! Implementation of `git branchless optimize-history` natively using `git2` and `lib::git::Repo`.
//!
//! The whole commit graph is loaded into RAM once. All rounds, patterns, and
//! rebase decisions run against the in-memory graph, producing only virtual
//! commits. Disk writes are deferred to a single topologically-ordered
//! materialization pass at the very end (and skipped entirely for `--dry-run`).

use std::collections::{BTreeMap, HashMap, HashSet};

use git_branchless_opts::Revset;
use lib::core::effects::Effects;
use lib::git::git2::{Oid, Sort};
use lib::git::{git2, GitRunInfo, Repo};
use lib::util::{ExitCode, EyreExitOr};

/// Load every local branch and tag ref as a name -> commit oid map.
fn load_all_local_refs(
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

struct MemNode {
    tree_id: Oid,
    cdate: i64,
    message: String,
    parents: Vec<Oid>,
    children: Vec<Oid>,
    depth: usize,
    is_virtual: bool,
}

/// Commit graph held entirely in RAM. All optimization decisions happen
/// against this graph; real commits are only written at the very end.
struct InMemoryGraph {
    nodes: HashMap<Oid, MemNode>,
    refs: BTreeMap<String, Oid>,
    root_oid: Oid,
    next_virtual_seq: u128,
    virtual_clock: i64,
}

impl InMemoryGraph {
    /// Single read pass: load all refs and every commit reachable from them.
    fn new(raw_repo: &git2::Repository, root_oid: Oid) -> Result<Self, git2::Error> {
        let refs = load_all_local_refs(raw_repo)?;

        let mut nodes: HashMap<Oid, MemNode> = HashMap::new();
        let mut max_cdate = 0i64;

        let mut revwalk = raw_repo.revwalk()?;
        revwalk.set_sorting(Sort::TOPOLOGICAL)?;
        for name in refs.keys() {
            if name.starts_with("refs/tags/") {
                if let Ok(obj) = raw_repo.revparse_single(name) {
                    if let Ok(c) = obj.peel_to_commit() {
                        revwalk.push(c.id())?;
                    }
                }
            } else {
                revwalk.push_ref(name)?;
            }
        }

        let mut walked: Vec<Oid> = Vec::new();
        for oid in revwalk {
            let oid = oid?;
            let commit = raw_repo.find_commit(oid)?;
            let cdate = commit.time().seconds();
            max_cdate = max_cdate.max(cdate);
            nodes.insert(
                oid,
                MemNode {
                    tree_id: commit.tree_id(),
                    cdate,
                    message: commit.message().unwrap_or("").to_string(),
                    parents: commit.parent_ids().collect(),
                    children: Vec::new(),
                    depth: 0,
                    is_virtual: false,
                },
            );
            walked.push(oid);
        }

        // Depth: number of commits between the root and the commit (root is 0).
        // The walk is children-first, so process it in reverse so parents are
        // resolved before children.
        if let Some(root) = nodes.get_mut(&root_oid) {
            root.depth = 0;
        }
        for &oid in walked.iter().rev() {
            if oid == root_oid {
                continue;
            }
            let d = nodes[&oid]
                .parents
                .iter()
                .map(|p| nodes.get(p).map(|n| n.depth).unwrap_or(0))
                .max()
                .unwrap_or(0)
                + 1;
            nodes.get_mut(&oid).unwrap().depth = d;
        }

        // Children edges.
        let child_edges: Vec<(Oid, Oid)> = nodes
            .iter()
            .flat_map(|(&oid, n)| n.parents.iter().map(move |&p| (p, oid)))
            .collect();
        for (parent, child) in child_edges {
            if let Some(node) = nodes.get_mut(&parent) {
                node.children.push(child);
            }
        }

        Ok(Self {
            nodes,
            refs,
            root_oid,
            next_virtual_seq: 0,
            virtual_clock: max_cdate + 1,
        })
    }

    /// Allocate a unique synthetic oid for a virtual (not-yet-written) commit.
    fn new_virtual_oid(&mut self) -> Oid {
        let mut buf = [0u8; 20];
        buf[..4].copy_from_slice(&[0x76, 0x69, 0x72, 0x74]); // b"virt"
        buf[4..].copy_from_slice(&self.next_virtual_seq.to_be_bytes());
        self.next_virtual_seq += 1;
        Oid::from_bytes(&buf).unwrap()
    }

    /// True iff `a` is an ancestor of (or equal to) `b`.
    ///
    /// Walks up from `b` through its parents, which is bounded by the depth of
    /// `b` instead of the size of `a`'s whole subtree.
    fn is_ancestor(&self, a: Oid, b: Oid) -> bool {
        if a == b {
            return true;
        }
        let mut stack = vec![b];
        let mut seen = HashSet::new();
        while let Some(n) = stack.pop() {
            if n == a {
                return true;
            }
            if n == self.root_oid || !seen.insert(n) {
                continue;
            }
            if let Some(node) = self.nodes.get(&n) {
                stack.extend(node.parents.iter().copied());
            }
        }
        false
    }

    /// True iff walking parents from `tip` reaches the root.
    fn descends_from_root(&self, tip: Oid) -> bool {
        if tip == self.root_oid {
            return true;
        }
        let mut stack = vec![tip];
        let mut seen = HashSet::new();
        while let Some(n) = stack.pop() {
            if n == self.root_oid {
                return true;
            }
            if !seen.insert(n) {
                continue;
            }
            if let Some(node) = self.nodes.get(&n) {
                stack.extend(node.parents.iter().copied());
            }
        }
        false
    }

    /// Post-order (parents first) ancestor walk from `n`, excluding the root.
    ///
    /// Iterative so that deep histories cannot overflow the stack; `seen` is
    /// shared across tips so each commit is walked and emitted exactly once.
    fn collect_ancestors(&self, n: Oid, seen: &mut HashSet<Oid>, out: &mut Vec<Oid>) {
        if n == self.root_oid {
            return;
        }
        let mut stack: Vec<(Oid, bool)> = vec![(n, false)];
        while let Some((x, expanded)) = stack.pop() {
            if x == self.root_oid {
                continue;
            }
            if expanded {
                out.push(x);
                continue;
            }
            if !seen.insert(x) {
                continue;
            }
            stack.push((x, true));
            if let Some(node) = self.nodes.get(&x) {
                for &p in node.parents.iter().rev() {
                    stack.push((p, false));
                }
            }
        }
    }

    /// Commits reachable from branch refs (tags and gto-tag-tmp excluded),
    /// children-first, with the root first. Mirrors the previous revwalk.
    fn reachable(&self) -> Vec<Oid> {
        let tips: Vec<Oid> = self
            .refs
            .iter()
            .filter(|(name, _)| {
                name.starts_with("refs/heads/") && !name.starts_with("refs/heads/gto-tag-tmp/")
            })
            .map(|(_, &oid)| oid)
            .collect();
        let mut seen = HashSet::new();
        let mut acc = Vec::new();
        for tip in tips {
            self.collect_ancestors(tip, &mut seen, &mut acc);
        }
        acc.reverse();
        let mut result = Vec::with_capacity(acc.len() + 1);
        result.push(self.root_oid);
        result.extend(acc);
        result
    }

    /// Group commits by tree and pick one canonical per group, ordered by
    /// (depth, cdate, oid). Same rule as the previous implementation.
    fn select_canonicals(&self, commits: &[Oid]) -> (HashMap<Oid, Oid>, HashMap<Oid, Vec<Oid>>) {
        let mut groups: HashMap<Oid, Vec<Oid>> = HashMap::new();
        let mut info: HashMap<Oid, (usize, i64, Oid)> = HashMap::new();
        for &c in commits {
            let node = &self.nodes[&c];
            groups.entry(node.tree_id).or_default().push(c);
            info.insert(c, (node.depth, node.cdate, c));
        }
        let mut canonicals = HashMap::new();
        for members in groups.values() {
            let mut sorted_m = members.clone();
            sorted_m.sort_by(|a, b| {
                let ia = &info[a];
                let ib = &info[b];
                ia.0
                    .cmp(&ib.0)
                    .then_with(|| ia.1.cmp(&ib.1))
                    .then_with(|| ia.2.cmp(&ib.2))
            });
            let canonical = sorted_m[0];
            for &m in members {
                canonicals.insert(m, canonical);
            }
        }
        (canonicals, groups)
    }

    /// Replay the commits strictly between `old_base` and the ref tip (both
    /// inclusive ends handled like the old revwalk: tip included, old_base
    /// excluded) on top of `new_base`, creating virtual commits only.
    fn virtual_rebase(
        &mut self,
        refname: &str,
        old_base: Oid,
        new_base: Oid,
        old_to_new: &mut HashMap<Oid, Oid>,
    ) {
        let target = self.refs[refname];

        // Proper ancestors of old_base (excluding old_base itself and the
        // root): everything a replayed commit must not be.
        let mut anc_old = HashSet::new();
        {
            let mut seen = HashSet::new();
            let mut stack = vec![old_base];
            while let Some(x) = stack.pop() {
                if x == self.root_oid || !seen.insert(x) {
                    continue;
                }
                if let Some(node) = self.nodes.get(&x) {
                    for &p in &node.parents {
                        anc_old.insert(p);
                        stack.push(p);
                    }
                }
            }
        }

        // Commits strictly between old_base and the ref tip: ancestors(target)
        // minus ancestors(old_base) minus {old_base}. Pruned during the walk,
        // so the cost is bounded by the replay set, not the whole graph.
        let mut revs = Vec::new();
        {
            let mut seen = HashSet::new();
            let mut stack = vec![target];
            while let Some(x) = stack.pop() {
                if x == self.root_oid || x == old_base || anc_old.contains(&x) {
                    continue;
                }
                if !seen.insert(x) {
                    continue;
                }
                revs.push(x);
                if let Some(node) = self.nodes.get(&x) {
                    stack.extend(node.parents.iter().copied());
                }
            }
        }
        revs.sort_by_key(|r| self.nodes[r].depth);

        let mut curr_parent = new_base;
        for r in revs {
            let (tree_id, message) = {
                let node = &self.nodes[&r];
                (node.tree_id, node.message.clone())
            };
            let v = self.new_virtual_oid();
            let cdate = self.virtual_clock;
            self.virtual_clock += 1;
            let depth = self.nodes[&curr_parent].depth + 1;
            if let Some(pnode) = self.nodes.get_mut(&curr_parent) {
                pnode.children.push(v);
            }
            self.nodes.insert(
                v,
                MemNode {
                    tree_id,
                    cdate,
                    message,
                    parents: vec![curr_parent],
                    children: Vec::new(),
                    depth,
                    is_virtual: true,
                },
            );
            old_to_new.insert(r, v);
            curr_parent = v;
        }
        self.refs.insert(refname.to_string(), curr_parent);
    }

    /// Commit -> branches (ref name, tip) whose ancestry contains it, for
    /// non-temp branch refs only. Builds once per pass in O(branches * depth)
    /// so that per-dup branch lookups become O(1) instead of rescanning every
    /// branch against every duplicate commit.
    fn descending_branches(&self) -> HashMap<Oid, Vec<(String, Oid)>> {
        let mut map: HashMap<Oid, Vec<(String, Oid)>> = HashMap::new();
        let branch_refs: Vec<(String, Oid)> = self
            .refs
            .iter()
            .filter(|(rname, _)| {
                rname.starts_with("refs/heads/")
                    && !rname.starts_with("refs/heads/gto-keep/")
                    && !rname.starts_with("refs/heads/gto/")
            })
            .map(|(r, &o)| (r.clone(), o))
            .collect();
        for (rname, tip) in branch_refs {
            let mut seen = HashSet::new();
            let mut stack = vec![tip];
            while let Some(n) = stack.pop() {
                if n == self.root_oid || !seen.insert(n) {
                    continue;
                }
                map.entry(n).or_default().push((rname.clone(), tip));
                if let Some(node) = self.nodes.get(&n) {
                    stack.extend(node.parents.iter().copied());
                }
            }
        }
        map
    }
}

/// Collect virtual commits reachable from the final refs, parents first.
fn collect_virtual_commits(
    oid: Oid,
    graph: &InMemoryGraph,
    visited: &mut HashSet<Oid>,
    order: &mut Vec<Oid>,
) {
    if !visited.insert(oid) {
        return;
    }
    let Some(node) = graph.nodes.get(&oid) else {
        return;
    };
    for &p in &node.parents {
        collect_virtual_commits(p, graph, visited, order);
    }
    if node.is_virtual {
        order.push(oid);
    }
}

/// Single deferred write pass: create every still-reachable virtual commit in
/// topological order, then update every remaining ref once.
fn materialize_commits_and_refs(
    raw_repo: &git2::Repository,
    graph: &InMemoryGraph,
    final_refs: &BTreeMap<String, Oid>,
) -> Result<(), git2::Error> {
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    for tip in final_refs.values() {
        collect_virtual_commits(*tip, graph, &mut visited, &mut order);
    }

    let signature = raw_repo.signature()?;
    let mut created: HashMap<Oid, Oid> = HashMap::new();
    for &v in &order {
        let node = &graph.nodes[&v];
        let parents: Vec<Oid> = node
            .parents
            .iter()
            .map(|p| created.get(p).copied().unwrap_or(*p))
            .collect();
        let parent_commits: Vec<git2::Commit> = parents
            .iter()
            .map(|p| raw_repo.find_commit(*p))
            .collect::<Result<Vec<_>, _>>()?;
        let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();
        let tree = raw_repo.find_tree(node.tree_id)?;
        let real = raw_repo.commit(
            None,
            &signature,
            &signature,
            &node.message,
            &tree,
            &parent_refs,
        )?;
        created.insert(v, real);
    }

    for (name, &tip) in final_refs {
        let real = created.get(&tip).copied().unwrap_or(tip);
        raw_repo.reference(name, real, true, "optimize_history")?;
    }
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

    // Load the whole commit graph into memory in a single read pass.
    let mut graph = InMemoryGraph::new(&raw_repo, root_oid)?;

    // Keep only refs that descend from root.
    let keep_refs: HashSet<Oid> = graph
        .refs
        .values()
        .copied()
        .filter(|&tip| graph.descends_from_root(tip))
        .collect();
    graph.refs.retain(|_, tip| keep_refs.contains(tip));
    if graph.refs.is_empty() {
        eprintln!("error: no local refs descend from root {root_ref_str} ({root_oid})");
        return Ok(Err(ExitCode(1)));
    }

    println!("Optimizing sub-tree history above root {root_ref_str} ({root_oid})...");

    let initial_tags: BTreeMap<String, Oid> = graph
        .refs
        .iter()
        .filter(|(r, _)| r.starts_with("refs/tags/"))
        .map(|(r, &o)| (r.clone(), o))
        .collect();

    let mut dup_targets = HashMap::new();
    {
        let initial_commits = graph.reachable();
        let (initial_canonicals, initial_tree_groups) = graph.select_canonicals(&initial_commits);
        for members in initial_tree_groups.values() {
            if members.len() > 1 {
                for &m in members {
                    if m != initial_canonicals[&m] {
                        dup_targets.insert(m, initial_canonicals[&m]);
                    }
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
            return Ok(Err(ExitCode(1)));
        }

        let reachable = graph.reachable();
        let proc_commits: Vec<Oid> = reachable.iter().copied().filter(|&c| c != root_oid).collect();
        if proc_commits.is_empty() {
            println!("No reachable commits to optimize.");
            break;
        }

        let (round_cans, tree_groups) = graph.select_canonicals(&reachable);

        // Step 0: branch the duplicates that lack a branch ref
        let mut head_refs: HashMap<Oid, Vec<String>> = HashMap::new();
        for (rname, &roid) in &graph.refs {
            if rname.starts_with("refs/heads/") {
                head_refs.entry(roid).or_default().push(rname.clone());
            }
        }

        let mut added_temp = false;
        for members in tree_groups.values() {
            if members.len() > 1 {
                for &m in members {
                    if m == root_oid {
                        continue;
                    }
                    if !head_refs.contains_key(&m) || head_refs[&m].is_empty() {
                        let temp_ref = format!("refs/heads/gto/{m}");
                        println!("Step 0: creating temporary branch {temp_ref} for duplicate commit {m}");
                        if !dry_run {
                            graph.refs.insert(temp_ref.clone(), m);
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
            let current_commits = graph.reachable();
            let proc_c: Vec<Oid> = current_commits.into_iter().filter(|&c| c != root_oid).collect();
            let descending = graph.descending_branches();
            let mut rebased_this_pass: HashSet<String> = HashSet::new();

            for &c_prime in &proc_c {
                let Some(&canonical_c) = round_cans.get(&c_prime) else {
                    continue;
                };
                if c_prime == canonical_c {
                    continue;
                }
                if !graph.is_ancestor(canonical_c, c_prime) {
                    let keep_ref = format!("refs/heads/gto-keep/{c_prime}");
                    if !dry_run && !graph.refs.contains_key(&keep_ref) {
                        graph.refs.insert(keep_ref.clone(), c_prime);
                    }
                    plan_actions.push(format!("Pattern 1: preserve chain {keep_ref} at {c_prime}"));

                    if let Some(branch_list) = descending.get(&c_prime) {
                        for (rname, roid) in branch_list {
                            if c_prime != *roid && !rebased_this_pass.contains(rname) {
                                println!("Round {round_num} Pass 1 (pass {p1_pass}): rebasing {rname} onto canonical {canonical_c}");
                                plan_actions.push(format!("Pattern 1: rebase {rname} onto {canonical_c} (from {c_prime})"));

                                if !dry_run {
                                    graph.virtual_rebase(rname, c_prime, canonical_c, &mut old_to_new);
                                    rebased_this_pass.insert(rname.clone());
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
            let current_commits = graph.reachable();
            let proc_c: Vec<Oid> = current_commits.into_iter().filter(|&c| c != root_oid).collect();
            let descending = graph.descending_branches();
            let mut rebased_this_pass: HashSet<String> = HashSet::new();

            for &y in &proc_c {
                let Some(&canonical_c) = round_cans.get(&y) else {
                    continue;
                };
                if y == canonical_c {
                    continue;
                }
                if graph.is_ancestor(canonical_c, y) {
                    let keep_ref = format!("refs/heads/gto-keep/{y}");
                    if !dry_run && !graph.refs.contains_key(&keep_ref) {
                        graph.refs.insert(keep_ref.clone(), y);
                    }
                    plan_actions.push(format!("Pattern 2: preserve chain {keep_ref} at {y}"));

                    if let Some(branch_list) = descending.get(&y) {
                        for (rname, roid) in branch_list {
                            if y != *roid && !rebased_this_pass.contains(rname) {
                                println!("Round {round_num} Pass 2 (pass {p2_pass}): shallowing {rname} onto canonical {canonical_c}");
                                plan_actions.push(format!("Pattern 2: rebase {rname} onto {canonical_c} (from {y})"));

                                if !dry_run {
                                    graph.virtual_rebase(rname, y, canonical_c, &mut old_to_new);
                                    rebased_this_pass.insert(rname.clone());
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
                graph.refs.insert(tag_ref.clone(), final_target);
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
    }

    // Materialize: drop gto/* temp refs, create any virtual commits that are
    // still reachable, and write every remaining ref in a single pass.
    if !dry_run {
        println!("Cleaning up temporary branches...");
        let final_refs: BTreeMap<String, Oid> = graph
            .refs
            .iter()
            .filter(|(rname, _)| {
                !(rname.starts_with("refs/heads/gto/") && !rname.starts_with("refs/heads/gto-keep/"))
            })
            .map(|(r, &o)| (r.clone(), o))
            .collect();
        materialize_commits_and_refs(&raw_repo, &graph, &final_refs)?;
    }

    println!("Finished sub-tree history optimization.");
    Ok(Ok(()))
}