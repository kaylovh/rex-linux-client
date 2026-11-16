//! Turns a Rex window (from `session.view`) into cell rectangles: one per visible block,
//! plus the separator lines between tiled blocks. Pure functions, no I/O.

const std = @import("std");
const json = std.json;
const control = @import("control.zig");

pub const Rect = struct {
    x: u16,
    y: u16,
    w: u16,
    h: u16,

    pub fn contains(r: Rect, x: u16, y: u16) bool {
        return x >= r.x and y >= r.y and x < r.x + r.w and y < r.y + r.h;
    }
};

pub const Placement = struct {
    block_id: []const u8,
    /// cells the terminal gets
    content: Rect,
    /// tiled leaf (its outer rect, borders included) or floating box
    outer: Rect,
    floating: bool,
    /// whether the block's creator is the terminal
    terminal: bool,
    label: []const u8,
};

/// A border between two tiled parts that the mouse can drag.
pub const Split = struct {
    /// "horizontal": a vertical line between left and right; "vertical": top/bottom
    horizontal: bool,
    /// any block on each side (session.resize_pane finds the split from them)
    before_block: []const u8,
    after_block: []const u8,
    /// the split's whole area, in cells
    rect: Rect,
    /// the separator's column (horizontal) or row (vertical)
    line: u16,

    /// The before part's share if the separator moved to cell `pos` (x or y).
    pub fn ratioAt(self: Split, pos: u16) f64 {
        const start: f64 = @floatFromInt(if (self.horizontal) self.rect.x else self.rect.y);
        const len: f64 = @floatFromInt(if (self.horizontal) self.rect.w else self.rect.h);
        if (len < 2) return 0.5;
        const r = (@as(f64, @floatFromInt(pos)) + 1 - start) / len;
        return std.math.clamp(r, 0.05, 0.95);
    }
};

pub const Layout = struct {
    placements: std.ArrayList(Placement) = .empty,
    splits: std.ArrayList(Split) = .empty,
    /// separator orientation per cell: bit0 vertical line, bit1 horizontal line
    borders: []u2 = &.{},
    width: u16 = 0,
    height: u16 = 0,

    fn orient(self: *const Layout, x: i32, y: i32) u2 {
        if (x < 0 or y < 0 or x >= self.width or y >= self.height) return 0;
        return self.borders[@as(usize, @intCast(y)) * self.width + @as(usize, @intCast(x))];
    }

    /// Line directions at a separator cell (0 if it isn't one), from its neighbors.
    pub fn borderAt(self: *const Layout, x: u16, y: u16) u4 {
        const o = self.orient(x, y);
        if (o == 0) return 0;
        const xi: i32 = x;
        const yi: i32 = y;
        var f: u4 = 0;
        if (self.orient(xi, yi - 1) & V != 0) f |= UP;
        if (self.orient(xi, yi + 1) & V != 0) f |= DOWN;
        if (self.orient(xi - 1, yi) & H != 0) f |= LEFT;
        if (self.orient(xi + 1, yi) & H != 0) f |= RIGHT;
        if (f == 0) f = if (o & V != 0) UP | DOWN else LEFT | RIGHT;
        return f;
    }

    /// The innermost split whose separator runs through (x, y).
    pub fn splitAt(self: *const Layout, x: u16, y: u16) ?Split {
        var best: ?Split = null;
        for (self.splits.items) |sp| {
            const on = if (sp.horizontal)
                x == sp.line and y >= sp.rect.y and y < sp.rect.y + sp.rect.h
            else
                y == sp.line and x >= sp.rect.x and x < sp.rect.x + sp.rect.w;
            if (!on) continue;
            if (best == null or @as(u32, sp.rect.w) * sp.rect.h < @as(u32, best.?.rect.w) * best.?.rect.h) best = sp;
        }
        return best;
    }

    pub fn find(self: *const Layout, bid: []const u8) ?Placement {
        for (self.placements.items) |p| if (std.mem.eql(u8, p.block_id, bid)) return p;
        return null;
    }

    /// The topmost placement under a cell (floating first).
    pub fn hit(self: *const Layout, x: u16, y: u16) ?Placement {
        var i = self.placements.items.len;
        while (i > 0) {
            i -= 1;
            const p = self.placements.items[i];
            if (p.outer.contains(x, y)) return p;
        }
        return null;
    }
};

const UP: u4 = 1;
const DOWN: u4 = 2;
const LEFT: u4 = 4;
const RIGHT: u4 = 8;
const V: u2 = 1;
const H: u2 = 2;

pub fn borderChar(f: u4) []const u8 {
    return switch (f) {
        UP | DOWN, UP, DOWN => "│",
        LEFT | RIGHT, LEFT, RIGHT => "─",
        UP | DOWN | LEFT | RIGHT => "┼",
        UP | DOWN | RIGHT => "├",
        UP | DOWN | LEFT => "┤",
        DOWN | LEFT | RIGHT => "┬",
        UP | LEFT | RIGHT => "┴",
        DOWN | RIGHT => "┌",
        DOWN | LEFT => "┐",
        UP | RIGHT => "└",
        UP | LEFT => "┘",
        else => " ",
    };
}

fn scale(f: f64, n: u16) u16 {
    const v = @round(f * @as(f64, @floatFromInt(n)));
    return @intFromFloat(std.math.clamp(v, 0, @as(f64, @floatFromInt(n))));
}

fn rectOf(r: []json.Value, w: u16, h: u16) ?Rect {
    if (r.len != 4) return null;
    const fx = num(r[0]);
    const fy = num(r[1]);
    const fw = num(r[2]);
    const fh = num(r[3]);
    const x0 = scale(fx, w);
    const y0 = scale(fy, h);
    const x1 = scale(fx + fw, w);
    const y1 = scale(fy + fh, h);
    return .{ .x = x0, .y = y0, .w = x1 -| x0, .h = y1 -| y0 };
}

fn num(v: json.Value) f64 {
    return switch (v) {
        .integer => |i| @floatFromInt(i),
        .float => |f| f,
        else => 0,
    };
}

fn blockInfo(layer: json.Value, bid: []const u8) struct { terminal: bool, label: []const u8 } {
    for (control.getArr(layer, "blocks")) |b| {
        if (control.eqlStr(b, "block_id", bid)) {
            const creator = control.getStr(b, "creator_name") orelse "";
            return .{ .terminal = std.mem.eql(u8, creator, "com.superlogical.terminal") or creator.len == 0, .label = control.getStr(b, "label") orelse "" };
        }
    }
    return .{ .terminal = true, .label = "" };
}

/// Lays out `window` (a session.view window object) in a w x h area.
pub fn compute(arena: std.mem.Allocator, window: json.Value, w: u16, h: u16) !Layout {
    var out: Layout = .{ .width = w, .height = h };
    out.borders = try arena.alloc(u2, @as(usize, w) * h);
    @memset(out.borders, 0);
    if (w == 0 or h == 0) return out;

    const layers = control.getArr(window, "layers");
    if (control.getStr(window, "zoomed_block_id")) |z| {
        for (layers) |layer| {
            for (control.getArr(layer, "blocks")) |b| if (control.eqlStr(b, "block_id", z)) {
                const info = blockInfo(layer, z);
                const full: Rect = .{ .x = 0, .y = 0, .w = w, .h = h };
                try out.placements.append(arena, .{ .block_id = z, .content = full, .outer = full, .floating = false, .terminal = info.terminal, .label = info.label });
                return out;
            };
        }
    }

    for (layers) |layer| {
        if (!control.eqlStr(layer, "kind", "tiled")) continue;
        const root = control.getObj(layer, "layout") orelse continue;
        try walk(arena, &out, layer, root, w, h);
    }
    for (layers) |layer| {
        if (control.eqlStr(layer, "kind", "tiled")) continue;
        if (control.eqlStr(layer, "state", "minimized")) continue;
        const b = rectOf(control.getArr(layer, "bounds"), w, h) orelse continue;
        if (b.w < 4 or b.h < 3) continue;
        const root = control.getObj(layer, "layout") orelse continue;
        // A floating layer: a box with its own (usually single-block) layout inside.
        var leaves: std.ArrayList(json.Value) = .empty;
        try collectLeaves(arena, root, &leaves);
        for (leaves.items) |leaf| {
            const bid = control.getStr(leaf, "block_id") orelse continue;
            const info = blockInfo(layer, bid);
            try out.placements.append(arena, .{
                .block_id = bid,
                .outer = b,
                .content = .{ .x = b.x + 1, .y = b.y + 1, .w = b.w - 2, .h = b.h - 2 },
                .floating = true,
                .terminal = info.terminal,
                .label = info.label,
            });
            break; // one block per floating box
        }
    }
    return out;
}

fn collectLeaves(arena: std.mem.Allocator, node: json.Value, out: *std.ArrayList(json.Value)) !void {
    if (control.getObj(node, "split")) |s| {
        if (control.getObj(s, "before")) |b| try collectLeaves(arena, b, out);
        if (control.getObj(s, "after")) |a| try collectLeaves(arena, a, out);
    } else if (control.getStr(node, "block_id") != null) try out.append(arena, node);
}

fn firstLeaf(node: json.Value) ?[]const u8 {
    if (control.getObj(node, "split")) |s| {
        if (control.getObj(s, "before")) |b| if (firstLeaf(b)) |id| return id;
        if (control.getObj(s, "after")) |a| return firstLeaf(a);
        return null;
    }
    return control.getStr(node, "block_id");
}

fn walk(arena: std.mem.Allocator, out: *Layout, layer: json.Value, node: json.Value, w: u16, h: u16) !void {
    if (control.getObj(node, "split")) |s| {
        split: {
            const before = control.getObj(s, "before") orelse break :split;
            const after = control.getObj(s, "after") orelse break :split;
            const r = rectOf(control.getArr(node, "rect"), w, h) orelse break :split;
            const br = rectOf(control.getArr(before, "rect"), w, h) orelse break :split;
            const horizontal = control.eqlStr(s, "direction", "horizontal");
            const end = if (horizontal) br.x + br.w else br.y + br.h;
            if (end == 0) break :split;
            try out.splits.append(arena, .{
                .horizontal = horizontal,
                .before_block = firstLeaf(before) orelse break :split,
                .after_block = firstLeaf(after) orelse break :split,
                .rect = r,
                .line = end - 1,
            });
        }
        if (control.getObj(s, "before")) |b| try walk(arena, out, layer, b, w, h);
        if (control.getObj(s, "after")) |a| try walk(arena, out, layer, a, w, h);
        return;
    }
    const bid = control.getStr(node, "block_id") orelse return;
    const r = rectOf(control.getArr(node, "rect"), w, h) orelse return;
    if (r.w == 0 or r.h == 0) return;
    const right_border = r.x + r.w < w;
    const bottom_border = r.y + r.h < h;
    const content: Rect = .{
        .x = r.x,
        .y = r.y,
        .w = if (right_border) r.w -| 1 else r.w,
        .h = if (bottom_border) r.h -| 1 else r.h,
    };
    if (right_border) {
        const cx = r.x + r.w - 1;
        var y = r.y;
        while (y < r.y + r.h) : (y += 1) out.borders[@as(usize, y) * w + cx] |= V;
    }
    if (bottom_border) {
        const cy = r.y + r.h - 1;
        var x = r.x;
        while (x < r.x + r.w) : (x += 1) out.borders[@as(usize, cy) * w + x] |= H;
    }
    const info = blockInfo(layer, bid);
    try out.placements.append(arena, .{ .block_id = bid, .content = content, .outer = r, .floating = false, .terminal = info.terminal, .label = info.label });
}

test "three-way split" {
    const src =
        \\{"window_id":"w","layers":[{"kind":"tiled","bounds":[0,0,1,1],"layout":{"rect":[0,0,1,1],"split":{"direction":"horizontal","before":{"rect":[0,0,0.5,1],"split":{"direction":"vertical","before":{"rect":[0,0,0.5,0.5],"block_id":"a"},"after":{"rect":[0,0.5,0.5,0.5],"block_id":"b"}}},"after":{"rect":[0.5,0,0.5,1],"block_id":"c"}}},"blocks":[]}]}
    ;
    var arena: std.heap.ArenaAllocator = .init(std.testing.allocator);
    defer arena.deinit();
    const parsed = try json.parseFromSliceLeaky(json.Value, arena.allocator(), src, .{});
    const l = try compute(arena.allocator(), parsed, 80, 24);
    try std.testing.expectEqual(@as(usize, 3), l.placements.items.len);
    const a = l.find("a").?.content;
    try std.testing.expectEqual(Rect{ .x = 0, .y = 0, .w = 39, .h = 11 }, a);
    const c = l.find("c").?.content;
    try std.testing.expectEqual(Rect{ .x = 40, .y = 0, .w = 40, .h = 24 }, c);
    // vertical separator at column 39, T-junction where the horizontal one meets it
    try std.testing.expectEqualStrings("│", borderChar(l.borderAt(39, 3)));
    try std.testing.expectEqualStrings("┤", borderChar(l.borderAt(39, 11)));
    try std.testing.expectEqualStrings("─", borderChar(l.borderAt(10, 11)));
    // the separator cells belong to the right splits
    const outer = l.splitAt(39, 3).?;
    try std.testing.expect(outer.horizontal);
    try std.testing.expectEqualStrings("a", outer.before_block);
    try std.testing.expectEqualStrings("c", outer.after_block);
    const inner = l.splitAt(10, 11).?;
    try std.testing.expect(!inner.horizontal);
    try std.testing.expectEqualStrings("b", inner.after_block);
    try std.testing.expect(l.splitAt(10, 3) == null);
    try std.testing.expectApproxEqAbs(@as(f64, 0.5), outer.ratioAt(39), 0.001);
}
