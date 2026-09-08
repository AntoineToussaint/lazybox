//! Pure layered layout for the epic dependency graph (`E g`, #1524).
//!
//! Ratatui-free so it lives here, tested in isolation, and rendered by
//! `lazybox-tui`. [`layout`] turns an [`EpicSnapshot`] into a grid of
//! tone-tagged text spans: waves become columns (left → right), members
//! stack within their wave, and the typed edges are drawn as orthogonal
//! connectors between columns — solid for a `Blocks` dependency, dashed
//! (`╌`/`╎`) for a `MergeAfter`-only edge.
//!
//! The layout is deterministic given `(snapshot, width, selected)` and
//! also returns the per-column member ordering the modal navigates with
//! (`j/k` within a column, `h/l` across).

use lazybox_core::WorkspaceKey;
use lazybox_ipc::{BlockerKind, EdgeKind, EpicMember, EpicMemberStatus, EpicSnapshot};
use std::collections::HashMap;

/// Semantic tone for a laid-out span. The render crate maps each to a
/// concrete `ratatui` style; keeping it abstract here is what lets the
/// layout stay ratatui-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Box-drawing connector for a `Blocks` edge.
    Edge,
    /// Box-drawing connector for a `MergeAfter`-only edge (dashed).
    MergeAfterEdge,
    /// A node at rest, keyed by its derived status.
    NodeDone,
    NodeFailed,
    NodeAsking,
    NodeActive,
    NodeHeld,
    NodeWaiting,
    /// A node caught in a dependency cycle — it cannot be ordered into a wave,
    /// so the layered layout would otherwise render it as an innocuous root.
    NodeCycle,
    /// The node under the cursor.
    Selected,
}

/// Whether a member is caught in a dependency cycle. `waves()` cannot order
/// cycle members, so the daemon flags each with a [`BlockerKind::Cycle`]
/// blocker; the layered layout collapses them all into wave 0 where they'd read
/// as independent roots, so they must be drawn distinctly.
pub fn member_in_cycle(m: &EpicMember) -> bool {
    m.blockers.iter().any(|b| b.kind == BlockerKind::Cycle)
}

/// One run of same-tone text on a rendered row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSpan {
    pub text: String,
    pub tone: Tone,
}

/// One rendered row, left to right.
pub type GraphLine = Vec<GraphSpan>;

/// The result of laying out a graph: the rendered art plus the
/// navigation model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DagLayout {
    /// Rendered rows, top to bottom.
    pub lines: Vec<GraphLine>,
    /// Member indices (into [`EpicSnapshot::members`]) per wave-column,
    /// top to bottom — the grid the modal navigates.
    pub columns: Vec<Vec<usize>>,
}

// Direction bits for the edge char-grid.
const L: u8 = 1;
const R: u8 = 2;
const U: u8 = 4;
const D: u8 = 8;

const GUTTER: usize = 4;
const ROW_STRIDE: usize = 2;
const MIN_NODE_W: usize = 10;
const MAX_NODE_W: usize = 40;

/// `github:owner/repo#123` → `owner/repo#123` (drop the provider
/// prefix; the graph is dense enough without it).
fn short_key(k: &WorkspaceKey) -> String {
    match k.0.split_once(':') {
        Some((_provider, rest)) => rest.to_string(),
        None => k.0.clone(),
    }
}

/// The status glyph + tone for a member node.
fn node_glyph(status: &EpicMemberStatus) -> (&'static str, Tone) {
    match status {
        EpicMemberStatus::Done => ("✓", Tone::NodeDone),
        EpicMemberStatus::Failed => ("✗", Tone::NodeFailed),
        EpicMemberStatus::Asking => ("?", Tone::NodeAsking),
        EpicMemberStatus::InProgress => ("●", Tone::NodeActive),
        EpicMemberStatus::Mergeable { held_by } if !held_by.is_empty() => ("⏸", Tone::NodeHeld),
        EpicMemberStatus::Mergeable { .. } => ("▶", Tone::NodeActive),
        EpicMemberStatus::PrOpen { .. } => ("○", Tone::NodeActive),
        EpicMemberStatus::Claimed => ("◦", Tone::NodeWaiting),
        EpicMemberStatus::Blocked => ("⊘", Tone::NodeWaiting),
        EpicMemberStatus::Ready => ("·", Tone::NodeWaiting),
    }
}

/// Truncate to `w` display columns, appending `…` when clipped.
fn truncate(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = chars[..w.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

/// Bitmask → box-drawing glyph. Junctions emerge from OR-ing the
/// direction bits of overlapping edges.
fn glyph(mask: u8) -> char {
    match mask {
        m if m == L | R => '─',
        m if m == U | D => '│',
        m if m == D | R => '┌',
        m if m == D | L => '┐',
        m if m == U | R => '└',
        m if m == U | L => '┘',
        m if m == L | R | D => '┬',
        m if m == L | R | U => '┴',
        m if m == U | D | R => '├',
        m if m == U | D | L => '┤',
        m if m == U | D | L | R => '┼',
        m if m & (L | R) != 0 => '─',
        m if m & (U | D) != 0 => '│',
        _ => ' ',
    }
}

/// The dashed counterpart for the straight `MergeAfter` segments; bends
/// reuse the solid glyph (there are no dashed corner glyphs).
fn dashed_glyph(mask: u8) -> char {
    match mask {
        m if m == L | R => '╌',
        m if m == U | D => '╎',
        other => glyph(other),
    }
}

/// Lay out `snapshot` into `width` columns of terminal cells.
/// `selected` (a member index) is drawn with [`Tone::Selected`].
pub fn layout(snapshot: &EpicSnapshot, width: u16, selected: Option<usize>) -> DagLayout {
    if snapshot.members.is_empty() {
        return DagLayout::default();
    }

    // Waves may be sparse; map the distinct wave values to dense
    // column indices 0..W in ascending order.
    let mut waves: Vec<u16> = snapshot.members.iter().map(|m| m.wave).collect();
    waves.sort_unstable();
    waves.dedup();
    let col_of_wave: HashMap<u16, usize> = waves.iter().enumerate().map(|(c, &w)| (w, c)).collect();
    let n_cols = waves.len();

    // Members per column, in snapshot order (stable), plus each
    // member's (col, row) position.
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); n_cols];
    let mut pos: Vec<(usize, usize)> = vec![(0, 0); snapshot.members.len()];
    let mut key_to_idx: HashMap<&WorkspaceKey, usize> = HashMap::new();
    for (i, m) in snapshot.members.iter().enumerate() {
        let c = col_of_wave[&m.wave];
        let r = columns[c].len();
        columns[c].push(i);
        pos[i] = (c, r);
        key_to_idx.insert(&m.key, i);
    }
    let max_rows = columns.iter().map(Vec::len).max().unwrap_or(0);

    // Size the node boxes to the width budget.
    let node_w = match (width as usize + GUTTER).checked_div(n_cols) {
        Some(w) => w.saturating_sub(GUTTER).clamp(MIN_NODE_W, MAX_NODE_W),
        None => MIN_NODE_W,
    };
    let col_stride = node_w + GUTTER;
    let grid_w = n_cols * col_stride;
    let grid_h = (max_rows.saturating_sub(1)) * ROW_STRIDE + 1;
    let col_x = |c: usize| c * col_stride;
    let row_y = |r: usize| r * ROW_STRIDE;

    // Collapse edges by (from,to): a pair with a `Blocks` edge is
    // solid, a `MergeAfter`-only pair is dashed.
    let mut pair_solid: HashMap<(usize, usize), bool> = HashMap::new();
    for e in &snapshot.edges {
        let (Some(&fi), Some(&ti)) = (key_to_idx.get(&e.from), key_to_idx.get(&e.to)) else {
            continue;
        };
        let solid = matches!(e.kind, EdgeKind::Blocks);
        pair_solid
            .entry((fi, ti))
            .and_modify(|s| *s |= solid)
            .or_insert(solid);
    }

    // Route edges into a direction-bit grid, tracking which cells any
    // solid edge touched (solid wins the glyph over a dashed overlap).
    let mut mask = vec![0u8; grid_w * grid_h];
    let mut solid_cell = vec![false; grid_w * grid_h];
    let mut add = |y: usize, x: usize, bits: u8, is_solid: bool| {
        if y < grid_h && x < grid_w {
            mask[y * grid_w + x] |= bits;
            if is_solid {
                solid_cell[y * grid_w + x] = true;
            }
        }
    };
    for (&(fi, ti), &is_solid) in &pair_solid {
        // `from` depends on `to`; `to` is the earlier (left) node.
        let (a, b) = (ti, fi);
        let (ca, ra) = pos[a];
        let (cb, rb) = pos[b];
        let (left, lr, right, rr) = if ca <= cb {
            (ca, ra, cb, rb)
        } else {
            (cb, rb, ca, ra)
        };
        if left == right {
            continue; // same wave — no horizontal corridor to route in.
        }
        let y_from = row_y(lr);
        let y_to = row_y(rr);
        let x0 = col_x(left) + node_w; // first gutter cell right of the left box
        let x_turn = col_x(right).saturating_sub(1); // cell just left of the right box
        if y_from == y_to {
            for x in x0..=x_turn {
                add(y_from, x, L | R, is_solid);
            }
        } else {
            for x in x0..x_turn {
                add(y_from, x, L | R, is_solid);
            }
            // Top bend, vertical run, bottom bend into the right box.
            let down = y_to > y_from;
            add(y_from, x_turn, L | if down { D } else { U }, is_solid);
            let (ylo, yhi) = (y_from.min(y_to), y_from.max(y_to));
            for y in ylo + 1..yhi {
                add(y, x_turn, U | D, is_solid);
            }
            add(y_to, x_turn, R | if down { U } else { D }, is_solid);
        }
    }

    // Compose the char grid: edges first, node text on top.
    let mut chars: Vec<(char, Tone)> = vec![(' ', Tone::Edge); grid_w * grid_h];
    for y in 0..grid_h {
        for x in 0..grid_w {
            let m = mask[y * grid_w + x];
            if m == 0 {
                continue;
            }
            let (ch, tone) = if solid_cell[y * grid_w + x] {
                (glyph(m), Tone::Edge)
            } else {
                (dashed_glyph(m), Tone::MergeAfterEdge)
            };
            chars[y * grid_w + x] = (ch, tone);
        }
    }
    for (i, m) in snapshot.members.iter().enumerate() {
        let (c, r) = pos[i];
        let (g, node_tone) = node_glyph(&m.status);
        // A cycle member reads as a wave-0 root in a layered layout; recolor it
        // so the cycle is visible at the node even without following an edge.
        let node_tone = if member_in_cycle(m) {
            Tone::NodeCycle
        } else {
            node_tone
        };
        let tone = if Some(i) == selected {
            Tone::Selected
        } else {
            node_tone
        };
        let label = truncate(&format!("{g} {}", short_key(&m.key)), node_w);
        let y = row_y(r);
        let x0 = col_x(c);
        for (dx, ch) in label.chars().enumerate() {
            let x = x0 + dx;
            if x < grid_w && y < grid_h {
                chars[y * grid_w + x] = (ch, tone);
            }
        }
    }

    // Run-length group each row into same-tone spans, dropping the
    // trailing blank tail.
    let mut lines = Vec::with_capacity(grid_h);
    for y in 0..grid_h {
        let mut spans: GraphLine = Vec::new();
        let mut cur = String::new();
        let mut cur_tone = Tone::Edge;
        for x in 0..grid_w {
            let (ch, tone) = chars[y * grid_w + x];
            if cur.is_empty() {
                cur.push(ch);
                cur_tone = tone;
            } else if tone == cur_tone {
                cur.push(ch);
            } else {
                spans.push(GraphSpan {
                    text: std::mem::take(&mut cur),
                    tone: cur_tone,
                });
                cur.push(ch);
                cur_tone = tone;
            }
        }
        if !cur.is_empty() {
            spans.push(GraphSpan {
                text: cur,
                tone: cur_tone,
            });
        }
        // Trim a trailing all-space span so rows don't carry filler.
        if let Some(last) = spans.last()
            && last.text.trim().is_empty()
        {
            spans.pop();
        }
        lines.push(spans);
    }

    DagLayout { lines, columns }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_ipc::{Blocker, BlockerOwner, EpicEdge, EpicMember};

    fn key(s: &str) -> WorkspaceKey {
        WorkspaceKey(format!("github:{s}"))
    }

    fn member(k: &str, wave: u16, status: EpicMemberStatus) -> EpicMember {
        EpicMember {
            key: key(k),
            wave,
            status,
            blocked_by: vec![],
            external_blockers: vec![],
            blockers: vec![],
        }
    }

    /// A member the daemon flagged as caught in a dependency cycle.
    fn cycle_member(k: &str, status: EpicMemberStatus) -> EpicMember {
        let mut m = member(k, 0, status);
        m.blockers = vec![Blocker {
            kind: BlockerKind::Cycle,
            reason: "dependency cycle".into(),
            owner: BlockerOwner::Operator,
            since: 0,
            holds: 0,
        }];
        m
    }

    fn snap(members: Vec<EpicMember>, edges: Vec<EpicEdge>) -> EpicSnapshot {
        EpicSnapshot {
            key: "epic".into(),
            name: "Epic".into(),
            members,
            done: 0,
            total: 0,
            ready: 0,
            blocked: 0,
            asking: 0,
            failing: 0,
            blockers_needing_operator: 0,
            cycle: false,
            critical_path: vec![],
            edges,
            merge_order: vec![],
            computed_at: 0,
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> EpicEdge {
        EpicEdge {
            from: key(from),
            to: key(to),
            kind,
        }
    }

    /// Render the layout back to a plain-text grid for text assertions.
    fn to_text(dag: &DagLayout) -> String {
        dag.lines
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn layout_places_members_by_wave() {
        let members = vec![
            member("o/r#1", 0, EpicMemberStatus::Done),
            member("o/r#2", 1, EpicMemberStatus::Ready),
            member("o/r#3", 1, EpicMemberStatus::Ready),
            member("o/r#4", 2, EpicMemberStatus::Ready),
        ];
        let dag = layout(&snap(members, vec![]), 120, None);
        // Three waves → three columns; wave 1 holds two stacked members.
        assert_eq!(dag.columns.len(), 3);
        assert_eq!(dag.columns[0], vec![0]);
        assert_eq!(dag.columns[1], vec![1, 2]);
        assert_eq!(dag.columns[2], vec![3]);

        // Column 0's node sits left of column 2's node on the top row.
        let text = to_text(&dag);
        let first = text.lines().next().unwrap();
        let p1 = first.find("o/r#1").expect("wave 0 node on row 0");
        let p4 = first.find("o/r#4").expect("wave 2 node on row 0");
        assert!(p1 < p4, "waves ascend left→right: {first:?}");
    }

    #[test]
    fn layout_routes_skip_wave_edges() {
        // #1 (wave 0) → #3 (wave 2), skipping wave 1. The connector must
        // span both corridors, reaching the far column's gutter.
        let members = vec![
            member("o/r#1", 0, EpicMemberStatus::Done),
            member("o/r#2", 1, EpicMemberStatus::Ready),
            member("o/r#3", 2, EpicMemberStatus::Ready),
        ];
        let edges = vec![edge("o/r#3", "o/r#1", EdgeKind::Blocks)];
        let dag = layout(&snap(members, edges), 120, None);
        let text = to_text(&dag);
        // A horizontal connector exists on the top row between the nodes.
        let first = text.lines().next().unwrap();
        assert!(first.contains('─'), "skip-wave connector drawn: {first:?}");
        // It reaches past wave 1: the connector's rightmost box char sits
        // to the right of wave 1's column start.
        let bar = first.rfind('─').unwrap();
        let mid = first.find("o/r#2").expect("wave 1 node present");
        assert!(bar > mid, "connector routes past the intervening wave");
    }

    #[test]
    fn merge_after_only_edges_render_dashed() {
        let members = vec![
            member("o/r#1", 0, EpicMemberStatus::Done),
            member("o/r#2", 1, EpicMemberStatus::Ready),
        ];
        // Only a MergeAfter edge, no Blocks → dashed connector.
        let edges = vec![edge("o/r#2", "o/r#1", EdgeKind::MergeAfter)];
        let dag = layout(&snap(members, edges), 120, None);
        let text = to_text(&dag);
        assert!(text.contains('╌'), "merge-after edge is dashed: {text:?}");
        assert!(!text.contains('─'), "no solid segment for a dashed edge");
    }

    #[test]
    fn blocks_edge_wins_over_implied_merge_after_as_solid() {
        let members = vec![
            member("o/r#1", 0, EpicMemberStatus::Done),
            member("o/r#2", 1, EpicMemberStatus::Ready),
        ];
        // Both edges present for the same pair (Blocks implies MergeAfter):
        // the connector is solid.
        let edges = vec![
            edge("o/r#2", "o/r#1", EdgeKind::Blocks),
            edge("o/r#2", "o/r#1", EdgeKind::MergeAfter),
        ];
        let dag = layout(&snap(members, edges), 120, None);
        let text = to_text(&dag);
        assert!(text.contains('─'), "solid connector: {text:?}");
        assert!(!text.contains('╌'), "not dashed when Blocks present");
    }

    #[test]
    fn selected_member_carries_selected_tone() {
        let members = vec![
            member("o/r#1", 0, EpicMemberStatus::Done),
            member("o/r#2", 1, EpicMemberStatus::Ready),
        ];
        let dag = layout(&snap(members, vec![]), 120, Some(1));
        let selected: Vec<&str> = dag
            .lines
            .iter()
            .flat_map(|l| l.iter())
            .filter(|s| s.tone == Tone::Selected)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            selected.iter().any(|t| t.contains("o/r#2")),
            "selected node tagged: {selected:?}"
        );
        assert!(
            !selected.iter().any(|t| t.contains("o/r#1")),
            "only the selected node is tagged"
        );
    }

    #[test]
    fn cycle_member_renders_with_cycle_tone() {
        // Two members caught in a dependency cycle (each carries a
        // BlockerKind::Cycle blocker). Both nodes must render with the
        // dedicated NodeCycle tone so the cycle is visible, not hidden behind
        // an otherwise-clean DAG.
        let members = vec![
            cycle_member("o/r#1", EpicMemberStatus::Ready),
            cycle_member("o/r#2", EpicMemberStatus::Ready),
        ];
        let mut s = snap(members, vec![]);
        s.cycle = true;
        let dag = layout(&s, 120, None);
        let cycled: Vec<&str> = dag
            .lines
            .iter()
            .flat_map(|l| l.iter())
            .filter(|sp| sp.tone == Tone::NodeCycle)
            .map(|sp| sp.text.as_str())
            .collect();
        assert!(
            cycled.iter().any(|t| t.contains("o/r#1")),
            "cycle member #1 carries the cycle tone: {cycled:?}"
        );
        assert!(
            cycled.iter().any(|t| t.contains("o/r#2")),
            "cycle member #2 carries the cycle tone: {cycled:?}"
        );
    }

    #[test]
    fn selection_marks_focused_cycle_node_others_stay_cycle_toned() {
        // Selecting a cycle node still shows the selection highlight (so
        // navigation feedback survives), while the *other* cycle nodes keep the
        // NodeCycle tone. Between the banner and the recolored siblings the
        // cycle stays fully visible regardless of where the cursor sits.
        let members = vec![
            cycle_member("o/r#1", EpicMemberStatus::Ready),
            cycle_member("o/r#2", EpicMemberStatus::Ready),
        ];
        let mut s = snap(members, vec![]);
        s.cycle = true;
        let dag = layout(&s, 120, Some(1));
        let selected: Vec<&str> = dag
            .lines
            .iter()
            .flat_map(|l| l.iter())
            .filter(|sp| sp.tone == Tone::Selected)
            .map(|sp| sp.text.as_str())
            .collect();
        let cycled: Vec<&str> = dag
            .lines
            .iter()
            .flat_map(|l| l.iter())
            .filter(|sp| sp.tone == Tone::NodeCycle)
            .map(|sp| sp.text.as_str())
            .collect();
        assert!(
            selected.iter().any(|t| t.contains("o/r#2")),
            "focused node keeps the selection tone: {selected:?}"
        );
        assert!(
            cycled.iter().any(|t| t.contains("o/r#1")),
            "the other cycle node stays cycle-toned: {cycled:?}"
        );
    }

    #[test]
    fn empty_snapshot_is_empty() {
        let dag = layout(&snap(vec![], vec![]), 80, None);
        assert!(dag.lines.is_empty());
        assert!(dag.columns.is_empty());
    }
}
