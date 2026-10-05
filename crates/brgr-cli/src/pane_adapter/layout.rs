//! Keeps one caller's workers in a single area beside it, divided evenly.
//!
//! The first worker splits the caller: to the right of a wide caller, below a
//! narrow one. Later workers split the last worker downward instead of the
//! caller, so the caller keeps its half however many workers run, and the
//! workers form one column whose panes are then made equal. A narrow caller's
//! workers stack too: side by side under it they would be too narrow for a TUI.
//! When a worker closes, its neighbour takes its space, so the rest are made
//! equal again.
//!
//! Herdr's split tree is binary. Splitting the last worker each time makes the
//! area a chain, `W1 | (W2 | (W3 | ...))`, so the split whose first child is the
//! i-th of n workers holds the rest of the chain and gets ratio `1 / (n - i)`.
//! `herdr pane resize --direction down` on a pane raises the ratio of the split
//! below it; `up` on the pane under that border lowers it. A negative amount is
//! not a decrease, so it is never sent.

use std::{fs, path::Path};

use serde_json::Value;

use super::lifecycle::PaneReceipt;

/// A caller pane narrower than this many columns gets its workers below it.
pub(super) const NARROW_CALLER: u64 = 100;
/// Below this many rows a stacked worker cannot show its TUI.
const MIN_ROWS: u64 = 10;
/// Ratio differences smaller than this are rounding, not imbalance.
const TOLERANCE: f64 = 0.02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u64,
    y: u64,
    width: u64,
    height: u64,
}

impl Rect {
    fn from(value: &Value) -> Option<Self> {
        Some(Self {
            x: value.get("x").and_then(Value::as_u64).unwrap_or(0),
            y: value.get("y").and_then(Value::as_u64).unwrap_or(0),
            width: value.get("width")?.as_u64()?,
            height: value.get("height")?.as_u64()?,
        })
    }
}

#[derive(Debug)]
struct Split {
    down: bool,
    ratio: f64,
    rect: Rect,
}

/// One tab's panes and splits, as `herdr pane layout` reports them.
#[derive(Debug)]
pub(super) struct Layout {
    panes: Vec<(String, Rect)>,
    splits: Vec<Split>,
}

impl Layout {
    pub(super) fn parse(response: &Value) -> Option<Self> {
        let layout = response.pointer("/result/layout")?;
        let panes = layout
            .get("panes")?
            .as_array()?
            .iter()
            .filter_map(|pane| {
                Some((
                    pane.get("pane_id")?.as_str()?.to_owned(),
                    Rect::from(pane.get("rect")?)?,
                ))
            })
            .collect();
        let splits = layout
            .get("splits")
            .and_then(Value::as_array)
            .map(|splits| {
                splits
                    .iter()
                    .filter_map(|split| {
                        Some(Split {
                            down: split.get("direction")?.as_str()? == "down",
                            ratio: split.get("ratio")?.as_f64()?,
                            rect: Rect::from(split.get("rect")?)?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self { panes, splits })
    }

    fn rect(&self, pane: &str) -> Option<Rect> {
        self.panes
            .iter()
            .find(|(id, _)| id == pane)
            .map(|(_, rect)| *rect)
    }
}

/// Where the next worker opens.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Placement {
    Split {
        pane: String,
        direction: &'static str,
    },
    /// The area has no room left for another readable pane.
    Tab,
}

/// The caller's workers that are still in its tab, top to bottom. Workers
/// elsewhere, such as a pane the user moved, are left alone.
fn column(layout: &Layout, caller: &str, workers: &[String]) -> Vec<(String, Rect)> {
    let Some(caller) = layout.rect(caller) else {
        return Vec::new();
    };
    let mut column: Vec<(String, Rect)> = workers
        .iter()
        .filter_map(|worker| Some((worker.clone(), layout.rect(worker)?)))
        .filter(|(_, rect)| rect.x >= caller.x + caller.width || rect.y >= caller.y + caller.height)
        .collect();
    column.sort_by_key(|(_, rect)| rect.y);
    column
}

pub(super) fn place(layout: &Layout, caller: &str, workers: &[String]) -> Placement {
    let column = column(layout, caller, workers);
    let Some((last, _)) = column.last() else {
        let narrow = layout
            .rect(caller)
            .is_some_and(|rect| rect.width < NARROW_CALLER);
        return Placement::Split {
            pane: caller.to_owned(),
            direction: if narrow { "down" } else { "right" },
        };
    };
    let rows: u64 = column.iter().map(|(_, rect)| rect.height).sum();
    if rows / (column.len() as u64 + 1) < MIN_ROWS {
        return Placement::Tab;
    }
    Placement::Split {
        pane: last.clone(),
        direction: "down",
    }
}

/// The resizes that make the caller's workers equal, as
/// `(pane, direction, amount)` for `herdr pane resize`.
pub(super) fn equalize(
    layout: &Layout,
    caller: &str,
    workers: &[String],
) -> Vec<(String, &'static str, f64)> {
    let column = column(layout, caller, workers);
    let count = column.len();
    let mut moves = Vec::new();
    for (index, (pane, rect)) in column.iter().enumerate().take(count.saturating_sub(1)) {
        // The split whose first child is this worker: it starts where the
        // worker starts, is as wide, and is taller.
        let Some(split) = layout
            .splits
            .iter()
            .filter(|split| {
                split.down
                    && split.rect.x == rect.x
                    && split.rect.y == rect.y
                    && split.rect.width == rect.width
                    && split.rect.height > rect.height
            })
            .min_by_key(|split| split.rect.height)
        else {
            continue;
        };
        #[allow(clippy::cast_precision_loss)]
        let target = 1.0 / (count - index) as f64;
        let change = target - split.ratio;
        if change > TOLERANCE {
            moves.push((pane.clone(), "down", change));
        } else if change < -TOLERANCE {
            moves.push((column[index + 1].0.clone(), "up", -change));
        }
    }
    moves
}

/// Panes brgr opened for this caller and has not closed.
pub(super) fn workers(runs: &Path, caller: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(runs) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".pane.json"))
        .filter_map(|entry| fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<PaneReceipt>(&bytes).ok())
        .filter(|receipt| receipt.caller.as_deref() == Some(caller) && receipt.cleanup != "closed")
        .map(|receipt| receipt.pane)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pane(id: &str, x: u64, y: u64, width: u64, height: u64) -> Value {
        json!({"pane_id": id, "rect": {"x": x, "y": y, "width": width, "height": height}})
    }

    fn split(direction: &str, ratio: f64, x: u64, y: u64, width: u64, height: u64) -> Value {
        json!({"direction": direction, "ratio": ratio,
               "rect": {"x": x, "y": y, "width": width, "height": height}})
    }

    fn layout(panes: &[Value], splits: &[Value]) -> Layout {
        Layout::parse(&json!({"result": {"layout": {"panes": panes, "splits": splits}}})).unwrap()
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_first_worker_splits_the_caller_by_its_width() {
        let wide = layout(&[pane("c", 0, 0, 208, 66)], &[]);
        assert_eq!(
            place(&wide, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "right"
            }
        );
        let narrow = layout(&[pane("c", 0, 0, 90, 66)], &[]);
        assert_eq!(
            place(&narrow, "c", &[]),
            Placement::Split {
                pane: "c".into(),
                direction: "down"
            }
        );
    }

    /// Seen in Herdr 0.9: caller left, three workers stacked on the right.
    fn three_stacked(ratios: [f64; 2]) -> Layout {
        layout(
            &[
                pane("c", 0, 0, 104, 66),
                pane("a", 104, 0, 104, 33),
                pane("b", 104, 33, 104, 17),
                pane("d", 104, 50, 104, 16),
            ],
            &[
                split("right", 0.5, 0, 0, 208, 66),
                split("down", ratios[0], 104, 0, 104, 66),
                split("down", ratios[1], 104, 33, 104, 33),
            ],
        )
    }

    #[test]
    fn later_workers_stack_under_the_last_worker_not_the_caller() {
        let layout = three_stacked([0.5, 0.5]);
        assert_eq!(
            place(&layout, "c", &names(&["a", "b", "d"])),
            Placement::Split {
                pane: "d".into(),
                direction: "down"
            }
        );
    }

    #[test]
    fn a_chain_of_halves_becomes_thirds() {
        let layout = three_stacked([0.5, 0.5]);
        let moves = equalize(&layout, "c", &names(&["d", "a", "b"]));
        assert_eq!(moves.len(), 1);
        // The top split shrinks from 1/2 to 1/3 by moving the border above b up.
        assert_eq!(moves[0].0, "b");
        assert_eq!(moves[0].1, "up");
        assert!((moves[0].2 - (0.5 - 1.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn equal_workers_need_no_resize_and_unknown_panes_are_ignored() {
        let layout = three_stacked([1.0 / 3.0, 0.5]);
        assert!(equalize(&layout, "c", &names(&["a", "b", "d", "gone"])).is_empty());
        // A pane brgr did not open is never part of the area.
        assert!(equalize(&layout, "c", &names(&["a"])).is_empty());
    }

    #[test]
    fn a_grown_lower_split_is_raised_from_its_upper_pane() {
        let layout = three_stacked([1.0 / 3.0, 0.3]);
        let moves = equalize(&layout, "c", &names(&["a", "b", "d"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("b", "down"));
        assert!((moves[0].2 - 0.2).abs() < 1e-9);
    }

    #[test]
    fn workers_below_a_narrow_caller_stack_under_it() {
        let layout = layout(
            &[
                pane("c", 0, 0, 90, 33),
                pane("a", 0, 33, 90, 22),
                pane("b", 0, 55, 90, 11),
            ],
            &[
                split("down", 0.5, 0, 0, 90, 66),
                split("down", 0.66, 0, 33, 90, 33),
            ],
        );
        assert_eq!(
            place(&layout, "c", &names(&["a", "b"])),
            Placement::Split {
                pane: "b".into(),
                direction: "down"
            }
        );
        let moves = equalize(&layout, "c", &names(&["a", "b"]));
        assert_eq!(moves.len(), 1);
        assert_eq!((moves[0].0.as_str(), moves[0].1), ("b", "up"));
        assert!((moves[0].2 - 0.16).abs() < 1e-9);
    }

    #[test]
    fn a_full_area_sends_the_next_worker_to_a_tab() {
        let panes: Vec<Value> = (0..6)
            .map(|index| pane(&format!("w{index}"), 104, index * 11, 104, 11))
            .chain([pane("c", 0, 0, 104, 66)])
            .collect();
        let layout = layout(&panes, &[]);
        let workers: Vec<String> = (0..6).map(|index| format!("w{index}")).collect();
        assert_eq!(place(&layout, "c", &workers), Placement::Tab);
    }
}
