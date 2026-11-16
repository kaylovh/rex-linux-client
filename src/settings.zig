//! Settings (status-line ⚙, Ctrl-b S): pages on the left, the page on the right.
//!   General  — the language
//!   Machines — this computer and the saved remote machines (nicknames, addresses)
//!   Server   — the connected Rex server (panel.zig draws it)

const std = @import("std");
const vaxis = @import("vaxis");
const i18n = @import("i18n.zig");
const t = i18n.t;

pub const Page = enum { general, machines, server };
const pages = [_]Page{ .general, .machines, .server };

pub const Source = enum { local, saved, rex_host, adhoc };

/// One machine rexa can connect to. `spec` is what main.zig connects with: "" for this
/// computer, else an endpoint (ssh://…, a `rex hosts` label, …).
pub const Machine = struct {
    name: []const u8,
    spec: []const u8,
    source: Source,
    current: bool = false,
    /// index in the config's saved list, for .saved
    index: usize = 0,
};

pub const Hit = union(enum) {
    page: Page,
    lang: i18n.Lang,
    machine: usize,
    /// a button: the key it stands for
    key: u21,
};

const Rect = struct { x0: u16, x1: u16, y: u16, hit: Hit };

const bg_i = 234;
const bg: vaxis.Style = .{ .bg = .{ .index = bg_i }, .fg = .{ .index = 252 } };
const dim: vaxis.Style = .{ .bg = .{ .index = bg_i }, .fg = .{ .index = 244 } };
const head: vaxis.Style = .{ .bg = .{ .index = bg_i }, .fg = .{ .index = 214 }, .bold = true };
const sel_st: vaxis.Style = .{ .bg = .{ .index = 31 }, .fg = .{ .index = 255 }, .bold = true };
const chip: vaxis.Style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 252 } };
const chip_key: vaxis.Style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 117 }, .bold = true };

pub const nav_width = 18;

pub const Settings = struct {
    page: Page = .general,
    /// selected row on the Machines page
    sel: usize = 0,
    /// `sel` starts on the current machine
    sel_set: bool = false,
    hits: [48]Rect = undefined,
    hit_count: usize = 0,

    pub fn pageName(p: Page) []const u8 {
        return switch (p) {
            .general => t("General", "通用"),
            .machines => t("Machines", "机器"),
            .server => t("Rex server", "Rex 服务端"),
        };
    }

    pub fn nextPage(self: *Settings, d: i32) void {
        const n: i32 = pages.len;
        self.page = @enumFromInt(@mod(@as(i32, @intFromEnum(self.page)) + d, n));
    }

    fn addHit(self: *Settings, x0: u16, x1: u16, y: u16, hit: Hit) void {
        if (self.hit_count >= self.hits.len) return;
        self.hits[self.hit_count] = .{ .x0 = x0, .x1 = x1, .y = y, .hit = hit };
        self.hit_count += 1;
    }

    pub fn hitAt(self: *const Settings, x: u16, y: u16) ?Hit {
        for (self.hits[0..self.hit_count]) |r| if (y == r.y and x >= r.x0 and x < r.x1) return r.hit;
        return null;
    }

    /// Draws the frame and the General / Machines page; returns the content window,
    /// where the caller draws the Server page.
    pub fn draw(self: *Settings, area: vaxis.Window, arena: std.mem.Allocator, machines: []const Machine, config_path: []const u8, save_error: ?[]const u8) vaxis.Window {
        self.hit_count = 0;
        area.fill(.{ .style = bg });
        const title = std.fmt.allocPrint(arena, " ⚙ {s} ", .{t("Settings", "设置")}) catch "";
        _ = area.print(&.{
            .{ .text = title, .style = .{ .bg = .{ .index = 31 }, .fg = .{ .index = 255 }, .bold = true } },
            .{ .text = t("   Tab next page · Esc close", "   Tab 切换页面 · Esc 关闭"), .style = dim },
        }, .{ .wrap = .none });
        // nav
        const nw: u16 = @min(nav_width, area.width / 3);
        for (pages, 0..) |p, i| {
            const row: u16 = 2 + @as(u16, @intCast(i));
            if (row < area.height) {
                const st = if (p == self.page) sel_st else bg;
                const nl = area.child(.{ .y_off = row, .width = nw, .height = 1 });
                nl.fill(.{ .style = st });
                _ = nl.print(&.{.{ .text = std.fmt.allocPrint(arena, " {d} {s}", .{ i + 1, pageName(p) }) catch "", .style = st }}, .{ .wrap = .none });
                self.addHit(@intCast(area.x_off), @intCast(area.x_off + nw), @intCast(area.y_off + row), .{ .page = p });
            }
        }
        var y: u16 = 2;
        while (y < area.height) : (y += 1) area.writeCell(nw, y, .{ .char = .{ .grapheme = "│" }, .style = .{ .bg = bg.bg, .fg = .{ .index = 240 } } });
        const content = area.child(.{ .x_off = nw + 2, .y_off = 2, .width = area.width -| (nw + 2), .height = area.height -| 2 });
        switch (self.page) {
            .general => self.drawGeneral(content, arena, config_path, save_error),
            .machines => self.drawMachines(content, arena, machines),
            .server => {},
        }
        return content;
    }

    fn chips(self: *Settings, win: vaxis.Window, row: u16, defs: []const struct { []const u8, []const u8, u21 }) void {
        if (row >= win.height) return;
        const lw = win.child(.{ .y_off = row, .height = 1 });
        var col: u16 = 0;
        for (defs) |d| {
            const x0 = col;
            const r = lw.print(&.{
                .{ .text = " ", .style = chip },
                .{ .text = d[0], .style = chip_key },
                .{ .text = " ", .style = chip },
                .{ .text = d[1], .style = chip },
                .{ .text = " ", .style = chip },
            }, .{ .wrap = .none, .col_offset = col });
            self.addHit(@intCast(win.x_off + x0), @intCast(win.x_off + r.col), @intCast(win.y_off + row), .{ .key = d[2] });
            col = r.col + 2;
        }
    }

    fn drawGeneral(self: *Settings, win: vaxis.Window, arena: std.mem.Allocator, config_path: []const u8, save_error: ?[]const u8) void {
        line(win, 0, &.{.{ .text = t("Language", "语言"), .style = head }});
        const lw = win.child(.{ .y_off = 2, .height = 1 });
        var col: u16 = 2;
        for ([_]struct { i18n.Lang, []const u8 }{ .{ .zh, " 中文 " }, .{ .en, " English " } }) |o| {
            const on = i18n.lang == o[0];
            const r = lw.print(&.{
                .{ .text = if (on) "◉" else "○", .style = .{ .bg = bg.bg, .fg = .{ .index = if (on) 39 else 244 } } },
                .{ .text = o[1], .style = if (on) sel_st else chip },
            }, .{ .wrap = .none, .col_offset = col });
            self.addHit(@intCast(win.x_off + col), @intCast(win.x_off + r.col), @intCast(win.y_off + 2), .{ .lang = o[0] });
            col = r.col + 3;
        }
        line(win, 3, &.{.{ .text = t("  ←→ or click to change; applies at once", "  ←→ 或点击切换,立即生效"), .style = dim }});
        line(win, 5, &.{.{ .text = t("Settings file", "设置文件"), .style = head }});
        line(win, 6, &.{.{ .text = std.fmt.allocPrint(arena, "  {s}", .{config_path}) catch "", .style = bg }});
        if (save_error) |e| line(win, 7, &.{.{ .text = std.fmt.allocPrint(arena, "  {s}: {s}", .{ t("saving failed", "保存失败"), e }) catch "", .style = .{ .bg = bg.bg, .fg = .{ .index = 203 } } }});
    }

    fn drawMachines(self: *Settings, win: vaxis.Window, arena: std.mem.Allocator, machines: []const Machine) void {
        if (self.sel >= machines.len) self.sel = machines.len -| 1;
        line(win, 0, &.{.{ .text = t("Machines", "机器"), .style = head }});
        line(win, 1, &.{.{ .text = t("  The sidebar lists these; click one there to switch to it.", "  侧栏会列出这些机器,在侧栏点一下就切换过去。"), .style = dim }});
        const name_w: u16 = 20;
        line(win, 3, &.{
            .{ .text = std.fmt.allocPrint(arena, "    {s}", .{t("Name", "名字")}) catch "", .style = dim },
        });
        _ = win.child(.{ .y_off = 3, .height = 1 }).print(&.{.{ .text = t("Address", "地址"), .style = dim }}, .{ .wrap = .none, .col_offset = name_w + 4 });
        var row: u16 = 4;
        for (machines, 0..) |m, i| {
            if (row + 4 >= win.height) break;
            const st = if (i == self.sel) sel_st else bg;
            const lw = win.child(.{ .y_off = row, .height = 1 });
            lw.fill(.{ .style = st });
            var name = m.name;
            if (lw.gwidth(name) > name_w) name = name[0..@min(name.len, name_w)];
            _ = lw.print(&.{
                .{ .text = if (m.current) " ● " else "   ", .style = .{ .bg = st.bg, .fg = .{ .index = 114 }, .bold = true } },
                .{ .text = name, .style = st },
            }, .{ .wrap = .none });
            const addr = switch (m.source) {
                .local => t("this computer", "这台电脑"),
                else => m.spec,
            };
            const tag = switch (m.source) {
                .local, .saved => "",
                .rex_host => t("  (from rex hosts)", "  (来自 rex hosts)"),
                .adhoc => t("  (not saved)", "  (未保存)"),
            };
            _ = lw.print(&.{
                .{ .text = addr, .style = st },
                .{ .text = tag, .style = .{ .bg = st.bg, .fg = .{ .index = if (i == self.sel) 252 else 244 } } },
                .{ .text = if (m.current) t("   connected", "   已连接") else "", .style = .{ .bg = st.bg, .fg = .{ .index = 114 } } },
            }, .{ .wrap = .none, .col_offset = name_w + 4 });
            self.addHit(@intCast(win.x_off), @intCast(win.x_off + win.width), @intCast(win.y_off + row), .{ .machine = i });
            row += 1;
        }
        row += 1;
        const cur = if (self.sel < machines.len) machines[self.sel] else null;
        var defs: std.ArrayList(struct { []const u8, []const u8, u21 }) = .empty;
        defs.append(arena, .{ "a", t("add", "添加"), 'a' }) catch {};
        if (cur) |m| {
            if (!m.current) defs.append(arena, .{ "Enter", t("connect", "连接"), vaxis.Key.enter }) catch {};
            switch (m.source) {
                .saved => {
                    defs.append(arena, .{ "r", t("rename", "改名字"), 'r' }) catch {};
                    defs.append(arena, .{ "e", t("edit address", "改地址"), 'e' }) catch {};
                    defs.append(arena, .{ "d", t("delete", "删除"), 'd' }) catch {};
                },
                .rex_host, .adhoc => defs.append(arena, .{ "s", t("save with a name", "保存并命名"), 's' }) catch {},
                .local => {},
            }
        }
        self.chips(win, row, defs.items);
        row += 2;
        line(win, row, &.{.{ .text = t("Address forms:", "地址写法:"), .style = dim }});
        row += 1;
        const forms = [_][]const u8{
            t("  ssh://user@host[:port]   any machine with Rex, over ssh (needs key login)", "  ssh://用户@主机[:端口]   任何装了 Rex 的机器,走 ssh(需要密钥登录)"),
            t("  a label from `rex hosts`, a Tailscale name, host:port", "  `rex hosts` 里的名字、Tailscale 名字、主机:端口"),
            t("  unix:///path/to/server.sock", "  unix:///路径/server.sock"),
        };
        for (forms) |f| {
            line(win, row, &.{.{ .text = f, .style = dim }});
            row += 1;
        }
    }
};

fn line(win: vaxis.Window, row: u16, segs: []const vaxis.Segment) void {
    if (row >= win.height) return;
    _ = win.child(.{ .y_off = row, .height = 1 }).print(segs, .{ .wrap = .none });
}
