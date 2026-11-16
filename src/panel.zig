//! Server panel (Ctrl-b S): what the connected Rex server is doing, its log, and
//! reload / stop / restart / update. Like `rex server ...`, minus install.

const std = @import("std");
const vaxis = @import("vaxis");
const control = @import("control.zig");
const transport = @import("transport.zig");
const i18n = @import("i18n.zig");
const t = i18n.t;

pub const Action = enum { none, stop, restart };

pub const Panel = struct {
    gpa: std.mem.Allocator,
    status: ?control.Result = null,
    version: ?control.Result = null,
    clients: ?control.Result = null,
    log: ?[]u8 = null,
    log_note: []const u8 = "",
    confirm: Action = .none,
    message: [200]u8 = undefined,
    message_len: usize = 0,
    /// clickable buttons after the last draw: screen row, and (x0, x1, key) each
    button_row: u16 = 0,
    buttons: [6]Button = undefined,
    button_count: usize = 0,

    pub const Button = struct { x0: u16, x1: u16, key: u21 };

    /// The key of the button under a screen cell, if any.
    pub fn buttonAt(self: *const Panel, x: u16, y: u16) ?u21 {
        if (y != self.button_row) return null;
        for (self.buttons[0..self.button_count]) |b| if (x >= b.x0 and x < b.x1) return b.key;
        return null;
    }

    pub fn deinit(self: *Panel) void {
        if (self.status) |r| r.deinit();
        if (self.version) |r| r.deinit();
        if (self.clients) |r| r.deinit();
        if (self.log) |l| self.gpa.free(l);
        self.* = .{ .gpa = self.gpa };
    }

    pub fn say(self: *Panel, comptime fmt: []const u8, args: anytype) void {
        const m = std.fmt.bufPrint(&self.message, fmt, args) catch self.message[0..0];
        self.message_len = m.len;
    }

    pub fn sayT(self: *Panel, comptime en: []const u8, comptime zh: []const u8, args: anytype) void {
        self.message_len = i18n.bufFmt(&self.message, en, zh, args).len;
    }

    /// Re-reads status, version, clients and the log.
    pub fn refresh(self: *Panel, ctl: *control.Control, io: std.Io, ep: transport.Endpoint) void {
        if (self.status) |r| r.deinit();
        if (self.version) |r| r.deinit();
        if (self.clients) |r| r.deinit();
        self.status = ctl.call("server.status", "{}") catch null;
        self.version = ctl.call("server.version", "{}") catch null;
        self.clients = ctl.call("client.list", "{}") catch null;
        if (self.log) |l| self.gpa.free(l);
        self.log = self.readLog(io, ep);
    }

    /// The log through whatever can reach it: `rex server logs` here or over ssh,
    /// else the systemd journal.
    fn readLog(self: *Panel, io: std.Io, ep: transport.Endpoint) ?[]u8 {
        var argv: std.ArrayList([]const u8) = .empty;
        defer argv.deinit(self.gpa);
        switch (ep) {
            .unix => |path| {
                // `rex server logs` reads this machine's default server only.
                if (!isDefaultSocket(path)) {
                    self.log_note = t("log not available for a server on a custom socket", "自定义 socket 上的服务端看不到日志");
                    return null;
                }
                argv.appendSlice(self.gpa, &.{ "rex", "server", "logs", "-n", "40" }) catch return null;
            },
            .command => |c| {
                if (c.len >= 2 and std.mem.eql(u8, c[0], "ssh")) {
                    // same ssh options and host, different remote command
                    argv.appendSlice(self.gpa, c[0 .. c.len - 1]) catch return null;
                    argv.append(self.gpa, transport.Endpoint.remote_path ++ "rex server logs -n 40 2>&1 || journalctl --user -u rex -n 40 --no-pager") catch return null;
                } else {
                    self.log_note = t("log not available for this connection", "这种连接方式看不到日志");
                    return null;
                }
            },
        }
        const out = run(self.gpa, io, argv.items);
        if (out) |o| if (o.len > 0) {
            self.log_note = "";
            return o;
        } else self.gpa.free(o);
        if (ep == .unix) {
            if (run(self.gpa, io, &.{ "journalctl", "--user", "-u", "rex", "-n", "40", "--no-pager" })) |j| return j;
        }
        self.log_note = t("no log found (servers run by systemd log to journalctl --user -u rex)", "没找到日志(由 systemd 运行的服务端,日志在 journalctl --user -u rex)");
        return null;
    }

    pub fn draw(self: *Panel, win: vaxis.Window, arena: std.mem.Allocator, host: []const u8) void {
        const bg: vaxis.Style = .{ .bg = .{ .index = 234 }, .fg = .{ .index = 252 } };
        const key: vaxis.Style = .{ .bg = .{ .index = 234 }, .fg = .{ .index = 110 }, .bold = true };
        const dim: vaxis.Style = .{ .bg = .{ .index = 234 }, .fg = .{ .index = 244 } };
        const head: vaxis.Style = .{ .bg = .{ .index = 31 }, .fg = .{ .index = 255 }, .bold = true };
        win.fill(.{ .style = bg });
        var row: u16 = 0;
        const title = std.fmt.allocPrint(arena, " {s} — {s} ", .{ t("Rex server", "Rex 服务端"), if (host.len > 0) host else t("this computer", "本机") }) catch "";
        line(win, row, &.{.{ .text = title, .style = head }});
        row += 2;

        const st = if (self.status) |s| s.value else std.json.Value{ .null = {} };
        const ver = if (self.version) |v| v.value else std.json.Value{ .null = {} };
        const fields = [_]struct { []const u8, []const u8 }{
            .{ t("version", "版本"), std.fmt.allocPrint(arena, "{s}  (api {d}, {s})", .{ control.getStr(ver, "rex_version") orelse "?", control.getInt(ver, "api_revision") orelse 0, control.getStr(ver, "platform") orelse "" }) catch "" },
            .{ t("process", "进程"), i18n.fmt(arena, "pid {d}, started {s}", "pid {d},启动于 {s}", .{ control.getInt(st, "pid") orelse 0, control.getStr(st, "started_at") orelse "?" }) },
            .{ "sessions", std.fmt.allocPrint(arena, "{d}{s}", .{ control.getInt(st, "session_count") orelse 0, if (control.getBool(st, "exit_empty") orelse false) t("  (stops when empty)", "  (没有 session 时自动停止)") else "" }) catch "" },
            .{ t("config", "配置"), configLine(arena, st) },
            .{ "tailscale", if (control.getObj(st, "tailscale")) |ts| control.getStr(ts, "state") orelse "?" else t("not available", "不可用") },
            .{ t("clients", "客户端"), clientsLine(arena, if (self.clients) |c| c.value else .null) },
        };
        for (fields) |f| {
            line(win, row, &.{ .{ .text = std.fmt.allocPrint(arena, "  {s}", .{f[0]}) catch "", .style = dim }, .{ .text = "            "[0..12 -| win.gwidth(f[0])], .style = dim }, .{ .text = f[1], .style = bg } });
            row += 1;
        }
        row += 1;
        {
            // buttons: click them or press their key
            const defs = [_]struct { []const u8, []const u8, u21 }{
                .{ "c", t("reload config", "重新加载配置"), 'c' },
                .{ "l", t("refresh", "刷新"), 'l' },
                .{ "R", t("restart", "重启"), 'R' },
                .{ "X", t("stop", "停止"), 'X' },
                .{ "U", t("update (new tab)", "更新(在新 tab 里)"), 'U' },
                .{ "Esc", t("close", "关闭"), vaxis.Key.escape },
            };
            const chip: vaxis.Style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 252 } };
            const chip_key: vaxis.Style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 117 }, .bold = true };
            var col: u16 = 2;
            self.button_row = @intCast(win.y_off + row);
            self.button_count = 0;
            const lw = win.child(.{ .y_off = row, .height = 1 });
            for (defs) |d| {
                const x0 = col;
                const r = lw.print(&.{
                    .{ .text = " ", .style = chip },
                    .{ .text = d[0], .style = chip_key },
                    .{ .text = " ", .style = chip },
                    .{ .text = d[1], .style = chip },
                    .{ .text = " ", .style = chip },
                }, .{ .wrap = .none, .col_offset = col });
                self.buttons[self.button_count] = .{ .x0 = @intCast(win.x_off + x0), .x1 = @intCast(win.x_off + r.col), .key = d[2] };
                self.button_count += 1;
                col = r.col + 2;
            }
            _ = key;
        }
        row += 1;
        if (self.message_len > 0) {
            line(win, row, &.{.{ .text = arena.dupe(u8, self.message[0..self.message_len]) catch "", .style = .{ .bg = .{ .index = 234 }, .fg = .{ .index = 222 } } }});
        }
        row += 2;
        if (self.confirm != .none) {
            const warn: vaxis.Style = .{ .bg = .{ .index = 124 }, .fg = .{ .index = 255 }, .bold = true };
            const what = if (self.confirm == .stop) t("STOP", "停止") else t("RESTART", "重启");
            line(win, row, &.{.{ .text = i18n.fmt(arena, "  {s} the server? Every program running in its sessions (shells, Claude, builds) will end. y = yes, anything else = cancel ", "  {s}服务端?所有 session 里运行的程序(shell、Claude、编译…)都会结束。y = 确定,其他键 = 取消 ", .{what}), .style = warn }});
            row += 1;
            // while asking, the clickable buttons are yes / cancel
            const lw = win.child(.{ .y_off = row, .height = 1 });
            const yes = lw.print(&.{.{ .text = i18n.fmt(arena, " y  yes, {s} ", " y  确定{s} ", .{what}), .style = warn }}, .{ .wrap = .none, .col_offset = 2 });
            const no = lw.print(&.{.{ .text = t(" n  cancel ", " n  取消 "), .style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 252 } } }}, .{ .wrap = .none, .col_offset = yes.col + 2 });
            self.button_row = @intCast(win.y_off + row);
            self.buttons[0] = .{ .x0 = @intCast(win.x_off + 2), .x1 = @intCast(win.x_off + yes.col), .key = 'y' };
            self.buttons[1] = .{ .x0 = @intCast(win.x_off + yes.col + 2), .x1 = @intCast(win.x_off + no.col), .key = 'n' };
            self.button_count = 2;
            row += 2;
        }
        line(win, row, &.{.{ .text = t("  log", "  日志"), .style = dim }});
        row += 1;
        if (self.log) |l| {
            // newest lines that fit
            var lines: std.ArrayList([]const u8) = .empty;
            var it = std.mem.splitScalar(u8, std.mem.trimEnd(u8, l, "\n"), '\n');
            while (it.next()) |ln| lines.append(arena, ln) catch {};
            const room = win.height -| row;
            const start = lines.items.len -| room;
            for (lines.items[start..]) |ln| {
                line(win, row, &.{.{ .text = std.fmt.allocPrint(arena, "  {s}", .{ln}) catch "", .style = dim }});
                row += 1;
            }
        } else line(win, row, &.{.{ .text = std.fmt.allocPrint(arena, "  {s}", .{self.log_note}) catch "", .style = dim }});
    }
};

fn isDefaultSocket(path: []const u8) bool {
    var buf: [64]u8 = undefined;
    const default = std.fmt.bufPrint(&buf, "/run/user/{d}/rex/server.sock", .{std.os.linux.getuid()}) catch return false;
    return std.mem.eql(u8, path, default);
}

fn line(win: vaxis.Window, row: u16, segs: []const vaxis.Segment) void {
    if (row >= win.height) return;
    _ = win.child(.{ .y_off = row, .height = 1 }).print(segs, .{ .wrap = .none });
}

fn configLine(arena: std.mem.Allocator, st: std.json.Value) []const u8 {
    const c = control.getObj(st, "config") orelse return "?";
    const path = control.getStr(c, "path") orelse t("(no file)", "(没有配置文件)");
    return i18n.fmt(arena, "{s}  (loaded {d}×)", "{s}  (已加载 {d} 次)", .{ path, control.getInt(c, "generation") orelse 0 });
}

fn clientsLine(arena: std.mem.Allocator, v: std.json.Value) []const u8 {
    var w: std.Io.Writer.Allocating = .init(arena);
    var n: usize = 0;
    for (control.getArr(v, "clients")) |c| {
        if (control.eqlStr(c, "connection_state", "lost")) continue;
        const info = control.getObj(c, "info") orelse c;
        const name = control.getStr(c, "label") orelse control.getStr(info, "name") orelse "?";
        const kind = control.getStr(info, "kind") orelse "";
        const plat = control.getStr(info, "platform") orelse "";
        w.writer.print("{s}{s} ({s}, {s})", .{ if (n > 0) ", " else "", name, kind, plat }) catch {};
        n += 1;
    }
    if (n == 0) return t("none", "无");
    return w.written();
}

fn run(gpa: std.mem.Allocator, io: std.Io, argv: []const []const u8) ?[]u8 {
    const r = std.process.run(gpa, io, .{ .argv = argv, .stdout_limit = .limited(256 * 1024) }) catch return null;
    defer gpa.free(r.stderr);
    const ok = switch (r.term) {
        .exited => |c| c == 0,
        else => false,
    };
    if (!ok) {
        gpa.free(r.stdout);
        return null;
    }
    return r.stdout;
}

/// argv that starts the server again after a stop, if we know how for this endpoint.
pub fn startCommand(arena: std.mem.Allocator, ep: transport.Endpoint, env: *const std.process.Environ.Map) ?[]const []const u8 {
    switch (ep) {
        .unix => |path| {
            // Only the default local server: a custom socket was started some custom way.
            _ = env;
            if (!isDefaultSocket(path)) return null;
            return arena.dupe([]const u8, &.{ "rex", "server", "start" }) catch null;
        },
        .command => |c| {
            if (c.len >= 2 and std.mem.eql(u8, c[0], "ssh")) {
                var argv: std.ArrayList([]const u8) = .empty;
                argv.appendSlice(arena, c[0 .. c.len - 1]) catch return null;
                argv.append(arena, transport.Endpoint.remote_path ++ "rex server start") catch return null;
                return argv.items;
            }
            return null;
        },
    }
}

/// Shell script for the update tab: runs on the server's machine.
pub const update_script =
    \\echo "Updating Rex on $(hostname)…"
    \\if command -v dnf >/dev/null 2>&1; then sudo dnf upgrade --refresh -y rex
    \\elif command -v apt-get >/dev/null 2>&1; then sudo apt-get update && sudo apt-get install --only-upgrade -y rex
    \\elif command -v pacman >/dev/null 2>&1; then sudo pacman -Syu rex
    \\else echo "No supported package manager found; update Rex the way you installed it."
    \\fi
    \\echo; rex version; echo
    \\echo "Restart the server (rexa: Ctrl-b S, then R) to run the new version."
    \\printf "Press Enter to close. "; read _
;
