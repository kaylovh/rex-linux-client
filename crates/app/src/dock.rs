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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Docks {
    pub panels: Vec<PanelState>,
}

pub const MIN_SIZE: f32 = 120.0;
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
