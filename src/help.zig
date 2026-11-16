//! The key manual (F1, Ctrl-b ?, or the "? keys" button): every key and mouse action,
//! grouped, in one or two columns, scrollable.

const std = @import("std");
const vaxis = @import("vaxis");

const i18n = @import("i18n.zig");
const tr = i18n.t;

/// title and rows in both languages: { key, English, 中文 }
const Section = struct { en: []const u8, zh: []const u8, rows: []const [3][]const u8 };

pub const sections = [_]Section{
    .{ .en = "Always (no prefix)", .zh = "随时可用(不用前缀)", .rows = &.{
        .{ "F1", "open / close this manual", "打开 / 关闭本手册" },
        .{ "Ctrl-V", "paste (an image is uploaded)", "粘贴;剪贴板是图片时上传图片并填入路径" },
        .{ "Alt-1…9", "go to tab N", "切换到第 N 个 tab" },
        .{ "Alt-←↑→↓", "go to the pane that way", "切换到那个方向的 pane" },
    } },
    .{ .en = "Prefix Ctrl-b, then: tabs and panes", .zh = "前缀 Ctrl-b,然后按:tab 和 pane", .rows = &.{
        .{ "c", "new tab", "新建 tab" },
        .{ "n  p", "next / previous tab", "下一个 / 上一个 tab" },
        .{ "1…9", "tab N", "第 N 个 tab" },
        .{ "%", "split right", "向右分屏" },
        .{ "\"", "split down", "向下分屏" },
        .{ "←↑→↓  o", "move between panes (way / in turn)", "切换 pane(方向 / 依次)" },
        .{ "z", "zoom the pane / restore", "放大当前 pane / 还原" },
        .{ "x", "close the pane", "关闭当前 pane" },
        .{ ",", "rename the tab", "重命名 tab" },
    } },
    .{ .en = "Prefix Ctrl-b, then: sessions", .zh = "前缀 Ctrl-b,然后按:session", .rows = &.{
        .{ "(  )", "previous / next session", "上一个 / 下一个 session" },
        .{ "N", "new session", "新建 session" },
        .{ "$", "rename the session", "重命名 session" },
        .{ "s", "show / hide the sidebar", "显示 / 隐藏侧栏" },
    } },
    .{ .en = "Prefix Ctrl-b, then: other", .zh = "前缀 Ctrl-b,然后按:其他", .rows = &.{
        .{ "Space w :", "palette: find anything", "命令面板:搜索 session、tab、机器" },
        .{ "[", "scrollback (look back, search)", "回滚模式(往回看输出、搜索)" },
        .{ "S", "settings (language, machines…)", "设置:语言、机器、Rex 服务端" },
        .{ "?", "this manual", "本手册" },
        .{ "d", "leave rexa (programs keep running)", "退出 rexa(程序继续在 Rex 里运行)" },
        .{ "Ctrl-b", "send Ctrl-b to the program", "把 Ctrl-b 发给程序" },
    } },
    .{ .en = "Scrollback (Ctrl-b [)", .zh = "回滚模式(Ctrl-b [)", .rows = &.{
        .{ "↑↓  j k", "one line", "滚一行" },
        .{ "PgUp PgDn", "one page (also b f)", "滚一页(也可 b f)" },
        .{ "Ctrl-u Ctrl-d", "half a page", "滚半页" },
        .{ "g  G", "top / back to live", "到最上 / 回到最新" },
        .{ "[  ]", "previous / next prompt", "上一条 / 下一条命令提示符" },
        .{ "/", "search", "搜索" },
        .{ "n  N", "older / newer match", "更早 / 更新的匹配" },
        .{ "q  Esc", "leave scrollback", "退出回滚模式" },
    } },
    .{ .en = "Mouse", .zh = "鼠标", .rows = &.{
        .{ "click", "a pane, tab, session or machine", "切换 pane、tab、session、机器" },
        .{ "drag a separator", "resize the split", "调整分屏大小" },
        .{ "drag", "select text and copy it", "选中文字并复制到剪贴板" },
        .{ "double / triple", "select a word / a line and copy", "选中一个词 / 一整行并复制" },
        .{ "middle", "paste the selection", "粘贴选中的文字" },
        .{ "right", "menu: copy, paste, split, close…", "菜单:复制、粘贴、分屏、关闭…" },
        .{ "wheel", "scroll back; on the tab bar: tabs", "往回翻输出;在 tab 栏上切换 tab" },
        .{ "× on a tab", "close it (or middle-click the tab)", "关闭 tab(中键点 tab 也可以)" },
        .{ "double-click tab", "rename it", "重命名 tab" },
        .{ "+", "new tab / session / machine", "新建 tab / session / 机器" },
        .{ "Shift + mouse", "if the program uses the mouse (vim…)", "程序自己用鼠标时(vim、htop…)," },
        .{ "", "Shift gets rexa's selection, menus", "按住 Shift 用 rexa 的选中和右键菜单" },
    } },
};

pub const Help = struct {
    scroll: u16 = 0,
    /// screen rect after the last draw
    box: struct { x: u16 = 0, y: u16 = 0, w: u16 = 0, h: u16 = 0 } = .{},
    max_scroll: u16 = 0,

    pub const Result = enum { none, close };

    pub fn key(self: *Help, k: vaxis.Key) Result {
        switch (k.codepoint) {
            vaxis.Key.escape, 'q', '?', vaxis.Key.f1, vaxis.Key.enter => return .close,
            vaxis.Key.up, 'k' => self.scrollBy(-1),
            vaxis.Key.down, 'j' => self.scrollBy(1),
            vaxis.Key.page_up, 'b' => self.scrollBy(-10),
            vaxis.Key.page_down, ' ', 'f' => self.scrollBy(10),
            vaxis.Key.home, 'g' => self.scroll = 0,
            vaxis.Key.end, 'G' => self.scroll = self.max_scroll,
            else => if (k.matches('c', .{ .ctrl = true }) or k.matches('g', .{ .ctrl = true })) return .close,
        }
        return .none;
    }

    fn scrollBy(self: *Help, d: i32) void {
        const v = std.math.clamp(@as(i32, self.scroll) + d, 0, @as(i32, self.max_scroll));
        self.scroll = @intCast(v);
    }

    pub fn mouse(self: *Help, m: vaxis.Mouse, x: u16, y: u16) Result {
        switch (m.button) {
            .wheel_up => self.scrollBy(-3),
            .wheel_down => self.scrollBy(3),
            .left, .right, .middle => if (m.type == .press) {
                const b = self.box;
                const in = x >= b.x and y >= b.y and x < b.x + b.w and y < b.y + b.h;
                // the close button in the top-right corner, or anywhere outside
                if (!in or (y == b.y and x + 5 >= b.x + b.w)) return .close;
            },
            else => {},
        }
        return .none;
    }

    fn sectionLines(s: Section) u16 {
        return @intCast(s.rows.len + 2); // title + rows + blank
    }

    pub fn draw(self: *Help, screen: vaxis.Window, arena: std.mem.Allocator) void {
        const w: u16 = @min(screen.width -| 2, 120);
        const h: u16 = screen.height -| 2;
        if (w < 30 or h < 6) return;
        const bx = (screen.width - w) / 2;
        const by: u16 = 1;
        self.box = .{ .x = bx, .y = by, .w = w, .h = h };
        const border: vaxis.Style = .{ .fg = .{ .index = 39 }, .bg = .{ .index = 234 } };
        const box = screen.child(.{ .x_off = bx, .y_off = by, .width = w, .height = h, .border = .{ .where = .all, .style = border, .glyphs = .single_rounded } });
        const bg: vaxis.Style = .{ .bg = .{ .index = 234 }, .fg = .{ .index = 252 } };
        box.fill(.{ .style = bg });
        // title and close button on the top border
        const top = screen.child(.{ .x_off = bx, .y_off = by, .width = w, .height = 1 });
        _ = top.print(&.{.{ .text = tr(" rexa keys ", " rexa 快捷键手册 "), .style = .{ .bg = .{ .index = 31 }, .fg = .{ .index = 255 }, .bold = true } }}, .{ .wrap = .none, .col_offset = 2 });
        _ = top.print(&.{.{ .text = " × ", .style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 255 }, .bold = true } }}, .{ .wrap = .none, .col_offset = w -| 5 });
        const foot = tr("Esc / F1 close · ↑↓ wheel scroll", "Esc / F1 关闭 · ↑↓ 滚轮 翻页");
        const bottom = screen.child(.{ .x_off = bx, .y_off = by + h - 1, .width = w, .height = 1 });
        _ = bottom.print(&.{.{ .text = foot, .style = .{ .bg = border.bg, .fg = .{ .index = 244 } } }}, .{ .wrap = .none, .col_offset = w -| (bottom.gwidth(foot) + 2) });

        // one column, or two when there's room (sections split to balance the lengths)
        const two = box.width >= 90;
        var total: u16 = 0;
        for (sections) |s| total += sectionLines(s);
        var split: usize = sections.len;
        if (two) {
            var acc: u16 = 0;
            var best: u16 = std.math.maxInt(u16);
            for (sections, 0..) |s, i| {
                acc += sectionLines(s);
                const other = total - acc;
                const longest = @max(acc, other);
                if (longest < best) {
                    best = longest;
                    split = i + 1;
                }
            }
        }
        const col_w: u16 = if (two) box.width / 2 else box.width;
        var lines_left: u16 = 0;
        for (sections[0..split]) |s| lines_left += sectionLines(s);
        const tallest = if (two) @max(lines_left, total - lines_left) else total;
        self.max_scroll = tallest -| box.height;
        if (self.scroll > self.max_scroll) self.scroll = self.max_scroll;
        drawColumn(box.child(.{ .width = col_w }), sections[0..split], self.scroll, arena);
        if (two) drawColumn(box.child(.{ .x_off = col_w, .width = box.width - col_w }), sections[split..], self.scroll, arena);
    }

    fn drawColumn(win: vaxis.Window, secs: []const Section, scroll: u16, arena: std.mem.Allocator) void {
        _ = arena;
        const bg = vaxis.Color{ .index = 234 };
        const head: vaxis.Style = .{ .bg = bg, .fg = .{ .index = 214 }, .bold = true };
        const keyst: vaxis.Style = .{ .bg = bg, .fg = .{ .index = 117 }, .bold = true };
        const desc: vaxis.Style = .{ .bg = bg, .fg = .{ .index = 252 } };
        var line: u16 = 0;
        for (secs) |s| {
            put(win, line, scroll, &.{.{ .text = tr(s.en, s.zh), .style = head }}, 1);
            line += 1;
            for (s.rows) |r| {
                put(win, line, scroll, &.{.{ .text = r[0], .style = keyst }}, 3);
                put(win, line, scroll, &.{.{ .text = tr(r[1], r[2]), .style = desc }}, 21);
                line += 1;
            }
            line += 1;
        }
    }

    fn put(win: vaxis.Window, line: u16, scroll: u16, segs: []const vaxis.Segment, col: u16) void {
        if (line < scroll) return;
        const row = line - scroll;
        if (row >= win.height) return;
        _ = win.child(.{ .y_off = row, .height = 1 }).print(segs, .{ .wrap = .none, .col_offset = col });
    }
};
