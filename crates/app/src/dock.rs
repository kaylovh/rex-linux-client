//! Where the panels sit: each panel (sessions, files, preview, transfers) is docked on the
//! left, the right or the bottom, open or not, with its own size. Panels on the same side
//! sit next to each other in their order. The status bar toggles and moves them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Panel {
    Sessions,
    Files,
    Preview,
    Transfers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PanelState {
    pub panel: Panel,
    pub side: Side,
    pub open: bool,
    /// Width (left, right) or height (bottom) in pixels.
    pub size: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Docks {
    pub panels: Vec<PanelState>,
}

/// Read leniently: an entry this build doesn't know (a newer panel, a bad side) is skipped,
/// a missing field takes the panel's default.
impl<'de> Deserialize<'de> for Docks {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            panels: Vec<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Entry {
            panel: Panel,
            side: Option<Side>,
            open: Option<bool>,
            size: Option<f32>,
        }
        let raw = Raw::deserialize(d)?;
        let panels = raw
            .panels
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Entry>(v).ok())
            .map(|e| {
                let def = Docks::default().get(e.panel);
                PanelState { panel: e.panel, side: e.side.unwrap_or(def.side), open: e.open.unwrap_or(def.open), size: e.size.unwrap_or(f32::NAN) }
            })
            .collect();
        Ok(Docks { panels })
    }
}

pub const MIN_SIZE: f32 = 120.0;
/// What the terminals keep at least, beside the side docks.
pub const MIN_CENTER: f32 = 320.0;

/// The side docks' widths, scaled down together when they'd leave the terminals less than
/// MIN_CENTER of `window`.
pub fn fit_widths(widths: &[f32], window: f32) -> Vec<f32> {
    let sum: f32 = widths.iter().sum();
    let room = (window - MIN_CENTER).max(0.0);
    if sum <= room || sum <= 0.0 {
        return widths.to_vec();
    }
    widths.iter().map(|w| w * room / sum).collect()
}
pub const MAX_SIZE: f32 = 1600.0;

impl Default for Docks {
    fn default() -> Self {
        let p = |panel, side, open, size| PanelState { panel, side, open, size };
        Docks {
            panels: vec![
                p(Panel::Sessions, Side::Left, true, 220.0),
                p(Panel::Preview, Side::Right, false, 520.0),
                p(Panel::Files, Side::Right, false, 300.0),
                p(Panel::Transfers, Side::Bottom, false, 180.0),
            ],
        }
    }
}

impl Docks {
    /// From a state.json written before docks: the sidebar and the file panel as they were.
    pub fn from_old(sidebar_open: bool, sidebar_width: Option<f32>, files_open: bool) -> Self {
        let mut d = Docks::default();
        d.set_open(Panel::Sessions, sidebar_open);
        if let Some(w) = sidebar_width {
            d.resize(Panel::Sessions, w);
        }
        d.set_open(Panel::Files, files_open);
        d
    }

    fn slot(&mut self, p: Panel) -> &mut PanelState {
        if !self.panels.iter().any(|s| s.panel == p) {
            let d = Docks::default().get(p);
            self.panels.push(d);
        }
        self.panels.iter_mut().find(|s| s.panel == p).expect("just added")
    }

    pub fn get(&self, p: Panel) -> PanelState {
        self.panels.iter().find(|s| s.panel == p).copied().unwrap_or_else(|| {
            Docks::default().panels.into_iter().find(|s| s.panel == p).expect("every panel has a default")
        })
    }

    pub fn is_open(&self, p: Panel) -> bool {
        self.get(p).open
    }

    /// The open panels on `side`, in order.
    pub fn shown(&self, side: Side) -> Vec<PanelState> {
        self.panels.iter().filter(|s| s.side == side && s.open).copied().collect()
    }

    /// Every panel docked on `side`, open or not (for the status bar).
    pub fn on(&self, side: Side) -> Vec<Panel> {
        self.panels.iter().filter(|s| s.side == side).map(|s| s.panel).collect()
    }

    pub fn set_open(&mut self, p: Panel, open: bool) {
        self.slot(p).open = open;
    }

    pub fn toggle(&mut self, p: Panel) {
        let open = !self.is_open(p);
        self.set_open(p, open);
    }

    /// Moves `p` to the end of `side` and opens it.
    pub fn move_to(&mut self, p: Panel, side: Side) {
        let mut s = *self.slot(p);
        s.open = true;
        if s.side != side {
            s.side = side;
            self.panels.retain(|x| x.panel != p);
            self.panels.push(s);
        } else {
            *self.slot(p) = s;
        }
    }

    pub fn resize(&mut self, p: Panel, size: f32) {
        self.slot(p).size = if size.is_finite() { size.clamp(MIN_SIZE, MAX_SIZE) } else { Docks::default().get(p).size };
    }

    /// As the app starts: the preview shows only when files are to be restored into it.
    pub fn on_start(mut self, has_previews: bool) -> Self {
        if !has_previews {
            self.set_open(Panel::Preview, false);
        }
        self
    }

    /// Repairs a state file: every panel once, sizes in range.
    pub fn normalized(self) -> Self {
        let mut out = Docks { panels: Vec::new() };
        for s in self.panels {
            if out.panels.iter().any(|x| x.panel == s.panel) {
                continue;
            }
            out.panels.push(s);
            out.resize(s.panel, s.size);
        }
        for d in Docks::default().panels {
            if !out.panels.iter().any(|x| x.panel == d.panel) {
                out.panels.push(d);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_leave_room_for_the_terminals() {
        // Enough room: unchanged.
        assert_eq!(fit_widths(&[220.0, 300.0], 1600.0), vec![220.0, 300.0]);
        // A laptop window with a state from a big monitor: scaled to leave MIN_CENTER.
        let w = fit_widths(&[600.0, 900.0, 500.0], 1200.0);
        let sum: f32 = w.iter().sum();
        assert!((sum - (1200.0 - MIN_CENTER)).abs() < 0.5, "{w:?}");
        assert!(w[1] > w[0] && w[0] > w[2]);
        // A tiny window: nothing negative.
        assert!(fit_widths(&[300.0], 100.0).iter().all(|x| *x >= 0.0));
    }

    #[test]
    fn preview_opens_only_with_files_to_show() {
        let mut d = Docks::default();
        d.set_open(Panel::Preview, true);
        assert!(!d.clone().on_start(false).is_open(Panel::Preview));
        assert!(d.on_start(true).is_open(Panel::Preview));
    }

    #[test]
    fn unknown_entries_are_skipped() {
        let json = r#"{"panels": [
            {"panel": "sessions", "side": "right", "open": false, "size": 250},
            {"panel": "minimap", "side": "left", "open": true, "size": 100},
            {"panel": "files", "side": "left"}
        ]}"#;
        let d: Docks = serde_json::from_str(json).unwrap();
        let d = d.normalized();
        assert_eq!(d.get(Panel::Sessions).side, Side::Right);
        assert_eq!(d.get(Panel::Files).side, Side::Left);
        assert_eq!(d.get(Panel::Files).size, Docks::default().get(Panel::Files).size);
        assert_eq!(d.panels.len(), 4);
    }

    #[test]
    fn defaults() {
        let d = Docks::default();
        assert_eq!(d.on(Side::Left), [Panel::Sessions]);
        assert_eq!(d.on(Side::Right), [Panel::Preview, Panel::Files]);
        assert_eq!(d.on(Side::Bottom), [Panel::Transfers]);
        assert!(d.is_open(Panel::Sessions));
        assert!(!d.is_open(Panel::Files));
        assert_eq!(d.shown(Side::Left).len(), 1);
    }

    #[test]
    fn old_state_keeps_its_look() {
        let d = Docks::from_old(false, Some(300.0), true);
        assert!(!d.is_open(Panel::Sessions));
        assert_eq!(d.get(Panel::Sessions).size, 300.0);
        assert!(d.is_open(Panel::Files));
        let d = Docks::from_old(true, None, false);
        assert!(d.is_open(Panel::Sessions));
        assert_eq!(d.get(Panel::Sessions).size, Docks::default().get(Panel::Sessions).size);
    }

    #[test]
    fn move_and_toggle() {
        let mut d = Docks::default();
        d.move_to(Panel::Files, Side::Left);
        assert_eq!(d.on(Side::Left), [Panel::Sessions, Panel::Files]);
        assert_eq!(d.on(Side::Right), [Panel::Preview]);
        assert!(d.is_open(Panel::Files), "a moved panel shows");
        // Moving to where it is keeps it there (and open).
        d.set_open(Panel::Files, false);
        d.move_to(Panel::Files, Side::Left);
        assert_eq!(d.on(Side::Left), [Panel::Sessions, Panel::Files]);
        assert!(d.is_open(Panel::Files));
        d.toggle(Panel::Sessions);
        assert_eq!(d.shown(Side::Left).iter().map(|p| p.panel).collect::<Vec<_>>(), [Panel::Files]);
        d.move_to(Panel::Sessions, Side::Bottom);
        assert_eq!(d.on(Side::Bottom), [Panel::Transfers, Panel::Sessions]);
    }

    #[test]
    fn sizes_stay_in_range_and_bad_states_are_repaired() {
        let mut d = Docks::default();
        d.resize(Panel::Files, 10.0);
        assert_eq!(d.get(Panel::Files).size, MIN_SIZE);
        d.resize(Panel::Files, 1e9);
        assert_eq!(d.get(Panel::Files).size, MAX_SIZE);
        let broken = Docks {
            panels: vec![
                PanelState { panel: Panel::Files, side: Side::Left, open: true, size: f32::NAN },
                PanelState { panel: Panel::Files, side: Side::Right, open: false, size: 200.0 },
            ],
        }
        .normalized();
        assert_eq!(broken.panels.len(), 4);
        assert_eq!(broken.get(Panel::Files).side, Side::Left);
        assert_eq!(broken.get(Panel::Files).size, Docks::default().get(Panel::Files).size);
        assert!(broken.get(Panel::Sessions).open);
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<Docks>(&json).unwrap(), d);
    }
}
