//! Command palette / go-to-anything (Ctrl-b Space): fuzzy-filter a list of sessions,
//! tabs, hosts and commands; or prompt for a line of text (renames).

const std = @import("std");
const vaxis = @import("vaxis");
const i18n = @import("i18n.zig");
const tr = i18n.t;

pub const Kind = enum { session, tab, host, command };

pub const Item = struct {
    kind: Kind,
    label: []const u8,
    /// session/window id, host endpoint spec, or command name
    arg: []const u8,
    hint: []const u8 = "",
    /// also matched by the filter (the English name of a translated command)
    alias: []const u8 = "",
};

pub fn kindName(k: Kind) []const u8 {
    return switch (k) {
        .session => "session",
        .tab => "tab",
        .host => tr("machine", "机器"),
        .command => tr("command", "命令"),
    };
}

/// Subsequence match, case-insensitive. Higher is better; null = no match.
pub fn score(query: []const u8, text: []const u8) ?i32 {
    if (query.len == 0) return 0;
    var qi: usize = 0;
    var s: i32 = 0;
    var last: ?usize = null;
    for (text, 0..) |c, i| {
        if (qi < query.len and std.ascii.toLower(c) == std.ascii.toLower(query[qi])) {
            s += 10;
            if (last != null and last.? + 1 == i) s += 15; // consecutive
            if (i == 0 or text[i - 1] == ' ' or text[i - 1] == ':' or text[i - 1] == '-' or text[i - 1] == '/') s += 20; // word start
            last = i;
            qi += 1;
        }
    }
    if (qi < query.len) return null;
    return s - @divTrunc(@as(i32, @intCast(@min(text.len, 200))), 4);
}

pub const Palette = struct {
    gpa: std.mem.Allocator,
    arena: std.heap.ArenaAllocator,
    items: std.ArrayList(Item) = .empty,
    query: std.ArrayList(u8) = .empty,
    shown: std.ArrayList(usize) = .empty,
    sel: usize = 0,
    /// prompt mode: asks for text; `prompt_for` says what it is for
    prompt: ?[]const u8 = null,
    prompt_for: []const u8 = "",
    /// prompt mode: the window or session id being renamed
    target: []const u8 = "",
    /// after draw: screen position of the list's first row, its size, first shown index
    list_x: u16 = 0,
    list_y: u16 = 0,
    list_w: u16 = 0,
    list_h: u16 = 0,
    list_first: usize = 0,
    box_x: u16 = 0,
    box_y: u16 = 0,
    box_w: u16 = 0,
    box_h: u16 = 0,

    pub fn init(gpa: std.mem.Allocator) Palette {
        return .{ .gpa = gpa, .arena = .init(gpa) };
    }

    pub fn deinit(self: *Palette) void {
        self.items.deinit(self.gpa);
        self.query.deinit(self.gpa);
        self.shown.deinit(self.gpa);
        self.arena.deinit();
    }

    /// Adds an item; strings are copied.
    pub fn add(self: *Palette, kind: Kind, label: []const u8, arg: []const u8, hint: []const u8) void {
        const a = self.arena.allocator();
        self.items.append(self.gpa, .{
            .kind = kind,
            .label = a.dupe(u8, label) catch return,
            .arg = a.dupe(u8, arg) catch return,
            .hint = a.dupe(u8, hint) catch return,
        }) catch {};
    }

    /// Sets the last added item's alias.
    pub fn alias(self: *Palette, text: []const u8) void {
        if (self.items.items.len == 0) return;
        self.items.items[self.items.items.len - 1].alias = self.arena.allocator().dupe(u8, text) catch "";
    }

    pub fn startPrompt(self: *Palette, title: []const u8, for_what: []const u8, initial: []const u8) void {
        const a = self.arena.allocator();
        self.prompt = a.dupe(u8, title) catch "";
        self.prompt_for = a.dupe(u8, for_what) catch "";
        self.query.clearRetainingCapacity();
        self.query.appendSlice(self.gpa, initial) catch {};
    }

    pub fn filter(self: *Palette) void {
        self.shown.clearRetainingCapacity();
        const Scored = struct { i: usize, s: i32 };
        var tmp: std.ArrayList(Scored) = .empty;
        defer tmp.deinit(self.gpa);
        for (self.items.items, 0..) |it, i| {
            var buf: [256]u8 = undefined;
            const text = std.fmt.bufPrint(&buf, "{s}: {s} {s}", .{ @tagName(it.kind), it.label, it.alias }) catch it.label;
            if (score(self.query.items, text)) |s| tmp.append(self.gpa, .{ .i = i, .s = s }) catch {};
        }
        std.mem.sort(Scored, tmp.items, {}, struct {
            fn lt(_: void, a: Scored, b: Scored) bool {
                return a.s > b.s;
            }
        }.lt);
        for (tmp.items) |t| self.shown.append(self.gpa, t.i) catch {};
        if (self.sel >= self.shown.items.len) self.sel = 0;
    }

    pub const Result = union(enum) { none, cancel, pick: Item, text: []const u8 };

    pub fn key(self: *Palette, k: vaxis.Key) Result {
        if (k.codepoint == vaxis.Key.escape or k.matches('c', .{ .ctrl = true }) or k.matches('g', .{ .ctrl = true })) return .cancel;
        if (k.codepoint == vaxis.Key.enter) {
            if (self.prompt != null) return .{ .text = self.query.items };
            if (self.shown.items.len == 0) return .none;
            return .{ .pick = self.items.items[self.shown.items[self.sel]] };
        }
        if (k.codepoint == vaxis.Key.up or k.matches('p', .{ .ctrl = true })) {
            if (self.sel > 0) self.sel -= 1;
            return .none;
        }
        if (k.codepoint == vaxis.Key.down or k.matches('n', .{ .ctrl = true }) or k.codepoint == vaxis.Key.tab) {
            if (self.sel + 1 < self.shown.items.len) self.sel += 1;
            return .none;
        }
        if (k.codepoint == vaxis.Key.backspace) {
            if (self.query.items.len > 0) {
                // drop one UTF-8 character
                var n = self.query.items.len - 1;
                while (n > 0 and (self.query.items[n] & 0xC0) == 0x80) n -= 1;
                self.query.shrinkRetainingCapacity(n);
            }
        } else if (k.matches('u', .{ .ctrl = true })) {
            self.query.clearRetainingCapacity();
        } else if (k.text) |t| {
            if (!k.mods.ctrl and !k.mods.alt) self.query.appendSlice(self.gpa, t) catch {};
        } else if (!k.mods.ctrl and !k.mods.alt and k.codepoint >= 0x20 and k.codepoint < vaxis.Key.insert) {
            var b: [4]u8 = undefined;
            const n = std.unicode.utf8Encode(k.codepoint, &b) catch 0;
            self.query.appendSlice(self.gpa, b[0..n]) catch {};
        }
        if (self.prompt == null) {
            self.sel = 0;
            self.filter();
        }
        return .none;
    }

    /// Pasted text: typed into the query, one line.
    pub fn insert(self: *Palette, text: []const u8) void {
        for (text) |c| {
            if (c == '\n' or c == '\r' or c == '\t') continue;
            self.query.append(self.gpa, c) catch return;
        }
        if (self.prompt == null) {
            self.sel = 0;
            self.filter();
        }
    }

    pub fn inside(self: *const Palette, x: u16, y: u16) bool {
        return x >= self.box_x and y >= self.box_y and x < self.box_x + self.box_w and y < self.box_y + self.box_h;
    }

    /// Mouse over the palette: hover/wheel move the selection, a click picks.
    pub fn mouse(self: *Palette, m: vaxis.Mouse, x: u16, y: u16) Result {
        switch (m.button) {
            .wheel_up => if (self.sel > 0) {
                self.sel -= 1;
            },
            .wheel_down => if (self.sel + 1 < self.shown.items.len) {
                self.sel += 1;
            },
            else => {},
        }
        if (m.button == .wheel_up or m.button == .wheel_down) return .none;
        if (m.type == .press and !self.inside(x, y)) return .cancel;
        if (self.prompt != null) return .none;
        const in_list = x >= self.list_x and x < self.list_x + self.list_w and y >= self.list_y and y < self.list_y + self.list_h;
        if (!in_list) return .none;
        const i = self.list_first + (y - self.list_y);
        if (i >= self.shown.items.len) return .none;
        switch (m.type) {
            .motion => self.sel = i,
            .press => if (m.button == .left) {
                self.sel = i;
                return .{ .pick = self.items.items[self.shown.items[i]] };
            },
            else => {},
        }
        return .none;
    }

    pub fn draw(self: *Palette, area: vaxis.Window, arena: std.mem.Allocator) void {
        const w: u16 = @min(area.width -| 4, 72);
        const max_rows: u16 = @min(area.height -| 4, 16);
        const h: u16 = if (self.prompt != null) 3 else max_rows + 3;
        const box = area.child(.{
            .x_off = @intCast((area.width -| w) / 2),
            .y_off = @intCast(@min(2, area.height -| h)),
            .width = w,
            .height = h,
            .border = .{ .where = .all, .style = .{ .fg = .{ .index = 39 } }, .glyphs = .single_rounded },
        });
        const bg: vaxis.Style = .{ .bg = .{ .index = 235 }, .fg = .{ .index = 252 } };
        box.fill(.{ .style = bg });
        self.box_x = @intCast(box.x_off - 1);
        self.box_y = @intCast(box.y_off - 1);
        self.box_w = w;
        self.box_h = h;
        const title = self.prompt orelse "›";
        const r = box.print(&.{
            .{ .text = title, .style = .{ .bg = bg.bg, .fg = .{ .index = 39 }, .bold = true } },
            .{ .text = " ", .style = bg },
            .{ .text = arena.dupe(u8, self.query.items) catch "", .style = bg },
        }, .{ .wrap = .none });
        box.showCursor(r.col, 0);
        if (self.prompt != null) {
            _ = box.child(.{ .y_off = 1, .height = 1 }).print(&.{.{ .text = tr("Enter to apply, Esc to cancel", "Enter 确定,Esc 取消"), .style = .{ .bg = bg.bg, .fg = .{ .index = 244 } } }}, .{ .wrap = .none });
            return;
        }
        const list = box.child(.{ .y_off = 1, .height = box.height -| 1 });
        const first: usize = if (self.sel >= max_rows) self.sel + 1 - max_rows else 0;
        self.list_x = @intCast(list.x_off);
        self.list_y = @intCast(list.y_off);
        self.list_w = list.width;
        self.list_h = list.height;
        self.list_first = first;
        var row: u16 = 0;
        for (self.shown.items[first..], first..) |idx, i| {
            if (row >= list.height) break;
            const it = self.items.items[idx];
            const st: vaxis.Style = if (i == self.sel) .{ .bg = .{ .index = 31 }, .fg = .{ .index = 255 }, .bold = true } else bg;
            const line = list.child(.{ .y_off = row, .height = 1 });
            line.fill(.{ .style = st });
            const kind_style: vaxis.Style = .{ .bg = st.bg, .fg = .{ .index = switch (it.kind) {
                .session => 114,
                .tab => 110,
                .host => 180,
                .command => 244,
            } } };
            _ = line.print(&.{
                .{ .text = " ", .style = st },
                .{ .text = kindName(it.kind), .style = kind_style },
                .{ .text = "          "[0..@min(10, 9 -| line.gwidth(kindName(it.kind)))], .style = st },
                .{ .text = it.label, .style = st },
                .{ .text = if (it.hint.len > 0) "  " else "", .style = st },
                .{ .text = it.hint, .style = .{ .bg = st.bg, .fg = .{ .index = 244 } } },
            }, .{ .wrap = .none });
            row += 1;
        }
        if (self.shown.items.len == 0) _ = list.print(&.{.{ .text = tr(" no matches", " 没有匹配"), .style = .{ .bg = bg.bg, .fg = .{ .index = 244 } } }}, .{ .wrap = .none });
    }
};

test "fuzzy score" {
    try std.testing.expect(score("ns", "command: new session") != null);
    try std.testing.expect(score("xyz", "session: t1") == null);
    // word starts beat scattered letters
    try std.testing.expect(score("nt", "command: new tab").? > score("nt", "command: zoom pane in tab").?);
}
