//! A small popup menu at the mouse (right-click menus, confirmations): a list of items,
//! picked with a click or with ↑↓ Enter; Esc or a click outside closes it.

const std = @import("std");
const vaxis = @import("vaxis");

pub const Item = struct {
    label: []const u8,
    /// shown dimmed on the right, e.g. the key that does the same
    hint: []const u8 = "",
    /// what the app does when this item is picked
    action: Action = .none,
    /// a separator line instead of an item
    sep: bool = false,
    danger: bool = false,
};

pub const Action = enum {
    none,
    copy,
    paste,
    split_right,
    split_down,
    zoom,
    close_pane,
    confirm_close_pane,
    new_tab,
    rename_tab,
    close_tab,
    confirm_close_tab,
    new_session,
    rename_session,
    switch_session,
    destroy_session,
    confirm_destroy_session,
    scroll_mode,
    search,
    palette,
    help,
    server_panel,
    toggle_sidebar,
    detach,
    exit_scroll,
    settings,
    /// machines: the menu's target is "machine:<spec>"
    connect_machine,
    rename_machine,
    edit_machine,
    save_machine,
    confirm_delete_machine,
    delete_machine,
    add_machine,
};

pub const Menu = struct {
    arena: std.heap.ArenaAllocator,
    items: std.ArrayList(Item) = .empty,
    title: []const u8 = "",
    /// what the menu is about: a block, window or session id (copied)
    target: []const u8 = "",
    /// where it was opened (screen cells); the box is placed from here
    x: u16,
    y: u16,
    sel: ?usize = null,
    /// screen rect of the box after the last draw (for clicks)
    box: struct { x: u16 = 0, y: u16 = 0, w: u16 = 0, h: u16 = 0 } = .{},

    pub fn init(gpa: std.mem.Allocator, x: u16, y: u16, title: []const u8, target: []const u8) Menu {
        var m: Menu = .{ .arena = .init(gpa), .x = x, .y = y };
        const a = m.arena.allocator();
        m.title = a.dupe(u8, title) catch "";
        m.target = a.dupe(u8, target) catch "";
        return m;
    }

    pub fn deinit(self: *Menu) void {
        self.arena.deinit();
    }

    pub fn add(self: *Menu, label: []const u8, hint: []const u8, action: Action) void {
        const a = self.arena.allocator();
        self.items.append(a, .{ .label = a.dupe(u8, label) catch "", .hint = a.dupe(u8, hint) catch "", .action = action }) catch {};
    }

    pub fn addDanger(self: *Menu, label: []const u8, hint: []const u8, action: Action) void {
        self.add(label, hint, action);
        self.items.items[self.items.items.len - 1].danger = true;
    }

    pub fn separator(self: *Menu) void {
        self.items.append(self.arena.allocator(), .{ .label = "", .sep = true }) catch {};
    }

    pub const Result = union(enum) { none, close, pick: Action };

    pub fn key(self: *Menu, k: vaxis.Key) Result {
        if (k.codepoint == vaxis.Key.escape or k.matches('c', .{ .ctrl = true }) or k.matches('g', .{ .ctrl = true }) or k.codepoint == 'q') return .close;
        if (k.codepoint == vaxis.Key.up or k.codepoint == 'k') self.move(-1) else if (k.codepoint == vaxis.Key.down or k.codepoint == 'j' or k.codepoint == vaxis.Key.tab) self.move(1) else if (k.codepoint == vaxis.Key.enter or k.codepoint == ' ') {
            if (self.sel) |i| return .{ .pick = self.items.items[i].action };
        }
        return .none;
    }

    fn move(self: *Menu, d: i32) void {
        const n = self.items.items.len;
        if (n == 0) return;
        var i: i32 = if (self.sel) |s| @intCast(s) else if (d > 0) -1 else @intCast(n);
        var tries: usize = 0;
        while (tries < n) : (tries += 1) {
            i = @mod(i + d, @as(i32, @intCast(n)));
            if (!self.items.items[@intCast(i)].sep) {
                self.sel = @intCast(i);
                return;
            }
        }
    }

    fn itemAt(self: *Menu, x: u16, y: u16) ?usize {
        const b = self.box;
        if (x <= b.x or x + 1 >= b.x + b.w or y <= b.y or y + 1 >= b.y + b.h) return null;
        const top = b.y + 1 + @as(u16, if (self.title.len > 0) 1 else 0);
        if (y < top) return null;
        const i: usize = y - top;
        if (i >= self.items.items.len or self.items.items[i].sep) return null;
        return i;
    }

    pub fn inside(self: *Menu, x: u16, y: u16) bool {
        const b = self.box;
        return x >= b.x and y >= b.y and x < b.x + b.w and y < b.y + b.h;
    }

    pub fn mouse(self: *Menu, m: vaxis.Mouse, x: u16, y: u16) Result {
        switch (m.type) {
            .motion, .drag => self.sel = self.itemAt(x, y) orelse self.sel,
            .press => {
                if (!self.inside(x, y)) return .close;
                if (m.button == .left or m.button == .right) self.sel = self.itemAt(x, y);
            },
            .release => if (m.button == .left or m.button == .right) {
                if (self.itemAt(x, y)) |i| return .{ .pick = self.items.items[i].action };
            },
        }
        return .none;
    }

    pub fn draw(self: *Menu, screen: vaxis.Window, arena: std.mem.Allocator) void {
        var inner: u16 = screen.gwidth(self.title) + 2;
        for (self.items.items) |it| {
            const w = screen.gwidth(it.label) + (if (it.hint.len > 0) screen.gwidth(it.hint) + 3 else 0) + 2;
            inner = @max(inner, w);
        }
        inner = @min(inner, screen.width -| 2);
        const h: u16 = @intCast(@min(self.items.items.len + 2 + @as(usize, if (self.title.len > 0) 1 else 0), screen.height));
        const w = inner + 2;
        var bx = self.x;
        var by = self.y;
        if (bx + w > screen.width) bx = screen.width -| w;
        if (by + h > screen.height) by = if (self.y >= h) self.y + 1 - h else screen.height -| h;
        self.box = .{ .x = bx, .y = by, .w = w, .h = h };
        const box = screen.child(.{
            .x_off = bx,
            .y_off = by,
            .width = w,
            .height = h,
            .border = .{ .where = .all, .style = .{ .fg = .{ .index = 39 }, .bg = .{ .index = 236 } }, .glyphs = .single_rounded },
        });
        const bg: vaxis.Style = .{ .bg = .{ .index = 236 }, .fg = .{ .index = 252 } };
        box.fill(.{ .style = bg });
        var row: u16 = 0;
        if (self.title.len > 0) {
            _ = box.child(.{ .y_off = 0, .height = 1 }).print(&.{.{ .text = arena.dupe(u8, self.title) catch "", .style = .{ .bg = bg.bg, .fg = .{ .index = 244 }, .bold = true } }}, .{ .wrap = .none, .col_offset = 1 });
            row = 1;
        }
        for (self.items.items, 0..) |it, i| {
            if (row >= box.height) break;
            const line = box.child(.{ .y_off = row, .height = 1 });
            row += 1;
            if (it.sep) {
                var x: u16 = 0;
                while (x < line.width) : (x += 1) line.writeCell(x, 0, .{ .char = .{ .grapheme = "─" }, .style = .{ .bg = bg.bg, .fg = .{ .index = 240 } } });
                continue;
            }
            const selected = self.sel != null and self.sel.? == i;
            var st = bg;
            if (it.danger) st.fg = .{ .index = 210 };
            if (selected) st = .{ .bg = .{ .index = if (it.danger) 124 else 31 }, .fg = .{ .index = 255 }, .bold = true };
            line.fill(.{ .style = st });
            _ = line.print(&.{.{ .text = it.label, .style = st }}, .{ .wrap = .none, .col_offset = 1 });
            if (it.hint.len > 0) {
                const hw = line.gwidth(it.hint);
                _ = line.print(&.{.{ .text = it.hint, .style = .{ .bg = st.bg, .fg = .{ .index = if (selected) 252 else 244 } } }}, .{ .wrap = .none, .col_offset = line.width -| (hw + 1) });
            }
        }
    }
};
