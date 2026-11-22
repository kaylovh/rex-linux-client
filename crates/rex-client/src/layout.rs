//! A Rex window's layout (from `session.view`) as panes and separators, in fractions of
//! the window's size. The view is the truth: a client draws it and asks the server to change it.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pane {
    pub block_id: String,
    pub rect: Rect,
    pub floating: bool,
    pub terminal: bool,
    pub label: String,
    pub host_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Separator {
    pub direction: Direction,
    /// The whole split's area.
    pub rect: Rect,
    pub ratio: f32,
    pub before_block: String,
    pub after_block: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowLayout {
    pub panes: Vec<Pane>,
    pub separators: Vec<Separator>,
    pub zoomed: Option<String>,
}

const FULL: Rect = Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };

fn rect_of(v: &Value) -> Option<Rect> {
    let a = v.as_array()?;
    let n = |i: usize| a.get(i).and_then(Value::as_f64).map(|f| f as f32);
    Some(Rect { x: n(0)?, y: n(1)?, w: n(2)?, h: n(3)? })
}

/// The block's entry in its layer's `blocks` list.
fn block_info<'a>(layer: &'a Value, id: &str) -> Option<&'a Value> {
    layer["blocks"].as_array()?.iter().find(|b| b["block_id"] == id)
}

fn pane(layer: &Value, id: &str, rect: Rect, floating: bool) -> Pane {
    let info = block_info(layer, id);
    let s = |k: &str| info.and_then(|b| b[k].as_str()).unwrap_or("").to_string();
    let creator = s("creator_name");
    Pane {
        block_id: id.to_string(),
        rect,
        floating,
        terminal: creator.is_empty() || creator == "com.superlogical.terminal",
        label: s("label"),
        host_id: s("host_id"),
    }
}

/// The top-left leaf of a layout subtree: a block on that side of a split.
fn first_block(node: &Value) -> Option<String> {
    if let Some(id) = node["block_id"].as_str() {
        return Some(id.to_string());
    }
    first_block(&node["split"]["before"])
}

fn walk(layer: &Value, node: &Value, out: &mut WindowLayout) {
    if let Some(id) = node["block_id"].as_str() {
        let rect = rect_of(&node["rect"]).unwrap_or(FULL);
        out.panes.push(pane(layer, id, rect, false));
        return;
    }
    let split = &node["split"];
    if !split.is_object() {
        return;
    }
    let (before, after) = (&split["before"], &split["after"]);
    if let (Some(rect), Some(b), Some(a)) = (rect_of(&node["rect"]), first_block(before), first_block(after)) {
        out.separators.push(Separator {
            direction: if split["direction"] == "vertical" { Direction::Vertical } else { Direction::Horizontal },
            rect,
            ratio: split["ratio"].as_f64().unwrap_or(0.5) as f32,
            before_block: b,
            after_block: a,
        });
    }
    walk(layer, before, out);
    walk(layer, after, out);
}

pub fn window_layout(window: &Value) -> WindowLayout {
    let mut out = WindowLayout::default();
    let Some(layers) = window["layers"].as_array() else { return out };
    if let Some(z) = window["zoomed_block_id"].as_str() {
        let layer = layers.iter().find(|l| block_info(l, z).is_some()).unwrap_or(&Value::Null);
        out.panes.push(pane(layer, z, FULL, false));
        out.zoomed = Some(z.to_string());
        return out;
    }
    for layer in layers.iter().filter(|l| l["kind"] == "tiled") {
        walk(layer, &layer["layout"], &mut out);
    }
    for layer in layers.iter().filter(|l| l["kind"] != "tiled" && l["state"] != "minimized") {
        let Some(bounds) = rect_of(&layer["bounds"]) else { continue };
        // A floating layer is a box; its first block fills it.
        if let Some(id) = first_block(&layer["layout"]) {
            out.panes.push(pane(layer, &id, bounds, true));
        }
    }
    out
}

impl Separator {
    /// The ratio that puts the border at (x, y), in window fractions, kept off the edges.
    pub fn ratio_at(&self, x: f32, y: f32) -> f32 {
        let r = match self.direction {
            Direction::Horizontal => (x - self.rect.x) / self.rect.w.max(f32::EPSILON),
            Direction::Vertical => (y - self.rect.y) / self.rect.h.max(f32::EPSILON),
        };
        r.clamp(0.05, 0.95)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    /// rexa's layout.zig test: a and b stacked in the left half, c on the right.
    fn three_way() -> Value {
        json!({"window_id":"w","layers":[{"kind":"tiled","bounds":[0,0,1,1],"layout":{"rect":[0,0,1,1],"split":{"split_id":"s1","direction":"horizontal","ratio":0.5,"before":{"rect":[0,0,0.5,1],"split":{"split_id":"s2","direction":"vertical","ratio":0.5,"before":{"rect":[0,0,0.5,0.5],"block_id":"a"},"after":{"rect":[0,0.5,0.5,0.5],"block_id":"b"}}},"after":{"rect":[0.5,0,0.5,1],"block_id":"c"}}},"blocks":[
            {"block_id":"a","creator_name":"com.superlogical.terminal","label":"A","host_id":"h1"},
            {"block_id":"b","creator_name":"com.superlogical.terminal"},
            {"block_id":"c","creator_name":"com.example.other"}]}]})
    }

    fn pane<'a>(l: &'a WindowLayout, id: &str) -> &'a Pane {
        l.panes.iter().find(|p| p.block_id == id).unwrap()
    }

    #[test]
    fn three_way_split_rects() {
        let l = window_layout(&three_way());
        assert_eq!(l.panes.len(), 3);
        assert_eq!(pane(&l, "a").rect, r(0.0, 0.0, 0.5, 0.5));
        assert_eq!(pane(&l, "b").rect, r(0.0, 0.5, 0.5, 0.5));
        assert_eq!(pane(&l, "c").rect, r(0.5, 0.0, 0.5, 1.0));
        assert!(pane(&l, "a").terminal && !pane(&l, "c").terminal);
        assert_eq!(pane(&l, "a").label, "A");
        assert_eq!(pane(&l, "a").host_id, "h1");
    }

    #[test]
    fn separators_name_blocks_on_each_side() {
        let l = window_layout(&three_way());
        assert_eq!(l.separators.len(), 2);
        let h = l.separators.iter().find(|s| s.direction == Direction::Horizontal).unwrap();
        assert_eq!((h.before_block.as_str(), h.after_block.as_str(), h.ratio), ("a", "c", 0.5));
        assert_eq!(h.rect, r(0.0, 0.0, 1.0, 1.0));
        let v = l.separators.iter().find(|s| s.direction == Direction::Vertical).unwrap();
        assert_eq!((v.before_block.as_str(), v.after_block.as_str()), ("a", "b"));
    }

    #[test]
    fn zoomed_window_shows_one_pane() {
        let mut w = three_way();
        w["zoomed_block_id"] = json!("b");
        let l = window_layout(&w);
        assert_eq!(l.panes.len(), 1);
        assert_eq!(l.panes[0].block_id, "b");
        assert_eq!(l.panes[0].rect, r(0.0, 0.0, 1.0, 1.0));
        assert!(l.separators.is_empty());
        assert_eq!(l.zoomed.as_deref(), Some("b"));
    }

    #[test]
    fn minimized_layer_is_hidden() {
        let mut w = three_way();
        let float = json!({"kind":"floating","state":"minimized","bounds":[0.2,0.2,0.5,0.5],"layout":{"rect":[0.2,0.2,0.5,0.5],"block_id":"f"},"blocks":[{"block_id":"f","creator_name":"com.superlogical.terminal"}]});
        w["layers"].as_array_mut().unwrap().push(float.clone());
        assert!(window_layout(&w).panes.iter().all(|p| p.block_id != "f"));
        let mut shown = float;
        shown.as_object_mut().unwrap().remove("state");
        w["layers"].as_array_mut().unwrap().pop();
        w["layers"].as_array_mut().unwrap().push(shown);
        let l = window_layout(&w);
        let f = pane(&l, "f");
        assert!(f.floating);
        assert_eq!(f.rect, r(0.2, 0.2, 0.5, 0.5));
        assert_eq!(l.panes.last().unwrap().block_id, "f");
    }

    #[test]
    fn ratio_at_clamps() {
        let s = Separator { direction: Direction::Horizontal, rect: r(0.0, 0.0, 1.0, 1.0), ratio: 0.5, before_block: "a".into(), after_block: "b".into() };
        assert!((s.ratio_at(0.3, 0.5) - 0.3).abs() < 1e-6);
        assert!((s.ratio_at(0.0, 0.5) - 0.05).abs() < 1e-6);
        let v = Separator { direction: Direction::Vertical, rect: r(0.0, 0.5, 1.0, 0.5), ..s };
        assert!((v.ratio_at(0.1, 0.75) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn empty_and_odd_input() {
        assert_eq!(window_layout(&json!({})), WindowLayout::default());
        let w = json!({"layers":[{"kind":"tiled","bounds":[0,0,1,1],"layout":null,"blocks":[]}]});
        assert!(window_layout(&w).panes.is_empty());
    }
}
