use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Display {
    pub name: Option<String>,
    pub work: Rect,
    pub scale: f64,
    pub primary: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Geometry {
    pub outer: Rect,
    pub inner_width: f64,
    pub inner_height: f64,
    pub display: Display,
}

#[derive(Debug)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: f64,
    pub height: f64,
    pub min_width: f64,
    pub min_height: f64,
}

impl Rect {
    fn valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|n| n.is_finite())
            && self.width > 0.0
            && self.height > 0.0
    }
}
impl Display {
    pub fn valid(&self) -> bool {
        self.work.valid() && self.scale.is_finite() && self.scale > 0.0
    }
}
impl Geometry {
    pub fn valid(&self) -> bool {
        self.outer.valid()
            && self.display.valid()
            && self.inner_width.is_finite()
            && self.inner_width > 0.0
            && self.inner_height.is_finite()
            && self.inner_height > 0.0
            && self.inner_width <= self.outer.width
            && self.inner_height <= self.outer.height
    }
}

// Position is physical; sizes passed to the Tauri builder are logical. Restore
// relative to the same display when present; otherwise center on the primary.
// Clamp the entire frame to the current work area, including a changed Dock.
pub fn fit(saved: Option<&Geometry>, displays: &[Display]) -> Result<Placement, String> {
    let fallback = displays
        .iter()
        .find(|d| d.primary && d.valid())
        .or_else(|| displays.iter().find(|d| d.valid()))
        .ok_or("No usable display is available")?;
    let saved = saved.filter(|g| g.valid());
    let same = saved.and_then(|g| {
        displays
            .iter()
            .filter(|d| {
                d.valid()
                    && d.name == g.display.name
                    && (d.name.is_some() || d.work == g.display.work)
            })
            .min_by(|a, b| {
                let distance =
                    |d: &Display| (d.work.x - g.display.work.x).hypot(d.work.y - g.display.work.y);
                distance(a).total_cmp(&distance(b))
            })
    });
    let target = same.unwrap_or(fallback);
    let (width, height, frame_width, frame_height) = saved
        .map(|g| {
            (
                g.inner_width / g.display.scale,
                g.inner_height / g.display.scale,
                (g.outer.width - g.inner_width) / g.display.scale,
                (g.outer.height - g.inner_height) / g.display.scale,
            )
        })
        .unwrap_or((980.0, 850.0, 0.0, 28.0));
    let max_width = (target.work.width / target.scale - frame_width).max(1.0);
    let max_height = (target.work.height / target.scale - frame_height).max(1.0);
    let min_width = width.clamp(320.0, 580.0).min(max_width);
    let min_height = height.clamp(240.0, 600.0).min(max_height);
    let width = width.clamp(min_width, max_width);
    let height = height.clamp(min_height, max_height);
    let outer_width = (width + frame_width) * target.scale;
    let outer_height = (height + frame_height) * target.scale;
    let (x, y) = match (saved, same) {
        (Some(g), Some(_)) => (
            target.work.x + (g.outer.x - g.display.work.x) / g.display.scale * target.scale,
            target.work.y + (g.outer.y - g.display.work.y) / g.display.scale * target.scale,
        ),
        _ => (
            target.work.x + (target.work.width - outer_width) / 2.0,
            target.work.y + (target.work.height - outer_height) / 2.0,
        ),
    };
    Ok(Placement {
        x: x.clamp(
            target.work.x,
            (target.work.x + target.work.width - outer_width).max(target.work.x),
        )
        .round() as i32,
        y: y.clamp(
            target.work.y,
            (target.work.y + target.work.height - outer_height).max(target.work.y),
        )
        .round() as i32,
        width,
        height,
        min_width,
        min_height,
    })
}

#[derive(Clone, Copy, PartialEq)]
pub enum Pattern {
    Columns,
    Focus,
    Grid,
}

/// Work-area cells are physical pixels, including window decorations.
pub fn cell(work: &Rect, pattern: Pattern, i: usize, count: usize) -> Rect {
    debug_assert!(i < count);
    let (cols, rows, col, row) = match pattern {
        Pattern::Grid => {
            let cols = (count as f64).sqrt().ceil() as usize;
            (cols, count.div_ceil(cols), i % cols, i / cols)
        }
        Pattern::Focus if count > 1 => {
            if i == 0 {
                (3, 1, 0, 0)
            } else {
                (3, count - 1, 2, i - 1)
            }
        }
        _ => (count, 1, i, 0),
    };
    let span = if pattern == Pattern::Focus && i == 0 && count > 1 {
        2
    } else {
        1
    };
    let x = (work.width * col as f64 / cols as f64).round();
    let y = (work.height * row as f64 / rows as f64).round();
    Rect {
        x: work.x + x,
        y: work.y + y,
        width: (work.width * (col + span) as f64 / cols as f64).round() - x,
        height: (work.height * (row + 1) as f64 / rows as f64).round() - y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn display(name: &str, x: f64, scale: f64) -> Display {
        Display {
            name: Some(name.into()),
            work: Rect {
                x,
                y: 40.0,
                width: 1920.0 * scale,
                height: 1040.0 * scale,
            },
            scale,
            primary: x == 0.0,
        }
    }
    fn geometry(display: Display) -> Geometry {
        Geometry {
            outer: Rect {
                x: display.work.x + 100.0 * display.scale,
                y: display.work.y + 50.0 * display.scale,
                width: 1000.0 * display.scale,
                height: 828.0 * display.scale,
            },
            inner_width: 1000.0 * display.scale,
            inner_height: 800.0 * display.scale,
            display,
        }
    }
    #[test]
    fn inconsistent_resize_sample_preserves_the_last_saved_position() {
        let mut layout = crate::layout::Layout::default();
        let good = geometry(display("external", -1920.0, 1.0));
        let surface = layout.surfaces.get_mut("main").expect("main window");
        surface.remember_geometry(good.clone());
        let mut mixed = good.clone();
        mixed.inner_width = mixed.outer.width + 1.0;
        surface.remember_geometry(mixed);
        assert_eq!(surface.geometry.as_ref(), Some(&good));
        layout
            .validate()
            .expect("other window state can still be saved");
    }

    #[test]
    fn identical_displays_and_tile_edges() {
        let left = display("External", -1920.0, 1.0);
        let right = display("External", 0.0, 1.0);
        let saved = geometry(left.clone());
        let p = fit(Some(&saved), &[right, left.clone()]).unwrap();
        assert_eq!(p.x, -1820);
        for pattern in [Pattern::Columns, Pattern::Focus, Pattern::Grid] {
            let tiles: Vec<_> = (0..4).map(|i| cell(&left.work, pattern, i, 4)).collect();
            assert_eq!(
                tiles.iter().map(|r| r.width * r.height).sum::<f64>(),
                left.work.width * left.work.height
            );
            for (i, a) in tiles.iter().enumerate() {
                for b in &tiles[i + 1..] {
                    assert!(
                        a.x + a.width <= b.x
                            || b.x + b.width <= a.x
                            || a.y + a.height <= b.y
                            || b.y + b.height <= a.y
                    );
                }
            }
        }
    }
    #[test]
    fn compact_grid_cell_keeps_its_saved_height_after_restart() {
        let monitor = display("desk", 0.0, 1.0);
        let mut saved = geometry(monitor.clone());
        saved.outer.height = 520.0;
        saved.inner_height = 492.0;
        let p = fit(Some(&saved), &[monitor]).expect("restore compact tile");
        assert_eq!(p.height, 492.0);
        assert!(p.min_height <= p.height);
    }

    #[test]
    fn preserves_logical_size_and_relative_position_across_scale_changes() {
        let old = geometry(display("external", -3840.0, 2.0));
        let new = display("external", -1920.0, 1.0);
        let p = fit(Some(&old), &[new]).expect("display");
        assert_eq!((p.width, p.height), (1000.0, 800.0));
        assert_eq!((p.x, p.y), (-1820, 90));
    }
    #[test]
    fn removed_display_and_smaller_work_area_keep_the_whole_window_visible() {
        let old = geometry(display("removed", 5000.0, 2.0));
        let mut current = display("laptop", 0.0, 1.0);
        current.work = Rect {
            x: 0.0,
            y: 40.0,
            width: 900.0,
            height: 650.0,
        };
        let p = fit(Some(&old), &[current.clone()]).expect("fallback");
        assert!(p.x >= 0 && p.y >= 40);
        assert!(f64::from(p.x) + p.width <= 900.0);
        assert!(f64::from(p.y) + p.height + 28.0 <= 690.0);
        assert_eq!(p.width, 900.0);
        assert_eq!(p.height, 622.0);
        let mut invalid = old;
        invalid.inner_width = f64::NAN;
        assert!(fit(Some(&invalid), &[current]).is_ok());
        assert!(fit(None, &[]).is_err());
    }
    #[test]
    fn changed_monitor_arrangement_and_dock_clamp_negative_and_oversized_positions() {
        let mut old = geometry(display("laptop", 0.0, 1.0));
        old.outer.x = 1800.0;
        old.outer.y = 1000.0;
        let mut current = display("laptop", -2000.0, 1.0);
        current.work.width = 1200.0;
        current.work.height = 900.0;
        let p = fit(Some(&old), &[current.clone()]).expect("clamp");
        assert!(f64::from(p.x) >= current.work.x);
        assert!(f64::from(p.x) + p.width <= current.work.x + current.work.width);
        assert!(f64::from(p.y) + p.height + 28.0 <= current.work.y + current.work.height);
    }
}
