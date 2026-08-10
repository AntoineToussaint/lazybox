//! Stacked-PR detection.
//!
//! A *stack* is a chain of open PRs where each PR targets the head
//! branch of the PR below it instead of the repo's default branch —
//! the pattern Graphite / `ghstack` / `spr` produce (A ← B ← C). The
//! link is purely branch-shaped: PR `B` is stacked on PR `A` when
//! `B.base_branch == A.branch` (A's head) and both are open PRs in the
//! same repo. That makes detection tool-agnostic — no Graphite/`ghstack`
//! metadata required.
//!
//! [`detect_stacks`] takes every in-scope PR and returns, for each PR
//! that participates in a stack, its [`StackPosition`]: the parent it
//! sits on, the children sitting on it, and where it falls in the chain
//! (`position` / `depth`, rendered as `2/3`). PRs based directly on the
//! default branch with nothing stacked on them are omitted — they are
//! not part of any stack.
//!
//! Detection degrades gracefully: if a parent PR isn't in view (never
//! fetched, out of scope), its children simply have `parent == None` and
//! are treated as chain roots. The chain is only ever as complete as the
//! PRs actually in scope.

use crate::{Task, TaskId};
use std::collections::HashMap;

/// A PR's place within a detected stack. Produced by [`detect_stacks`],
/// one entry per PR that is linked to at least one other PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackPosition {
    /// The parent PR this one is stacked on — the open PR whose head
    /// branch equals this PR's base branch. `None` when the base is the
    /// repo default (bottom of the stack) or the parent isn't in scope.
    pub parent: Option<TaskId>,
    /// PRs stacked directly on this one (their base == this PR's head),
    /// in a stable order (by task id).
    pub children: Vec<TaskId>,
    /// 1-based position measured from the bottom of the chain — the
    /// bottom-most PR is `1`, the one stacked on it is `2`, and so on.
    pub position: usize,
    /// Height of the whole stack this PR belongs to — the longest
    /// root-to-leaf chain in its tree. This is a per-stack constant, so
    /// two PRs branching off the same parent report the same `N` in
    /// `k/N` rather than each measuring only the chain through itself.
    pub depth: usize,
}

/// Link every in-scope PR to the parent whose head branch is that PR's
/// base branch, building the chain transitively. Returns a map keyed by
/// PR [`TaskId`], containing only PRs that participate in a stack (have a
/// parent or at least one child).
///
/// Non-PR tasks, closed/merged PRs, and PRs missing a repo/head/base are
/// ignored. Base→head matching is scoped per-repo. Cycles (which a
/// well-formed stack never contains, but a mislinked base could) are
/// broken so traversal always terminates.
pub fn detect_stacks<'a>(
    tasks: impl IntoIterator<Item = &'a Task>,
) -> HashMap<TaskId, StackPosition> {
    // Only open PRs with the branch fields we need to match on.
    let prs: Vec<&Task> = tasks
        .into_iter()
        .filter(|t| is_open_pr(t) && t.repo.is_some() && t.branch.is_some())
        .collect();

    // (repo, head branch) → PR id. A head branch uniquely identifies an
    // open PR within a repo, so this is the reverse index base→parent
    // matching consults.
    let mut by_head: HashMap<(&str, &str), &TaskId> = HashMap::new();
    for pr in &prs {
        let repo = pr.repo.as_deref().expect("filtered to repo.is_some()");
        let head = pr.branch.as_deref().expect("filtered to branch.is_some()");
        by_head.insert((repo, head), &pr.id);
    }

    // Direct parent of each PR: the PR whose head == this PR's base.
    let mut parent: HashMap<&TaskId, &TaskId> = HashMap::new();
    let mut children: HashMap<&TaskId, Vec<&TaskId>> = HashMap::new();
    for pr in &prs {
        let repo = pr.repo.as_deref().expect("filtered to repo.is_some()");
        let Some(base) = pr.base_branch.as_deref() else {
            continue;
        };
        if let Some(&parent_id) = by_head.get(&(repo, base))
            && parent_id != &pr.id
        {
            parent.insert(&pr.id, parent_id);
            children.entry(parent_id).or_default().push(&pr.id);
        }
    }
    for kids in children.values_mut() {
        kids.sort_by(|a, b| (&a.source, &a.key).cmp(&(&b.source, &b.key)));
    }

    let mut out = HashMap::new();
    for pr in &prs {
        let id = &pr.id;
        let has_parent = parent.contains_key(id);
        let kids = children.get(id).cloned().unwrap_or_default();
        if !has_parent && kids.is_empty() {
            continue; // standalone PR — not part of any stack
        }
        out.insert(
            (*id).clone(),
            StackPosition {
                parent: parent.get(id).map(|p| (*p).clone()),
                children: kids.iter().map(|c| (*c).clone()).collect(),
                position: position_from_bottom(id, &parent),
                depth: chain_depth(id, &parent, &children),
            },
        );
    }
    out
}

/// An open PR is one that hasn't reached a terminal (merged/closed)
/// state — the only PRs that can still take part in a live stack.
fn is_open_pr(task: &Task) -> bool {
    use crate::TaskState::*;
    task.is_pr() && matches!(task.state, Open | InProgress | InReview | Draft)
}

/// Walk parent links down to the bottom of the chain, counting hops.
/// Cycle-safe: a repeated id ends the walk.
fn position_from_bottom(id: &TaskId, parent: &HashMap<&TaskId, &TaskId>) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut cur = id;
    let mut pos = 1;
    seen.insert(cur);
    while let Some(&p) = parent.get(cur) {
        if !seen.insert(p) {
            break;
        }
        pos += 1;
        cur = p;
    }
    pos
}

/// Height of the whole tree `id` belongs to: walk to the root, then
/// measure its tallest root-to-leaf chain. Constant for every node in a
/// tree, so siblings share the same `N` in `k/N`. Cycle-safe.
fn chain_depth<'a>(
    id: &'a TaskId,
    parent: &HashMap<&'a TaskId, &'a TaskId>,
    children: &HashMap<&'a TaskId, Vec<&'a TaskId>>,
) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut root = id;
    seen.insert(root);
    while let Some(&p) = parent.get(root) {
        if !seen.insert(p) {
            break;
        }
        root = p;
    }
    subtree_height(root, children, &mut std::collections::HashSet::new())
}

/// Number of PRs in the tallest descendant chain including `id` itself
/// (a leaf is height 1). Cycle-safe via the visited set.
fn subtree_height<'a>(
    id: &'a TaskId,
    children: &HashMap<&'a TaskId, Vec<&'a TaskId>>,
    seen: &mut std::collections::HashSet<&'a TaskId>,
) -> usize {
    if !seen.insert(id) {
        return 0;
    }
    let tallest = children
        .get(id)
        .into_iter()
        .flatten()
        .map(|c| subtree_height(c, children, seen))
        .max()
        .unwrap_or(0);
    seen.remove(id);
    1 + tallest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CiStatus, Mergeable, ReviewStatus, TaskKind, TaskRole, TaskState};

    fn pr(repo: &str, num: u64, head: &str, base: &str) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: format!("{repo}#{num}"),
            },
            title: format!("PR {num}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{repo}/pull/{num}"),
            repo: Some(repo.into()),
            branch: Some(head.into()),
            base_branch: Some(base.into()),
            updated_at: chrono::Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
            linked_tasks: vec![],
            priority: None,
            state_label: None,
        }
    }

    fn id(repo: &str, num: u64) -> TaskId {
        TaskId {
            source: "github".into(),
            key: format!("{repo}#{num}"),
        }
    }

    #[test]
    fn base_equals_head_builds_the_chain() {
        // A(main) ← B(feat-a) ← C(feat-b)
        let a = pr("o/r", 1, "feat-a", "main");
        let b = pr("o/r", 2, "feat-b", "feat-a");
        let c = pr("o/r", 3, "feat-c", "feat-b");
        let stacks = detect_stacks([&a, &b, &c]);

        assert_eq!(stacks.len(), 3);

        let sa = &stacks[&id("o/r", 1)];
        assert_eq!(sa.parent, None);
        assert_eq!(sa.children, vec![id("o/r", 2)]);
        assert_eq!((sa.position, sa.depth), (1, 3));

        let sb = &stacks[&id("o/r", 2)];
        assert_eq!(sb.parent, Some(id("o/r", 1)));
        assert_eq!(sb.children, vec![id("o/r", 3)]);
        assert_eq!((sb.position, sb.depth), (2, 3));

        let sc = &stacks[&id("o/r", 3)];
        assert_eq!(sc.parent, Some(id("o/r", 2)));
        assert!(sc.children.is_empty());
        assert_eq!((sc.position, sc.depth), (3, 3));
    }

    #[test]
    fn standalone_pr_is_not_in_a_stack() {
        let a = pr("o/r", 1, "feat-a", "main");
        let stacks = detect_stacks([&a]);
        assert!(stacks.is_empty());
    }

    #[test]
    fn parent_out_of_scope_degrades_to_root() {
        // Only the child B is in view; its base (feat-a) matches no
        // in-scope head, so it reads as a lone root — no link.
        let b = pr("o/r", 2, "feat-b", "feat-a");
        let stacks = detect_stacks([&b]);
        assert!(stacks.is_empty());
    }

    #[test]
    fn merged_parent_is_ignored() {
        let mut a = pr("o/r", 1, "feat-a", "main");
        a.state = TaskState::Merged;
        let b = pr("o/r", 2, "feat-b", "feat-a");
        let stacks = detect_stacks([&a, &b]);
        // A is terminal, so B has no in-scope parent and stands alone.
        assert!(stacks.is_empty());
    }

    #[test]
    fn matching_is_scoped_per_repo() {
        // Same branch names in two repos must not cross-link.
        let a = pr("o/r1", 1, "feat-a", "main");
        let b = pr("o/r2", 2, "feat-b", "feat-a");
        let stacks = detect_stacks([&a, &b]);
        assert!(stacks.is_empty());
    }

    #[test]
    fn branching_stack_reports_tree_height_as_depth() {
        // A ← B, A ← C, C ← D. The tree height is 3 (chain A-C-D), and
        // `depth` is that height for EVERY node — so siblings B and C
        // both read `2/3`, not `2/2` vs `2/3`.
        let a = pr("o/r", 1, "feat-a", "main");
        let b = pr("o/r", 2, "feat-b", "feat-a");
        let c = pr("o/r", 3, "feat-c", "feat-a");
        let d = pr("o/r", 4, "feat-d", "feat-c");
        let stacks = detect_stacks([&a, &b, &c, &d]);

        let sa = &stacks[&id("o/r", 1)];
        assert_eq!(sa.children, vec![id("o/r", 2), id("o/r", 3)]);
        assert_eq!((sa.position, sa.depth), (1, 3));

        // Siblings off the same parent share N (the tree height).
        let sb = &stacks[&id("o/r", 2)];
        assert_eq!((sb.position, sb.depth), (2, 3));
        let sc = &stacks[&id("o/r", 3)];
        assert_eq!((sc.position, sc.depth), (2, 3));

        let sd = &stacks[&id("o/r", 4)];
        assert_eq!((sd.position, sd.depth), (3, 3));
    }

    #[test]
    fn cycle_does_not_hang() {
        // Malformed: A bases on B's head and B bases on A's head.
        let a = pr("o/r", 1, "feat-a", "feat-b");
        let b = pr("o/r", 2, "feat-b", "feat-a");
        let stacks = detect_stacks([&a, &b]);
        // Both link to each other; traversal terminates with finite values.
        assert_eq!(stacks.len(), 2);
        assert!(stacks[&id("o/r", 1)].position >= 1);
        assert!(stacks[&id("o/r", 2)].position >= 1);
    }
}
