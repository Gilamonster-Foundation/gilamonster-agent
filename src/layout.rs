//! tmux absolute-cell layout tree (#50 — cockpit design phase 3).
//!
//! Ports tmux's layout cell model as a **pure** module: it owns its own
//! [`Rect`]/[`Axis`] types and imports neither `newt-*` nor `ratatui`
//! (a boundary adapter converts to the renderer's rect), so a future
//! `gila-mux-core` extraction stays mechanical — the same discipline
//! [`crate::keys`] follows.
//!
//! # The model
//!
//! An arena ([`LayoutTree::cells`]) of [`Cell`]s. A cell is either a
//! [`Leaf`](CellKind::Leaf) (a pane) or a [`Split`](CellKind::Split) of child
//! cells along one [`Axis`]. Sizes are **absolute integer cells**, never
//! weights or percentages, and siblings are separated by a **1-cell divider**,
//! so every split obeys the load-bearing invariant:
//!
//! ```text
//!   sum(child extents along the split axis) + (n - 1) == parent extent
//! ```
//!
//! [`resize_tree`](LayoutTree::resize_tree) re-fits the stored sizes to a new
//! terminal by **round-robin** ±1 distribution (tmux's discipline — preserve
//! absolute sizes, nudge to fit) rather than proportional rescaling, clamped so
//! no pane drops below [`PANE_MIN`]. When the tree legitimately can't fit
//! (every pane already at the floor), the overflow is **clipped at render** by
//! [`rects`](LayoutTree::rects), never by silently violating the invariant.
//!
//! # What is and isn't ported
//!
//! Ported: `split` (the new pane gets `(ss+1)/2 - 1`), `close` (give the freed
//! `size + 1` to a neighbour, then collapse a now-single-child parent), the
//! round-robin resize, `zoom` as a render-time root swap under the strict
//! **unzoom → mutate → rezoom** discipline, geometric directional nav (edge
//! adjacency across the 1-cell border with an MRU tie-break), and the
//! `even-h` / `even-v` / `main-vertical` presets. **Not** ported (post-3.5
//! master extras): floating panes, scrollbars, pane-status borders, full-size
//! splits, and tmux's textual custom-layout strings (the tree serializes via
//! serde instead, when that lands).

use std::collections::HashMap;

/// The minimum extent (cells) of a pane along either axis. Larger than tmux's
/// `1` because a gila chat pane needs room to render a transcript.
pub const PANE_MIN: u16 = 3;

/// An opaque pane handle. Stable for the life of the pane; the cockpit maps it
/// to a `ChatPane`/shell/etc.
pub type PaneId = usize;

/// Arena index of a [`Cell`]. Internal; never leaks past the public API.
type CellId = usize;

/// The orientation of a split: [`Horizontal`](Axis::Horizontal) lays children
/// left-to-right (each full height), [`Vertical`](Axis::Vertical) top-to-bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// A pane-relative direction for [`neighbour`](LayoutTree::neighbour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// An absolute rectangle in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    #[must_use]
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }
    fn right(&self) -> u16 {
        self.x + self.w
    }
    fn bottom(&self) -> u16 {
        self.y + self.h
    }
    /// Do the two rectangles share any interior area? (Used by the no-overlap
    /// invariant checks.)
    #[cfg(test)]
    fn overlaps(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
    /// The intersection with `bounds`, or `None` if disjoint — the clip-at-render
    /// primitive.
    fn clip(&self, bounds: &Rect) -> Option<Rect> {
        let x = self.x.max(bounds.x);
        let y = self.y.max(bounds.y);
        let r = self.right().min(bounds.right());
        let b = self.bottom().min(bounds.bottom());
        if r > x && b > y {
            Some(Rect::new(x, y, r - x, b - y))
        } else {
            None
        }
    }
}

/// A layout cell: a pane leaf, or a split of child cells.
#[derive(Debug, Clone)]
enum CellKind {
    Leaf(PaneId),
    Split { axis: Axis, children: Vec<CellId> },
}

#[derive(Debug, Clone)]
struct Cell {
    kind: CellKind,
    parent: Option<CellId>,
    /// Absolute width/height in cells (the stored layout; re-fit by
    /// [`resize_tree`](LayoutTree::resize_tree)).
    sx: u16,
    sy: u16,
}

/// The tmux cell tree for one tab. Construct with a preset (or [`single`]) then
/// [`split`]/[`close`]/[`resize_tree`]; read the pane rectangles with
/// [`rects`].
///
/// [`single`]: LayoutTree::single
/// [`split`]: LayoutTree::split
/// [`close`]: LayoutTree::close
/// [`resize_tree`]: LayoutTree::resize_tree
/// [`rects`]: LayoutTree::rects
#[derive(Debug, Clone)]
pub struct LayoutTree {
    cells: Vec<Cell>,
    root: CellId,
    next_pane: PaneId,
    /// When `Some(pane)`, `rects` renders that pane alone, full-terminal — the
    /// zoom state (structure is untouched; unzoom restores it).
    zoom: Option<PaneId>,
    /// Per-pane last-focus tick, the MRU tie-break for directional nav.
    focus_tick: HashMap<PaneId, u64>,
    tick: u64,
}

impl LayoutTree {
    // ── construction ────────────────────────────────────────────────────────

    /// A tree with a single full-terminal pane (id `0`).
    #[must_use]
    pub fn single() -> Self {
        let mut t = Self {
            cells: Vec::new(),
            root: 0,
            next_pane: 0,
            zoom: None,
            focus_tick: HashMap::new(),
            tick: 0,
        };
        let p = t.mint_pane();
        t.root = t.push(Cell {
            kind: CellKind::Leaf(p),
            parent: None,
            sx: 0,
            sy: 0,
        });
        t
    }

    /// `n` panes in a single left-to-right row (`even-h`). `n == 0` is treated
    /// as `1`.
    #[must_use]
    pub fn even_h(n: usize) -> Self {
        Self::even(n, Axis::Horizontal)
    }

    /// `n` panes in a single top-to-bottom column (`even-v`).
    #[must_use]
    pub fn even_v(n: usize) -> Self {
        Self::even(n, Axis::Vertical)
    }

    fn even(n: usize, axis: Axis) -> Self {
        let n = n.max(1);
        let mut t = Self::single();
        if n == 1 {
            return t;
        }
        // Grow the single root leaf into an `axis` split of `n` leaves.
        let first = t.root;
        let mut children = vec![first];
        for _ in 1..n {
            let p = t.mint_pane();
            let leaf = t.push(Cell {
                kind: CellKind::Leaf(p),
                parent: None,
                sx: 0,
                sy: 0,
            });
            children.push(leaf);
        }
        let split = t.push(Cell {
            kind: CellKind::Split {
                axis,
                children: children.clone(),
            },
            parent: None,
            sx: 0,
            sy: 0,
        });
        for &c in &children {
            t.cells[c].parent = Some(split);
        }
        t.root = split;
        t
    }

    /// A wide **main** pane on the left and the remaining `n - 1` panes stacked
    /// in a right-hand column — the natural follow-me shape (shell left, chat
    /// right). `n <= 1` degrades to [`single`](Self::single).
    #[must_use]
    pub fn main_vertical(n: usize) -> Self {
        if n <= 1 {
            return Self::single();
        }
        let mut t = Self::single();
        let main = t.root; // pane 0 = the main pane
                           // Right column: a vertical split of the remaining panes.
        let mut col_children = Vec::new();
        for _ in 1..n {
            let p = t.mint_pane();
            col_children.push(t.push(Cell {
                kind: CellKind::Leaf(p),
                parent: None,
                sx: 0,
                sy: 0,
            }));
        }
        let col = if col_children.len() == 1 {
            col_children[0]
        } else {
            let c = t.push(Cell {
                kind: CellKind::Split {
                    axis: Axis::Vertical,
                    children: col_children.clone(),
                },
                parent: None,
                sx: 0,
                sy: 0,
            });
            for &ch in &col_children {
                t.cells[ch].parent = Some(c);
            }
            c
        };
        let root = t.push(Cell {
            kind: CellKind::Split {
                axis: Axis::Horizontal,
                children: vec![main, col],
            },
            parent: None,
            sx: 0,
            sy: 0,
        });
        t.cells[main].parent = Some(root);
        t.cells[col].parent = Some(root);
        t.root = root;
        t
    }

    // ── queries ─────────────────────────────────────────────────────────────

    /// Every pane id, in document (left-to-right, top-to-bottom) order.
    #[must_use]
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(self.root, &mut out);
        out
    }

    /// The number of panes.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes().len()
    }

    /// Whether a pane is currently zoomed.
    #[must_use]
    pub fn is_zoomed(&self) -> bool {
        self.zoom.is_some()
    }

    // ── layout ──────────────────────────────────────────────────────────────

    /// Re-fit the stored sizes so the root fills `w × h`, distributing every
    /// per-split delta round-robin (±1 per child, cycling), clamped at
    /// [`PANE_MIN`]. Absolute sizes are preserved as far as the new size allows;
    /// a tree that cannot fit keeps its floor sizes and is clipped by
    /// [`rects`](Self::rects). Idempotent for an unchanged `(w, h)`.
    pub fn resize_tree(&mut self, w: u16, h: u16) {
        let root = self.root;
        self.fit(root, w, h);
    }

    /// The pane rectangles for a terminal `term`, clipped to it. Pure: reads the
    /// stored sizes (call [`resize_tree`](Self::resize_tree) first when the
    /// terminal changed). A zoomed pane renders alone, filling `term`.
    #[must_use]
    pub fn rects(&self, term: Rect) -> Vec<(PaneId, Rect)> {
        if let Some(z) = self.zoom {
            return vec![(z, term)];
        }
        let mut out = Vec::new();
        self.walk(self.root, term.x, term.y, &mut out);
        out.into_iter()
            .filter_map(|(p, r)| r.clip(&term).map(|c| (p, c)))
            .collect()
    }

    // ── mutations ───────────────────────────────────────────────────────────

    /// Split the pane holding `pane` along `axis`, returning the new pane id, or
    /// `None` if `pane` is unknown or too small to split in two above
    /// [`PANE_MIN`]. The new pane gets `(ss + 1) / 2 - 1` of the old extent (the
    /// old pane keeps the rest, less the divider) — tmux's split arithmetic.
    /// Follows the unzoom → mutate → rezoom discipline.
    pub fn split(&mut self, pane: PaneId, axis: Axis) -> Option<PaneId> {
        let leaf = self.leaf_of(pane)?;
        let (old_extent, cross) = match axis {
            Axis::Horizontal => (self.cells[leaf].sx, self.cells[leaf].sy),
            Axis::Vertical => (self.cells[leaf].sy, self.cells[leaf].sx),
        };
        // Need room for two panes + a divider, each at least PANE_MIN.
        if old_extent < 2 * PANE_MIN + 1 {
            return None;
        }
        let zoom = self.unzoom_take();

        let new_pane = self.mint_pane();
        let new_leaf = self.push(Cell {
            kind: CellKind::Leaf(new_pane),
            parent: None,
            sx: 0,
            sy: 0,
        });

        let new_size = old_extent.div_ceil(2) - 1;
        let old_size = old_extent - 1 - new_size;
        let set_axis = |t: &mut Self, cell: CellId, along: u16| match axis {
            Axis::Horizontal => {
                t.cells[cell].sx = along;
                t.cells[cell].sy = cross;
            }
            Axis::Vertical => {
                t.cells[cell].sy = along;
                t.cells[cell].sx = cross;
            }
        };
        set_axis(self, leaf, old_size);
        set_axis(self, new_leaf, new_size);

        let parent = self.cells[leaf].parent;
        match parent {
            Some(p) if matches!(&self.cells[p].kind, CellKind::Split { axis: a, .. } if *a == axis) =>
            {
                // Same-axis parent: insert the new leaf right after `leaf`.
                if let CellKind::Split { children, .. } = &mut self.cells[p].kind {
                    let idx = children.iter().position(|&c| c == leaf).unwrap();
                    children.insert(idx + 1, new_leaf);
                }
                self.cells[new_leaf].parent = Some(p);
            }
            _ => {
                // Wrap `leaf` in a fresh split occupying its slot.
                let split = self.push(Cell {
                    kind: CellKind::Split {
                        axis,
                        children: vec![leaf, new_leaf],
                    },
                    parent,
                    sx: self.cells[leaf].sx,
                    sy: self.cells[leaf].sy,
                });
                // The split takes the leaf's full old box; the leaf's along-axis
                // size was already reduced to old_size above, so fix the split's
                // box to the pre-split extent.
                match axis {
                    Axis::Horizontal => {
                        self.cells[split].sx = old_extent;
                        self.cells[split].sy = cross;
                    }
                    Axis::Vertical => {
                        self.cells[split].sy = old_extent;
                        self.cells[split].sx = cross;
                    }
                }
                self.replace_child(parent, leaf, split);
                self.cells[leaf].parent = Some(split);
                self.cells[new_leaf].parent = Some(split);
                if parent.is_none() {
                    self.root = split;
                }
            }
        }
        self.rezoom(zoom);
        Some(new_pane)
    }

    /// Close `pane`, giving its freed extent (`size + 1`, reclaiming the
    /// divider) to a sibling and collapsing a parent left with one child.
    /// Returns `false` if `pane` is unknown or is the last pane (never remove
    /// the last). Follows the unzoom → mutate → rezoom discipline.
    pub fn close(&mut self, pane: PaneId) -> bool {
        let Some(leaf) = self.leaf_of(pane) else {
            return false;
        };
        let Some(parent) = self.cells[leaf].parent else {
            return false; // the root leaf — the last pane
        };
        let zoom = self.unzoom_take().filter(|&z| z != pane);

        let (axis, idx, freed) = {
            let CellKind::Split { axis, children } = &self.cells[parent].kind else {
                unreachable!("a parented cell's parent is a split");
            };
            let idx = children.iter().position(|&c| c == leaf).unwrap();
            let freed = match axis {
                Axis::Horizontal => self.cells[leaf].sx,
                Axis::Vertical => self.cells[leaf].sy,
            } + 1; // + the divider that also disappears
            (*axis, idx, freed)
        };
        // Give the freed extent to ONE neighbour (previous sibling, else next);
        // this alone restores the invariant for the remaining children, so we
        // re-fit only that neighbour's subtree — not the whole parent (which
        // would round-robin the space across every sibling, not one).
        let sib_slot = if idx > 0 { idx - 1 } else { 1 };
        let sib = match &self.cells[parent].kind {
            CellKind::Split { children, .. } => children[sib_slot],
            _ => unreachable!(),
        };
        match axis {
            Axis::Horizontal => self.cells[sib].sx += freed,
            Axis::Vertical => self.cells[sib].sy += freed,
        }
        let (ssx, ssy) = (self.cells[sib].sx, self.cells[sib].sy);
        self.fit(sib, ssx, ssy);
        if let CellKind::Split { children, .. } = &mut self.cells[parent].kind {
            children.remove(idx);
        }
        // Collapse a now-single-child split into that child, then re-fit the
        // survivor to the box it inherited.
        let remaining = match &self.cells[parent].kind {
            CellKind::Split { children, .. } => children.len(),
            _ => 0,
        };
        if remaining == 1 {
            let only = match &self.cells[parent].kind {
                CellKind::Split { children, .. } => children[0],
                _ => unreachable!(),
            };
            let (bx, by) = (self.cells[parent].sx, self.cells[parent].sy);
            self.collapse_if_singleton(parent);
            self.fit(only, bx, by);
        }
        self.rezoom(zoom);
        true
    }

    /// Toggle zoom on `pane`: zoom it if unzoomed (renders alone), or unzoom.
    /// Zooming an unknown pane is a no-op. Returns the new zoom state.
    pub fn toggle_zoom(&mut self, pane: PaneId) -> bool {
        if self.zoom == Some(pane) {
            self.zoom = None;
        } else if self.leaf_of(pane).is_some() {
            self.zoom = Some(pane);
        }
        self.is_zoomed()
    }

    /// Record that `pane` was focused — the MRU signal
    /// [`neighbour`](Self::neighbour) breaks ties with.
    pub fn touch(&mut self, pane: PaneId) {
        self.tick += 1;
        self.focus_tick.insert(pane, self.tick);
    }

    /// The pane geometrically adjacent to `pane` across the border in `dir`,
    /// within terminal `term`. Among equally-adjacent candidates the
    /// most-recently-[`touch`](Self::touch)ed wins. `None` at an edge or for an
    /// unknown pane. Total: defined (Some/None, never a panic) for every pane
    /// and direction.
    #[must_use]
    pub fn neighbour(&self, pane: PaneId, dir: Dir, term: Rect) -> Option<PaneId> {
        let rects = self.rects(term);
        let me = rects.iter().find(|(p, _)| *p == pane).map(|(_, r)| *r)?;
        let mut best: Option<(PaneId, u64)> = None;
        for (p, r) in &rects {
            if *p == pane {
                continue;
            }
            let adjacent = match dir {
                // `r` sits immediately to the given side of `me` across the
                // 1-cell divider, with an overlapping cross-range.
                Dir::Right => {
                    r.x >= me.right() && r.x <= me.right() + 1 && overlap_1d(me.y, me.h, r.y, r.h)
                }
                Dir::Left => {
                    r.right() <= me.x && r.right() + 1 >= me.x && overlap_1d(me.y, me.h, r.y, r.h)
                }
                Dir::Down => {
                    r.y >= me.bottom() && r.y <= me.bottom() + 1 && overlap_1d(me.x, me.w, r.x, r.w)
                }
                Dir::Up => {
                    r.bottom() <= me.y && r.bottom() + 1 >= me.y && overlap_1d(me.x, me.w, r.x, r.w)
                }
            };
            if adjacent {
                let mru = self.focus_tick.get(p).copied().unwrap_or(0);
                if best.is_none_or(|(_, b)| mru > b) {
                    best = Some((*p, mru));
                }
            }
        }
        best.map(|(p, _)| p)
    }

    // ── internals ─────────────────────────────────────────────────────────────

    fn mint_pane(&mut self) -> PaneId {
        let p = self.next_pane;
        self.next_pane += 1;
        p
    }

    fn push(&mut self, cell: Cell) -> CellId {
        self.cells.push(cell);
        self.cells.len() - 1
    }

    fn leaf_of(&self, pane: PaneId) -> Option<CellId> {
        (0..self.cells.len()).find(|&i| {
            matches!(self.cells[i].kind, CellKind::Leaf(p) if p == pane) && self.reachable(i)
        })
    }

    fn reachable(&self, cell: CellId) -> bool {
        let mut cur = Some(cell);
        while let Some(c) = cur {
            if c == self.root {
                return true;
            }
            cur = self.cells[c].parent;
        }
        false
    }

    fn collect_panes(&self, cell: CellId, out: &mut Vec<PaneId>) {
        match &self.cells[cell].kind {
            CellKind::Leaf(p) => out.push(*p),
            CellKind::Split { children, .. } => {
                for &c in children {
                    self.collect_panes(c, out);
                }
            }
        }
    }

    fn replace_child(&mut self, parent: Option<CellId>, old: CellId, new: CellId) {
        if let Some(p) = parent {
            if let CellKind::Split { children, .. } = &mut self.cells[p].kind {
                if let Some(slot) = children.iter_mut().find(|c| **c == old) {
                    *slot = new;
                }
            }
        }
    }

    fn collapse_if_singleton(&mut self, split: CellId) {
        let only = match &self.cells[split].kind {
            CellKind::Split { children, .. } if children.len() == 1 => children[0],
            _ => return,
        };
        let parent = self.cells[split].parent;
        self.cells[only].parent = parent;
        // The surviving child inherits the split's box.
        self.cells[only].sx = self.cells[split].sx;
        self.cells[only].sy = self.cells[split].sy;
        if let Some(p) = parent {
            self.replace_child(Some(p), split, only);
        } else {
            self.root = only;
        }
    }

    /// Re-fit `cell` to `sx × sy`, recursing into a split's children with a
    /// round-robin ±1 distribution that maintains `sum + (n-1) == extent`.
    fn fit(&mut self, cell: CellId, sx: u16, sy: u16) {
        self.cells[cell].sx = sx;
        self.cells[cell].sy = sy;
        let CellKind::Split { axis, children } = &self.cells[cell].kind else {
            return;
        };
        let axis = *axis;
        let children = children.clone();
        let n = children.len();
        let (extent, cross) = match axis {
            Axis::Horizontal => (sx, sy),
            Axis::Vertical => (sy, sx),
        };
        let avail = extent.saturating_sub((n as u16).saturating_sub(1));
        let cur: Vec<u16> = children
            .iter()
            .map(|&c| match axis {
                Axis::Horizontal => self.cells[c].sx,
                Axis::Vertical => self.cells[c].sy,
            })
            .collect();
        let sizes = distribute(&cur, avail);
        for (i, &c) in children.iter().enumerate() {
            match axis {
                Axis::Horizontal => self.fit(c, sizes[i], cross),
                Axis::Vertical => self.fit(c, cross, sizes[i]),
            }
        }
    }

    fn walk(&self, cell: CellId, x: u16, y: u16, out: &mut Vec<(PaneId, Rect)>) {
        let c = &self.cells[cell];
        match &c.kind {
            CellKind::Leaf(p) => out.push((*p, Rect::new(x, y, c.sx, c.sy))),
            CellKind::Split { axis, children } => {
                let (mut cx, mut cy) = (x, y);
                for &ch in children {
                    self.walk(ch, cx, cy, out);
                    match axis {
                        Axis::Horizontal => cx += self.cells[ch].sx + 1, // + divider
                        Axis::Vertical => cy += self.cells[ch].sy + 1,
                    }
                }
            }
        }
    }

    fn unzoom_take(&mut self) -> Option<PaneId> {
        self.zoom.take()
    }

    fn rezoom(&mut self, zoom: Option<PaneId>) {
        // Rezoom only if the pane survived the mutation.
        if let Some(p) = zoom {
            if self.leaf_of(p).is_some() {
                self.zoom = Some(p);
            }
        }
    }
}

/// Do two 1-D intervals `[a, a+al)` and `[b, b+bl)` share any length?
fn overlap_1d(a: u16, al: u16, b: u16, bl: u16) -> bool {
    a < b + bl && b < a + al
}

/// Distribute `target` cells across `n` children given their current extents,
/// maintaining `sum(out) == target` when it can. From a zero start it splits
/// evenly; otherwise it nudges the current sizes ±1 round-robin (preserving
/// absolute sizes), never shrinking a child below [`PANE_MIN`]. When `target`
/// is below `n * PANE_MIN` the result floors at `PANE_MIN` and overflows —
/// caught by clip-at-render.
fn distribute(cur: &[u16], target: u16) -> Vec<u16> {
    let n = cur.len();
    if n == 0 {
        return Vec::new();
    }
    let sum: u32 = cur.iter().map(|&x| u32::from(x)).sum();
    if sum == 0 {
        let base = target / n as u16;
        let rem = target % n as u16;
        return (0..n)
            .map(|i| (base + u16::from((i as u16) < rem)).max(PANE_MIN))
            .collect();
    }
    let mut out = cur.to_vec();
    let mut delta = i64::from(target) - i64::from(sum);
    let mut i = 0usize;
    // Bounded by |delta| plus a full no-progress sweep.
    let mut guard = delta.unsigned_abs() as usize + n + 1;
    while delta != 0 && guard > 0 {
        guard -= 1;
        let idx = i % n;
        i += 1;
        if delta > 0 {
            out[idx] += 1;
            delta -= 1;
        } else if out[idx] > PANE_MIN {
            out[idx] -= 1;
            delta += 1;
        }
        if delta < 0 && out.iter().all(|&x| x <= PANE_MIN) {
            break; // every child at the floor — the rest overflows (clip)
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const TERM: Rect = Rect {
        x: 0,
        y: 0,
        w: 200,
        h: 200,
    };

    /// Recursively assert the size invariant `sum(children)+(n-1)==parent` on
    /// every split, and that each child's cross-axis size equals the parent's.
    fn assert_invariant(t: &LayoutTree, cell: CellId) {
        if let CellKind::Split { axis, children } = &t.cells[cell].kind {
            let n = children.len() as u16;
            let (extent, cross) = match axis {
                Axis::Horizontal => (t.cells[cell].sx, t.cells[cell].sy),
                Axis::Vertical => (t.cells[cell].sy, t.cells[cell].sx),
            };
            let sum: u16 = children
                .iter()
                .map(|&c| match axis {
                    Axis::Horizontal => t.cells[c].sx,
                    Axis::Vertical => t.cells[c].sy,
                })
                .sum();
            assert_eq!(sum + (n - 1), extent, "size invariant on cell {cell}");
            for &c in children {
                let ccross = match axis {
                    Axis::Horizontal => t.cells[c].sy,
                    Axis::Vertical => t.cells[c].sx,
                };
                assert_eq!(ccross, cross, "cross-axis equals parent on child {c}");
                assert_invariant(t, c);
            }
        }
    }

    fn no_overlaps(rects: &[(PaneId, Rect)]) {
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(
                    !rects[i].1.overlaps(&rects[j].1),
                    "panes {} and {} overlap: {:?} {:?}",
                    rects[i].0,
                    rects[j].0,
                    rects[i].1,
                    rects[j].1
                );
            }
        }
    }

    #[test]
    fn single_pane_fills_the_terminal() {
        let mut t = LayoutTree::single();
        t.resize_tree(80, 24);
        assert_eq!(t.rects(TERM), vec![(0, Rect::new(0, 0, 80, 24))]);
    }

    #[test]
    fn split_halves_with_a_one_cell_divider() {
        let mut t = LayoutTree::single();
        t.resize_tree(81, 24);
        let np = t.split(0, Axis::Horizontal).unwrap();
        assert_eq!(np, 1);
        let rects = t.rects(Rect::new(0, 0, 81, 24));
        // 81 = 40 (old) + 1 (divider) + 40 (new). new = (81+1)/2-1 = 40.
        assert_eq!(rects.len(), 2);
        let w0 = rects.iter().find(|(p, _)| *p == 0).unwrap().1.w;
        let w1 = rects.iter().find(|(p, _)| *p == 1).unwrap().1.w;
        assert_eq!(w0 + w1 + 1, 81);
        no_overlaps(&rects);
        assert_invariant(&t, t.root);
    }

    #[test]
    fn split_refuses_when_too_small() {
        let mut t = LayoutTree::single();
        t.resize_tree(2 * PANE_MIN, 24); // one short of 2*MIN+1
        assert_eq!(t.split(0, Axis::Horizontal), None);
        assert_eq!(t.pane_count(), 1);
    }

    #[test]
    fn nested_split_keeps_the_invariant() {
        let mut t = LayoutTree::single();
        t.resize_tree(120, 40);
        let a = t.split(0, Axis::Horizontal).unwrap();
        let _b = t.split(a, Axis::Vertical).unwrap();
        let _c = t.split(0, Axis::Vertical).unwrap();
        assert_eq!(t.pane_count(), 4);
        assert_invariant(&t, t.root);
        no_overlaps(&t.rects(Rect::new(0, 0, 120, 40)));
    }

    #[test]
    fn close_reclaims_area_and_collapses() {
        let mut t = LayoutTree::single();
        t.resize_tree(100, 30);
        let a = t.split(0, Axis::Horizontal).unwrap();
        let total_before: u32 = t
            .rects(Rect::new(0, 0, 100, 30))
            .iter()
            .map(|(_, r)| u32::from(r.w) * u32::from(r.h))
            .sum();
        assert!(t.close(a));
        assert_eq!(t.pane_count(), 1);
        // The lone survivor fills the terminal again (single-child collapse).
        assert_eq!(
            t.rects(Rect::new(0, 0, 100, 30)),
            vec![(0, Rect::new(0, 0, 100, 30))]
        );
        let total_after = 100u32 * 30;
        assert!(total_after >= total_before, "closing never loses area");
    }

    #[test]
    fn cannot_close_the_last_pane() {
        let mut t = LayoutTree::single();
        t.resize_tree(80, 24);
        assert!(!t.close(0));
        assert_eq!(t.pane_count(), 1);
    }

    #[test]
    fn zoom_renders_one_pane_full_then_restores() {
        let mut t = LayoutTree::single();
        t.resize_tree(100, 30);
        let a = t.split(0, Axis::Horizontal).unwrap();
        assert!(t.toggle_zoom(a));
        assert_eq!(
            t.rects(Rect::new(0, 0, 100, 30)),
            vec![(a, Rect::new(0, 0, 100, 30))]
        );
        assert!(!t.toggle_zoom(a));
        assert_eq!(t.rects(Rect::new(0, 0, 100, 30)).len(), 2);
    }

    #[test]
    fn mutating_while_zoomed_rezooms_survivors_only() {
        let mut t = LayoutTree::single();
        t.resize_tree(120, 30);
        let a = t.split(0, Axis::Horizontal).unwrap();
        t.toggle_zoom(a);
        // Splitting the zoomed pane keeps zoom on the surviving original.
        let _b = t.split(a, Axis::Vertical).unwrap();
        assert!(t.is_zoomed());
        // Closing the zoomed pane drops zoom (it did not survive).
        t.toggle_zoom(0);
        assert!(t.is_zoomed());
        t.close(0);
        assert!(!t.is_zoomed());
    }

    #[test]
    fn directional_nav_crosses_the_divider_and_is_total() {
        let mut t = LayoutTree::single();
        t.resize_tree(100, 30);
        let right = t.split(0, Axis::Horizontal).unwrap();
        let term = Rect::new(0, 0, 100, 30);
        assert_eq!(t.neighbour(0, Dir::Right, term), Some(right));
        assert_eq!(t.neighbour(right, Dir::Left, term), Some(0));
        assert_eq!(t.neighbour(0, Dir::Left, term), None); // at the edge
        assert_eq!(t.neighbour(0, Dir::Up, term), None);
    }

    #[test]
    fn nav_breaks_ties_by_mru() {
        // A tall left pane faces two stacked right panes; from the left, "right"
        // is ambiguous — MRU decides.
        let mut t = LayoutTree::main_vertical(3); // pane 0 left, 1 & 2 stacked right
        t.resize_tree(100, 30);
        let term = Rect::new(0, 0, 100, 30);
        t.touch(2);
        assert_eq!(t.neighbour(0, Dir::Right, term), Some(2));
        t.touch(1);
        assert_eq!(t.neighbour(0, Dir::Right, term), Some(1));
    }

    #[test]
    fn presets_have_the_right_shape() {
        let mut h = LayoutTree::even_h(3);
        h.resize_tree(100, 30);
        let r = h.rects(Rect::new(0, 0, 100, 30));
        assert_eq!(r.len(), 3);
        assert!(r.iter().all(|(_, rc)| rc.h == 30), "even-h: full height");
        no_overlaps(&r);

        let mut v = LayoutTree::even_v(4);
        v.resize_tree(100, 40);
        let rv = v.rects(Rect::new(0, 0, 100, 40));
        assert_eq!(rv.len(), 4);
        assert!(rv.iter().all(|(_, rc)| rc.w == 100), "even-v: full width");

        let mut mv = LayoutTree::main_vertical(3);
        mv.resize_tree(100, 30);
        let rmv = mv.rects(Rect::new(0, 0, 100, 30));
        assert_eq!(rmv.len(), 3);
        // The main pane (0) is full height; the other two share the right column.
        let main = rmv.iter().find(|(p, _)| *p == 0).unwrap().1;
        assert_eq!(main.h, 30);
        assert_eq!(main.x, 0);
    }

    #[test]
    fn main_vertical_two_has_a_single_right_pane() {
        // n == 2: the right column is a lone leaf (no nested split to collapse).
        let mut t = LayoutTree::main_vertical(2);
        t.resize_tree(100, 30);
        let r = t.rects(Rect::new(0, 0, 100, 30));
        assert_eq!(r.len(), 2);
        let main = r.iter().find(|(p, _)| *p == 0).unwrap().1;
        let side = r.iter().find(|(p, _)| *p == 1).unwrap().1;
        assert_eq!(main.h, 30);
        assert_eq!(side.h, 30);
        assert_eq!(main.w + side.w + 1, 100);
    }

    #[test]
    fn vertical_split_nav_crosses_up_and_down() {
        let mut t = LayoutTree::single();
        t.resize_tree(40, 30);
        let below = t.split(0, Axis::Vertical).unwrap();
        let term = Rect::new(0, 0, 40, 30);
        assert_eq!(t.neighbour(0, Dir::Down, term), Some(below));
        assert_eq!(t.neighbour(below, Dir::Up, term), Some(0));
        assert_eq!(t.neighbour(below, Dir::Down, term), None);
    }

    #[test]
    fn unknown_pane_operations_are_safe_noops() {
        let mut t = LayoutTree::single();
        t.resize_tree(80, 24);
        assert_eq!(t.split(999, Axis::Horizontal), None);
        assert!(!t.close(999));
        assert_eq!(t.neighbour(999, Dir::Left, Rect::new(0, 0, 80, 24)), None);
        assert!(!t.toggle_zoom(999)); // zooming a missing pane stays unzoomed
        assert_eq!(t.pane_count(), 1);
    }

    #[test]
    fn even_of_zero_is_one_pane() {
        assert_eq!(LayoutTree::even_h(0).pane_count(), 1);
        assert_eq!(LayoutTree::main_vertical(1).pane_count(), 1);
    }

    #[test]
    fn oversized_tree_clips_at_render_without_overlap() {
        let mut t = LayoutTree::even_h(20);
        // 20 panes need at least 20*PANE_MIN + 19 dividers = 79 cells; give 40.
        t.resize_tree(40, 10);
        let rects = t.rects(Rect::new(0, 0, 40, 10));
        // Every returned rect is within the terminal and none overlap.
        for (_, r) in &rects {
            assert!(
                r.right() <= 40 && r.bottom() <= 10,
                "clipped to term: {r:?}"
            );
        }
        no_overlaps(&rects);
    }

    // ── the proptest invariant suite (the hard deliverable) ─────────────────

    #[derive(Debug, Clone)]
    enum Op {
        Split(usize, bool), // (pane index into current panes, horizontal?)
        Close(usize),
        Resize(u16, u16),
        Zoom(usize),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0usize..8, any::<bool>()).prop_map(|(i, h)| Op::Split(i, h)),
            (0usize..8).prop_map(Op::Close),
            (10u16..180, 10u16..180).prop_map(|(w, h)| Op::Resize(w, h)),
            (0usize..8).prop_map(Op::Zoom),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// After ANY sequence of split/close/resize/zoom, on a generous
        /// terminal (no clipping): the size invariant holds, panes never
        /// overlap, and directional nav is total from every pane.
        #[test]
        fn invariants_hold_under_arbitrary_ops(ops in prop::collection::vec(op_strategy(), 0..40)) {
            let mut t = LayoutTree::single();
            t.resize_tree(TERM.w, TERM.h);
            for op in ops {
                let panes = t.panes();
                match op {
                    Op::Split(i, h) => {
                        if !panes.is_empty() {
                            let p = panes[i % panes.len()];
                            let axis = if h { Axis::Horizontal } else { Axis::Vertical };
                            t.split(p, axis);
                        }
                    }
                    Op::Close(i) => {
                        if !panes.is_empty() {
                            t.close(panes[i % panes.len()]);
                        }
                    }
                    Op::Resize(w, h) => t.resize_tree(w, h),
                    Op::Zoom(i) => {
                        if !panes.is_empty() {
                            t.toggle_zoom(panes[i % panes.len()]);
                        }
                    }
                }
                // Renormalize to the generous terminal so the invariant is exact
                // (small resizes above may deliberately overflow → clip).
                t.resize_tree(TERM.w, TERM.h);

                prop_assert!(t.pane_count() >= 1, "never lose the last pane");
                assert_invariant(&t, t.root);
                if !t.is_zoomed() {
                    let rects = t.rects(TERM);
                    prop_assert_eq!(rects.len(), t.pane_count());
                    for i in 0..rects.len() {
                        for j in (i + 1)..rects.len() {
                            prop_assert!(!rects[i].1.overlaps(&rects[j].1));
                        }
                    }
                }
                // Nav is total: every (pane, dir) yields None or a real pane.
                let live = t.panes();
                for &p in &live {
                    for dir in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
                        if let Some(nb) = t.neighbour(p, dir, TERM) {
                            prop_assert!(live.contains(&nb));
                        }
                    }
                }
            }
        }

        /// Close-then-reopen (split) restores the total pane area on a fixed
        /// terminal — area is conserved across the round trip.
        #[test]
        fn close_then_split_conserves_area(w in 40u16..160, h in 12u16..80) {
            let mut t = LayoutTree::single();
            t.resize_tree(w, h);
            let term = Rect::new(0, 0, w, h);
            let area = |t: &LayoutTree| -> u32 {
                t.rects(term).iter().map(|(_, r)| u32::from(r.w) * u32::from(r.h)).sum()
            };
            let a0 = area(&t);
            if let Some(p) = t.split(0, Axis::Horizontal) {
                prop_assert!(t.close(p));
                // Back to one pane filling the terminal.
                prop_assert_eq!(area(&t), a0);
            }
        }
    }
}
