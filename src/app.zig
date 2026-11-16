//! The interactive client: one vaxis loop, a control connection, and a Pane per
//! visible terminal block. Screen: [sidebar] | tab bar / window layout / status line.

const std = @import("std");
const vaxis = @import("vaxis");
const control = @import("control.zig");
const transport = @import("transport.zig");
const keys = @import("keys.zig");
const upload = @import("upload.zig");
const layout = @import("layout.zig");
const model = @import("model.zig");
const Pane = @import("pane.zig").Pane;
const panel_mod = @import("panel.zig");
const palette_mod = @import("palette.zig");
const menu_mod = @import("menu.zig");
const help_mod = @import("help.zig");
const settings_mod = @import("settings.zig");
const config_mod = @import("config.zig");
const i18n = @import("i18n.zig");
const tr = i18n.t;
const Machine = settings_mod.Machine;
const json = std.json;

pub const Event = union(enum) {
    key_press: vaxis.Key,
    key_release: vaxis.Key,
    mouse: vaxis.Mouse,
    winsize: vaxis.Winsize,
    focus_in,
    focus_out,
    paste_start,
    paste_end,
    paste: []const u8,
    /// a pane's picture changed (coalesced)
    redraw,
    /// spinner animation
    tick,
    /// server events are waiting in App.inbox
    server_wake,
    /// a background job finished with a message for the status line (owned)
    notice: []u8,
};

pub const Options = struct {
    session: ?[]const u8 = null,
    /// what we connected with: "" for this computer, else the -S value / a machine's address
    spec: []const u8 = "",
    config: *config_mod.Config,
    /// shown in the status line at start (e.g. why switching machines failed)
    notice: []const u8 = "",
};

/// Prefix of the exit message when the user picked another machine; the rest is its spec.
pub const switch_prefix = "switch-host:";

const sidebar_width = 26;
const spinner = [_][]const u8{ "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏" };

const Style = struct {
    const bar: vaxis.Style = .{ .bg = .{ .index = 236 }, .fg = .{ .index = 250 } };
    const tab_active: vaxis.Style = .{ .bg = .{ .index = 31 }, .fg = .{ .index = 255 }, .bold = true };
    const tab: vaxis.Style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 252 } };
    const border: vaxis.Style = .{ .fg = .{ .index = 240 } };
    const border_focus: vaxis.Style = .{ .fg = .{ .index = 39 } };
    const side: vaxis.Style = .{ .bg = .{ .index = 235 }, .fg = .{ .index = 250 } };
    const side_current: vaxis.Style = .{ .bg = .{ .index = 31 }, .fg = .{ .index = 255 }, .bold = true };
    const dim: vaxis.Style = .{ .fg = .{ .index = 244 } };
    const notice: vaxis.Style = .{ .bg = .{ .index = 236 }, .fg = .{ .index = 222 } };
    const prefix: vaxis.Style = .{ .bg = .{ .index = 214 }, .fg = .{ .index = 0 }, .bold = true };
};

fn stateIcon(s: model.State, frame: usize) struct { []const u8, vaxis.Color } {
    return switch (s) {
        .idle => .{ " ", .default },
        .working => .{ spinner[frame % spinner.len], .{ .index = 214 } },
        .blocked => .{ "●", .{ .index = 203 } },
        .err => .{ "✗", .{ .index = 196 } },
        .done => .{ "✓", .{ .index = 114 } },
    };
}

const TabHit = struct { x0: u16, x1: u16, window_id: []const u8, title: []const u8, close_x: u16 };
const Button = struct { x0: u16, x1: u16, action: menu_mod.Action };
/// a clickable sidebar row (screen row)
const SideHit = struct {
    y: u16,
    what: union(enum) { session: []const u8, new_session, machine: []const u8, add_machine },
};

pub const App = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    env: *std.process.Environ.Map,
    ep: transport.Endpoint,
    ctl: *control.Control,

    tty: vaxis.Tty = undefined,
    tty_buf: [16 * 1024]u8 = undefined,
    vx: vaxis.Vaxis = undefined,
    loop: vaxis.Loop(Event) = undefined,

    running: std.atomic.Value(bool) = .init(true),
    exit_msg: [200]u8 = undefined,
    exit_len: usize = 0,

    sid: ?[]u8 = null,
    view: ?control.Result = null,
    view_stale: bool = true,
    model: model.Model,

    panes: std.StringHashMapUnmanaged(*Pane) = .empty,
    /// blocks whose size we decide; value = last size we asked for
    owned: std.StringHashMapUnmanaged([2]u16) = .empty,
    /// blocks another client took; we take them back on input, not on redraw
    lost: std.StringHashMapUnmanaged(void) = .empty,

    prefix: bool = false,
    /// mouse text selection in progress: pane block id (frame-independent copy) + anchor
    sel_block: ?[]u8 = null,
    sel_anchor: [2]u16 = .{ 0, 0 },
    sel_moved: bool = false,
    /// Ctrl-b Space : command palette over the window area
    palette: ?palette_mod.Palette = null,
    /// Settings (Ctrl-b S, ⚙): replaces the window area
    settings: ?settings_mod.Settings = null,
    /// the Rex server page of Settings, while it is open
    panel: ?panel_mod.Panel = null,
    config: *config_mod.Config,
    /// what we connected with ("" = this computer), see Options.spec
    spec: []const u8 = "",
    /// Ctrl-b [ : keys scroll the focused pane's scrollback instead of typing
    scroll_mode: bool = false,
    pasting: bool = false,
    paste_buf: std.ArrayList(u8) = .empty,
    redraw_pending: std.atomic.Value(bool) = .init(false),
    /// Server events from the control reader. It must never block on the UI queue
    /// (the UI thread may be waiting for a response that this reader delivers).
    inbox: std.ArrayList(control.Parsed) = .empty,
    inbox_lock: std.Io.Mutex = .init,
    inbox_wake: std.atomic.Value(bool) = .init(false),
    /// Events posted before the loop exists are dropped (the view is re-read anyway).
    loop_ready: std.atomic.Value(bool) = .init(false),
    animate: std.atomic.Value(bool) = .init(false),
    ticker: ?std.Thread = null,
    spin: usize = 0,
    notice_buf: [200]u8 = undefined,
    notice_len: usize = 0,
    frame: std.heap.ArenaAllocator,

    sidebar: ?bool = null,
    /// REXA_DEBUG=<file>: log every handled event there
    debug: ?std.Io.File = null,
    /// The block this rexa runs in, when it runs inside a Rex pane: never mirrored
    /// (it would show itself forever).
    self_block: ?[]const u8 = null,
    // hit testing, rebuilt every frame (frame arena)
    last_layout: ?layout.Layout = null,
    area_x: u16 = 0,
    area_y: u16 = 0,
    tab_hits: []TabHit = &.{},
    tab_plus: ?[2]u16 = null,
    tab_y: u16 = 0,
    side_hits: []SideHit = &.{},
    /// label + first endpoint of each host from `hosts.list`
    hosts: std.ArrayList(struct { label: []u8, endpoint: []u8 }) = .empty,
    side_w: u16 = 0,
    /// status line buttons (screen coords)
    status_y: u16 = 0,
    status_buttons: []Button = &.{},

    /// right-click menu / confirmation, drawn over everything
    menu: ?menu_mod.Menu = null,
    /// F1 / Ctrl-b ? : the key manual
    help: ?help_mod.Help = null,
    /// dragging a pane separator: the split and the last position sent
    drag: ?struct { horizontal: bool, before: [160]u8, before_len: usize, after: [160]u8, after_len: usize, rect: layout.Rect, last: u16 } = null,
    /// for double / triple clicks
    last_click: struct { at: i96 = 0, x: u16 = 0, y: u16 = 0, count: u8 = 0 } = .{},
    /// what rexa copied last (middle-click pastes it)
    last_copy: std.ArrayList(u8) = .empty,
    mouse_shape: vaxis.Mouse.Shape = .default,
    last_settings_page: settings_mod.Page = .general,

    /// Runs until detach; returns why it ended, copied into `msg_out`.
    pub fn run(gpa: std.mem.Allocator, io: std.Io, env: *std.process.Environ.Map, ep: transport.Endpoint, opts: Options, msg_out: []u8) ![]const u8 {
        const ctl = try control.Control.connect(gpa, io, ep);
        const app = try gpa.create(App);
        defer gpa.destroy(app);
        app.* = .{ .gpa = gpa, .io = io, .env = env, .ep = ep, .ctl = ctl, .frame = .init(gpa), .model = .{ .gpa = gpa }, .config = opts.config, .spec = opts.spec };
        defer app.frame.deinit();
        defer app.model.deinit();
        app.self_block = env.get("REX_BLOCK");
        if (env.get("REXA_DEBUG")) |path| app.debug = std.Io.Dir.cwd().createFile(io, path, .{}) catch null;
        try ctl.start(onServerEvent, app);

        app.loadSessions();
        app.loadHosts();
        try app.pickSession(opts.session);
        if (opts.notice.len > 0) app.notice("{s}", .{opts.notice});

        app.tty = try vaxis.Tty.init(io, &app.tty_buf);
        defer app.tty.deinit();
        app.vx = try vaxis.init(io, gpa, env, .{ .kitty_keyboard_flags = .{ .report_events = false } });
        app.loop = .init(io, &app.tty, &app.vx);
        try app.loop.start();
        app.loop_ready.store(true, .release);
        app.view_stale = true;
        const w = app.tty.writer();
        try app.vx.enterAltScreen(w);
        try app.vx.queryTerminal(w, .fromSeconds(1));
        try app.vx.setBracketedPaste(w, true);
        try app.vx.setMouseMode(w, true);
        try app.vx.resize(gpa, w, try app.tty.getWinsize());
        app.ticker = std.Thread.spawn(.{}, tickLoop, .{app}) catch null;

        app.mainLoop() catch |e| app.setExit("error: {s}", .{@errorName(e)});

        app.running.store(false, .release);
        if (app.ticker) |t| t.join();
        app.vx.setMouseMode(w, false) catch {};
        app.loop.stop();
        app.vx.deinit(gpa, w);
        w.flush() catch {};
        app.teardown();
        const n = @min(app.exit_len, msg_out.len);
        @memcpy(msg_out[0..n], app.exit_msg[0..n]);
        return msg_out[0..n];
    }

    fn setExit(self: *App, comptime fmt: []const u8, args: anytype) void {
        const m = std.fmt.bufPrint(&self.exit_msg, fmt, args) catch self.exit_msg[0..0];
        self.exit_len = m.len;
        self.running.store(false, .release);
    }

    pub fn notice(self: *App, comptime fmt: []const u8, args: anytype) void {
        const m = std.fmt.bufPrint(&self.notice_buf, fmt, args) catch self.notice_buf[0..0];
        self.notice_len = m.len;
    }

    /// A status-line notice in the current language.
    pub fn say(self: *App, comptime en: []const u8, comptime zh: []const u8, args: anytype) void {
        self.notice_len = i18n.bufFmt(&self.notice_buf, en, zh, args).len;
    }

    fn exitWith(self: *App, comptime en: []const u8, comptime zh: []const u8) void {
        if (i18n.isZh()) self.setExit(zh, .{}) else self.setExit(en, .{});
    }

    fn teardown(self: *App) void {
        self.dropAllPanes();
        self.panes.deinit(self.gpa);
        self.owned.deinit(self.gpa);
        self.lost.deinit(self.gpa);
        if (self.sid) |sid| {
            if (self.call("session.detach", "{{\"session_id\":{f}}}", .{control.q(sid)})) |r| r.deinit();
            self.gpa.free(sid);
        }
        if (self.view) |v| v.deinit();
        self.closePanel();
        self.closePalette();
        self.paste_buf.deinit(self.gpa);
        for (self.hosts.items) |h| {
            self.gpa.free(h.label);
            self.gpa.free(h.endpoint);
        }
        self.hosts.deinit(self.gpa);
        self.ctl.close();
        for (self.inbox.items) |ev| ev.deinit();
        self.inbox.deinit(self.gpa);
    }

    fn tickLoop(self: *App) void {
        while (self.running.load(.acquire)) {
            self.io.sleep(.fromMilliseconds(120), .awake) catch {};
            if (self.animate.load(.acquire) and self.loop_ready.load(.acquire)) _ = self.loop.tryPostEvent(.tick) catch false;
        }
    }

    // ---- server ---------------------------------------------------------------------

    fn onServerEvent(ctx: ?*anyopaque, ev: control.Parsed) void {
        const self: *App = @ptrCast(@alignCast(ctx.?));
        if (!self.loop_ready.load(.acquire)) {
            // Before the UI exists, a lost connection still has to end the run.
            if (control.eqlStr(ev.value, "type", "rexa_disconnected")) self.running.store(false, .release);
            return ev.deinit();
        }
        self.inbox_lock.lockUncancelable(self.io);
        self.inbox.append(self.gpa, ev) catch ev.deinit();
        self.inbox_lock.unlock(self.io);
        if (!self.inbox_wake.swap(true, .acq_rel)) _ = self.loop.tryPostEvent(.server_wake) catch false;
    }

    fn paneRedraw(ctx: *anyopaque) void {
        const self: *App = @ptrCast(@alignCast(ctx));
        if (!self.loop_ready.load(.acquire)) return;
        if (!self.redraw_pending.swap(true, .acq_rel)) {
            _ = self.loop.tryPostEvent(.redraw) catch false;
        }
    }

    pub fn call(self: *App, method: []const u8, comptime fmt: []const u8, args: anytype) ?control.Result {
        var buf: std.Io.Writer.Allocating = .init(self.gpa);
        defer buf.deinit();
        buf.writer.print(fmt, args) catch return null;
        return self.ctl.call(method, buf.written()) catch {
            self.notice("{s}", .{self.ctl.lastError()});
            return null;
        };
    }

    pub fn send(self: *App, method: []const u8, comptime fmt: []const u8, args: anytype) void {
        var buf: std.Io.Writer.Allocating = .init(self.gpa);
        defer buf.deinit();
        buf.writer.print(fmt, args) catch return;
        self.ctl.send(method, buf.written());
    }

    /// Session list plus every block's program status, from the whole server.
    fn loadSessions(self: *App) void {
        const all = self.call("session.attach_all", "{{\"event_scope\":\"all\"}}", .{}) orelse return;
        defer all.deinit();
        for (control.getArr(all.value, "sessions")) |s| {
            const id = control.getStr(s, "session_id") orelse continue;
            self.model.putSession(id, control.getStr(s, "label") orelse "") catch {};
            self.loadStatuses(id);
        }
    }

    fn loadHosts(self: *App) void {
        const r = self.call("hosts.list", "{{}}", .{}) orelse {
            self.notice_len = 0;
            return;
        };
        defer r.deinit();
        for (control.getArr(r.value, "hosts")) |h| {
            const label = control.getStr(h, "label") orelse continue;
            const eps = control.getArr(h, "endpoints");
            if (eps.len == 0 or eps[0] != .string) continue;
            self.hosts.append(self.gpa, .{
                .label = self.gpa.dupe(u8, label) catch continue,
                .endpoint = self.gpa.dupe(u8, eps[0].string) catch continue,
            }) catch {};
        }
    }

    fn loadStatuses(self: *App, sid: []const u8) void {
        const v = self.call("session.view", "{{\"session_id\":{f}}}", .{control.q(sid)}) orelse return;
        defer v.deinit();
        for (control.getArr(v.value, "windows")) |w| {
            for (control.getArr(w, "layers")) |l| for (control.getArr(l, "blocks")) |b| {
                const bid = control.getStr(b, "block_id") orelse continue;
                if (!control.eqlStr(b, "creator_name", "com.superlogical.terminal")) continue;
                const r = self.call("com.superlogical.terminal.program_status", "{{\"session_id\":{f},\"block_id\":{f},\"args\":{{}}}}", .{ control.q(sid), control.q(bid) }) orelse continue;
                defer r.deinit();
                for (control.getArr(r.value, "records")) |rec| {
                    self.model.setStatus(sid, bid, control.getStr(rec, "id") orelse "", model.State.parse(control.getStr(rec, "state") orelse "")) catch {};
                }
            };
        }
        self.notice_len = 0;
    }

    fn pickSession(self: *App, want: ?[]const u8) !void {
        if (want) |q| {
            if (std.mem.startsWith(u8, q, "session:")) return self.switchSession(q);
            const r = self.call("target.resolve", "{{\"kinds\":[\"session\"],\"queries\":[{f}]}}", .{control.q(q)}) orelse return error.NoSession;
            defer r.deinit();
            const res = control.getArr(r.value, "results");
            if (res.len == 0 or !control.eqlStr(res[0], "status", "resolved")) return error.NoSession;
            return self.switchSession(control.getStr(control.getObj(res[0], "target") orelse return error.NoSession, "session_id") orelse return error.NoSession);
        }
        if (self.model.sessions.items.len > 0) return self.switchSession(self.model.sessions.items[0].id);
        return self.newSession();
    }

    fn newSession(self: *App) !void {
        const created = self.call("session.create", "{{\"initial_windows\":[{{\"layout\":{{\"block\":{{\"flavor\":\"com.superlogical.terminal.shell\"}}}}}}]}}", .{}) orelse return error.NoSession;
        defer created.deinit();
        const sid = control.getStr(created.value, "session_id") orelse return error.NoSession;
        self.model.putSession(sid, control.getStr(created.value, "label") orelse "") catch {};
        try self.switchSession(sid);
    }

    pub fn switchSession(self: *App, sid: []const u8) !void {
        if (self.sid) |old| if (std.mem.eql(u8, old, sid)) return;
        const new_sid = try self.gpa.dupe(u8, sid);
        if (self.sid) |old| {
            self.send("session.detach", "{{\"session_id\":{f}}}", .{control.q(old)});
            self.gpa.free(old);
            self.dropAllPanes();
        }
        self.sid = new_sid;
        const att = self.call("session.attach", "{{\"session_id\":{f}}}", .{control.q(new_sid)}) orelse return error.AttachFailed;
        att.deinit();
        self.view_stale = true;
        try self.refreshView();
    }

    fn cycleSession(self: *App, delta: i32) void {
        const n = self.model.sessions.items.len;
        if (n < 2) return;
        const cur = self.model.findSession(self.sid orelse return) orelse 0;
        const next: usize = @intCast(@mod(@as(i64, @intCast(cur)) + delta, @as(i64, @intCast(n))));
        self.switchSession(self.model.sessions.items[next].id) catch {};
    }

    fn dropAllPanes(self: *App) void {
        var it = self.panes.iterator();
        while (it.next()) |e| {
            e.value_ptr.*.destroy();
            self.gpa.free(e.key_ptr.*);
        }
        self.panes.clearRetainingCapacity();
        var oit = self.owned.keyIterator();
        while (oit.next()) |k| self.gpa.free(k.*);
        self.owned.clearRetainingCapacity();
        var lit = self.lost.keyIterator();
        while (lit.next()) |k| self.gpa.free(k.*);
        self.lost.clearRetainingCapacity();
    }

    fn refreshView(self: *App) !void {
        const sid = self.sid orelse return;
        self.view_stale = false;
        const v = self.call("session.view", "{{\"session_id\":{f}}}", .{control.q(sid)}) orelse return;
        if (self.view) |old| old.deinit();
        self.view = v;
        if (control.getStr(v.value, "label")) |l| self.model.putSession(sid, l) catch {};
    }

    fn handleServer(self: *App, ev: control.Parsed) void {
        defer ev.deinit();
        const v = ev.value;
        if (control.eqlStr(v, "type", "rexa_disconnected")) {
            self.exitWith("lost the connection to the Rex server", "和 Rex 服务端的连接断了");
            return;
        }
        if (control.eqlStr(v, "type", "rexa_reconnecting")) {
            self.say("connection lost — reconnecting…", "连接断了,正在重连…", .{});
            return;
        }
        if (control.eqlStr(v, "type", "rexa_reconnected")) {
            if (control.getBool(v, "resumed") orelse false) {
                self.say("reconnected", "已重新连接", .{});
            } else {
                // A new client on the server: attach again and rebuild every pane.
                self.model.clear();
                self.loadSessions();
                self.dropAllPanes();
                if (self.view) |old| old.deinit();
                self.view = null;
                const still_there = if (self.sid) |sid| self.model.findSession(sid) != null else false;
                if (still_there) {
                    if (self.call("session.attach", "{{\"session_id\":{f}}}", .{control.q(self.sid.?)})) |r| r.deinit();
                    self.view_stale = true;
                } else {
                    // The server came back without our session (e.g. it restarted).
                    if (self.sid) |old| self.gpa.free(old);
                    self.sid = null;
                    self.pickSession(null) catch {};
                }
                self.say("reconnected", "已重新连接", .{});
            }
            return;
        }
        const outer = control.getObj(v, "payload") orelse return;
        const otype = control.getStr(outer, "type") orelse return;
        const op = control.getObj(outer, "payload") orelse json.Value{ .null = {} };
        if (std.mem.eql(u8, otype, "session_created")) {
            const id = control.getStr(op, "session_id") orelse return;
            self.model.putSession(id, control.getStr(op, "label") orelse "") catch {};
            return;
        }
        if (std.mem.eql(u8, otype, "session_destroyed")) {
            const id = control.getStr(op, "session_id") orelse return;
            self.model.removeSession(id);
            if (self.sid) |sid| if (std.mem.eql(u8, sid, id)) {
                if (self.model.sessions.items.len == 0) {
                    self.exitWith("session ended", "session 已结束");
                } else self.switchSession(self.model.sessions.items[0].id) catch self.exitWith("session ended", "session 已结束");
            };
            return;
        }
        if (!std.mem.eql(u8, otype, "session_event")) return;
        const ev_sid = control.getStr(op, "session_id") orelse return;
        const inner = control.getObj(op, "event") orelse return;
        const itype = control.getStr(inner, "type") orelse return;
        const ip = control.getObj(inner, "payload") orelse json.Value{ .null = {} };
        const current = if (self.sid) |s| std.mem.eql(u8, s, ev_sid) else false;

        if (std.mem.eql(u8, itype, "block_event")) return self.blockEvent(ev_sid, ip, current);
        if (std.mem.eql(u8, itype, "session_label_changed")) {
            if (control.getStr(ip, "label")) |l| self.model.putSession(ev_sid, l) catch {};
        }
        if (std.mem.eql(u8, itype, "block_closed")) {
            if (control.getStr(ip, "block_id")) |b| self.model.removeBlock(b);
        }
        if (!current) return;
        if (std.mem.eql(u8, itype, "block_owner_changed")) {
            const bid = control.getStr(ip, "block_id") orelse return;
            const owner = control.getStr(ip, "owner") orelse "";
            if (!std.mem.eql(u8, owner, self.ctl.client_id)) {
                if (self.owned.fetchRemove(bid)) |kv| {
                    self.gpa.free(kv.key);
                    if (!self.lost.contains(bid)) {
                        const key = self.gpa.dupe(u8, bid) catch return;
                        self.lost.put(self.gpa, key, {}) catch self.gpa.free(key);
                    }
                }
            }
        } else if (std.mem.eql(u8, itype, "session_view_changed") or
            std.mem.eql(u8, itype, "active_window_changed") or
            std.mem.eql(u8, itype, "window_created") or
            std.mem.eql(u8, itype, "window_closed") or
            std.mem.eql(u8, itype, "block_closed") or
            std.mem.eql(u8, itype, "block_created") or
            std.mem.eql(u8, itype, "window_label_changed"))
        {
            self.view_stale = true;
        }
    }

    fn drainInbox(self: *App) void {
        self.inbox_wake.store(false, .release);
        while (true) {
            self.inbox_lock.lockUncancelable(self.io);
            var batch = self.inbox;
            self.inbox = .empty;
            self.inbox_lock.unlock(self.io);
            if (batch.items.len == 0) {
                batch.deinit(self.gpa);
                return;
            }
            for (batch.items) |ev| self.handleServer(ev);
            batch.deinit(self.gpa);
        }
    }

    fn blockEvent(self: *App, sid: []const u8, p: json.Value, current: bool) void {
        const bid = control.getStr(p, "block_id") orelse return;
        const name = control.getStr(p, "name") orelse return;
        const body = control.getObj(p, "payload") orelse json.Value{ .null = {} };
        if (std.mem.eql(u8, name, "program_status_changed")) {
            const rec = control.getObj(body, "record") orelse return;
            const id = control.getStr(body, "id") orelse control.getStr(rec, "id") orelse "";
            self.model.setStatus(sid, bid, id, model.State.parse(control.getStr(rec, "state") orelse "")) catch {};
        } else if (std.mem.eql(u8, name, "program_status_removed")) {
            for (control.getArr(body, "ids")) |id| if (id == .string) self.model.removeRecord(bid, id.string);
        } else if (current and std.mem.eql(u8, name, "clipboard_written")) {
            // A program copied (OSC 52): put it on this machine's clipboard.
            if (control.getStr(body, "text")) |t| {
                self.vx.copyToSystemClipboard(self.tty.writer(), t, self.gpa) catch {};
            }
        } else if (std.mem.eql(u8, name, "desktop_notification")) {
            const title = control.getStr(body, "title");
            const text = control.getStr(body, "body") orelse "";
            self.vx.notify(self.tty.writer(), title, text) catch {};
        }
        self.animate.store(self.model.anyWorking(), .release);
    }

    // ---- view helpers ---------------------------------------------------------------

    fn activeWindow(self: *App) ?json.Value {
        const v = (self.view orelse return null).value;
        const active = control.getStr(v, "active_window_id") orelse return null;
        for (control.getArr(v, "windows")) |w| if (control.eqlStr(w, "window_id", active)) return w;
        return null;
    }

    fn windows(self: *App) []json.Value {
        const v = (self.view orelse return &.{}).value;
        return control.getArr(v, "windows");
    }

    pub fn focusedBlock(self: *App) ?[]const u8 {
        const w = self.activeWindow() orelse return null;
        return control.getStr(w, "focused_block_id");
    }

    fn windowState(self: *App, w: json.Value) model.State {
        var s: model.State = .idle;
        for (control.getArr(w, "layers")) |l| for (control.getArr(l, "blocks")) |b| {
            if (control.getStr(b, "block_id")) |bid| s = s.max(self.model.blockState(bid));
        };
        return s;
    }

    fn windowTitle(self: *App, w: json.Value) []const u8 {
        const label = control.getStr(w, "label") orelse "";
        // A default label ("Window 1a2b3c4d") says nothing; use the focused pane's title.
        if (label.len == 0 or (std.mem.startsWith(u8, label, "Window ") and label.len == 15)) {
            if (control.getStr(w, "focused_block_id")) |bid| {
                if (self.panes.get(bid)) |p| if (p.title().len > 0) return p.title();
                for (control.getArr(w, "layers")) |l| for (control.getArr(l, "blocks")) |b| {
                    if (control.eqlStr(b, "block_id", bid)) return control.getStr(b, "label") orelse label;
                };
            }
        }
        return label;
    }

    fn paneFor(self: *App, bid: []const u8) ?*Pane {
        if (self.panes.get(bid)) |p| return p;
        const sid = self.sid orelse return null;
        const p = Pane.create(self.gpa, self.io, self.ctl, self.ep, sid, bid, .{ .ctx = self, .redraw = paneRedraw }) catch return null;
        const key = self.gpa.dupe(u8, bid) catch return null;
        self.panes.put(self.gpa, key, p) catch return null;
        return p;
    }

    /// Destroys panes for blocks that aren't shown any more.
    fn prunePanes(self: *App, keep: []const []const u8) void {
        var dead: std.ArrayList([]const u8) = .empty;
        defer dead.deinit(self.gpa);
        var it = self.panes.keyIterator();
        outer: while (it.next()) |k| {
            for (keep) |b| if (std.mem.eql(u8, b, k.*)) continue :outer;
            dead.append(self.gpa, k.*) catch {};
        }
        for (dead.items) |k| {
            const kv = self.panes.fetchRemove(k).?;
            kv.value.destroy();
            self.gpa.free(kv.key);
        }
    }

    /// Takes the block's size and sets it to cols x rows, unless another client took it.
    fn sizeBlock(self: *App, bid: []const u8, cols: u16, rows: u16) void {
        if (cols == 0 or rows == 0) return;
        if (self.lost.contains(bid)) return;
        const sid = self.sid orelse return;
        if (self.owned.get(bid)) |s| if (s[0] == cols and s[1] == rows) return;
        if (!self.owned.contains(bid)) {
            self.send("session.own_block", "{{\"session_id\":{f},\"block_ids\":[{f}],\"mode\":\"take\"}}", .{ control.q(sid), control.q(bid) });
            const key = self.gpa.dupe(u8, bid) catch return;
            self.owned.put(self.gpa, key, .{ cols, rows }) catch {};
        } else self.owned.getPtr(bid).?.* = .{ cols, rows };
        self.send("com.superlogical.terminal.resize", "{{\"session_id\":{f},\"block_id\":{f},\"args\":{{\"columns\":{d},\"rows\":{d}}}}}", .{ control.q(sid), control.q(bid), cols, rows });
    }

    // ---- loop -----------------------------------------------------------------------

    fn mainLoop(self: *App) !void {
        self.render();
        var last_render = std.Io.Clock.awake.now(self.io);
        const frame: std.Io.Duration = .fromMilliseconds(16);
        while (self.running.load(.acquire)) {
            const ev = try self.loop.nextEvent();
            const output_only = ev == .redraw or ev == .tick;
            self.handle(ev);
            while (try self.loop.tryEvent()) |e| self.handle(e);
            self.drainInbox();
            if (self.view_stale) self.refreshView() catch {};
            if (!self.running.load(.acquire)) break;
            // Terminal output alone renders at most ~60 times a second; input and
            // layout changes render at once.
            if (output_only) {
                const since = last_render.durationTo(std.Io.Clock.awake.now(self.io));
                if (since.nanoseconds < frame.nanoseconds) {
                    self.io.sleep(.{ .nanoseconds = frame.nanoseconds - since.nanoseconds }, .awake) catch {};
                    while (try self.loop.tryEvent()) |e| self.handle(e);
                    if (self.view_stale) self.refreshView() catch {};
                }
            }
            self.render();
            last_render = std.Io.Clock.awake.now(self.io);
        }
    }

    fn dbg(self: *App, comptime fmt: []const u8, args: anytype) void {
        const f = self.debug orelse return;
        var b: [512]u8 = undefined;
        const line = std.fmt.bufPrint(&b, fmt ++ "\n", args) catch return;
        f.writeStreamingAll(self.io, line) catch {};
    }

    fn handle(self: *App, ev: Event) void {
        switch (ev) {
            .key_press => |k| self.dbg("key cp={d} text={?s} mods={any} pasting={} prefix={} scroll={} palette={} panel={}", .{ k.codepoint, k.text, k.mods, self.pasting, self.prefix, self.scroll_mode, self.palette != null, self.panel != null }),
            .redraw, .tick, .server_wake => {},
            .mouse => |m| self.dbg("mouse {s} {s} {d},{d} shift={}", .{ @tagName(m.type), @tagName(m.button), m.col, m.row, m.mods.shift }),
            else => self.dbg("event {s}", .{@tagName(ev)}),
        }
        switch (ev) {
            .key_press => |k| self.onKey(k),
            .mouse => |m| self.onMouse(m),
            .winsize => |ws| self.vx.resize(self.gpa, self.tty.writer(), ws) catch {},
            .paste_start => {
                self.pasting = true;
                self.paste_buf.clearRetainingCapacity();
            },
            .paste_end => {
                self.pasting = false;
                // a paste while a prompt or the palette is open is typed there
                if (self.palette) |*p| p.insert(self.paste_buf.items) else self.sendPaste(self.paste_buf.items);
            },
            .paste => |p| self.gpa.free(p),
            .server_wake => self.drainInbox(),
            .notice => |m| {
                self.notice("{s}", .{m});
                self.gpa.free(m);
            },
            .redraw => self.redraw_pending.store(false, .release),
            .tick => self.spin +%= 1,
            else => {},
        }
    }

    /// Whether the server runs on another machine (its paths aren't ours).
    fn remoteServer(self: *App) bool {
        return switch (self.ep) {
            .unix => false,
            .command => true,
        };
    }

    fn sendPaste(self: *App, text: []const u8) void {
        self.sendPasteTo(self.focusedBlock() orelse return, text);
    }

    /// Pastes into a given pane (a click's pane: focus moves there only once the
    /// server's view comes back).
    fn sendPasteTo(self: *App, bid: []const u8, text: []const u8) void {
        const sid = self.sid orelse return;
        self.dbg("paste {d} bytes to {s}", .{ text.len, bid });
        self.retake(bid);
        // Files dropped on the terminal arrive as pasted local paths. On a remote server
        // those paths mean nothing: upload the files instead; the server types their
        // paths over there.
        if (self.remoteServer()) {
            var arena: std.heap.ArenaAllocator = .init(self.gpa);
            defer arena.deinit();
            if (upload.droppedFiles(arena.allocator(), self.io, text)) |files| {
                for (files) |f| self.uploadFile(sid, bid, f);
                return;
            }
        }
        self.send("block.event", "{{\"session_id\":{f},\"block_id\":{f},\"event\":{{\"type\":\"paste\",\"payload\":{{\"text\":{f}}}}}}}", .{ control.q(sid), control.q(bid), control.q(text) });
    }

    /// With the kitty keyboard protocol a shifted letter comes as its base key plus text
    /// ('s' + "S"); rexa's own keys (Ctrl-b S, R in the server page…) want the character.
    fn ownKey(k: vaxis.Key) vaxis.Key {
        var n = k;
        if (k.mods.ctrl or k.mods.alt or k.mods.super) return n;
        if (k.text) |t| if (t.len == 1 and t[0] > 0x20 and t[0] < 0x7f) {
            n.codepoint = t[0];
        };
        return n;
    }

    fn onKey(self: *App, key_in: vaxis.Key) void {
        // `k` for rexa's own handling; the program gets `key_in` unchanged
        const k = ownKey(key_in);
        if (self.pasting) {
            if (k.text) |t| self.paste_buf.appendSlice(self.gpa, t) catch {} else if (k.codepoint == vaxis.Key.enter) self.paste_buf.append(self.gpa, '\n') catch {} else if (k.codepoint < 0x110000 and k.codepoint >= 0x20) {
                var b: [4]u8 = undefined;
                const n = std.unicode.utf8Encode(k.codepoint, &b) catch 0;
                self.paste_buf.appendSlice(self.gpa, b[0..n]) catch {};
            }
            return;
        }
        if (self.prefix) {
            self.prefix = false;
            self.prefixCommand(k);
            return;
        }
        if (k.codepoint == vaxis.Key.f1 and !k.mods.ctrl and !k.mods.alt and !k.mods.shift) return self.toggleHelp();
        // innermost first: the help and menus, a prompt or palette, scroll mode, the panel
        if (self.help != null) {
            if (self.help.?.key(k) == .close) self.help = null;
            return;
        }
        if (self.menu != null) {
            switch (self.menu.?.key(k)) {
                .none => {},
                .close => self.closeMenu(),
                .pick => |a| self.pickMenu(a),
            }
            return;
        }
        if (self.palette != null) return self.paletteKey(k);
        if (self.scroll_mode) return self.scrollKey(k);
        if (self.settings != null) return self.settingsKey(k);
        if (k.matches('b', .{ .ctrl = true })) {
            self.prefix = true;
            return;
        }
        if (k.mods.alt and !k.mods.ctrl and !k.mods.super) {
            if (k.codepoint >= '1' and k.codepoint <= '9') return self.focusWindowIndex(k.codepoint - '1');
            const dir: ?[]const u8 = switch (k.codepoint) {
                vaxis.Key.left => "left",
                vaxis.Key.right => "right",
                vaxis.Key.up => "up",
                vaxis.Key.down => "down",
                else => null,
            };
            if (dir) |d| return self.focusDirection(d);
        }
        if (k.matches('v', .{ .ctrl = true }) and self.tryPasteImage()) return;
        self.sendKey(key_in, null);
    }

    fn sendKey(self: *App, k: vaxis.Key, prefix: ?[]const u8) void {
        const sid = self.sid orelse return;
        const bid = self.focusedBlock() orelse return;
        self.retake(bid);
        // Typing jumps back to the live screen, like any terminal.
        if (self.panes.get(bid)) |p| {
            if (p.scrolledBack()) p.scrollTo(false);
            p.clearSelection();
        }
        var buf: [1024]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        w.print("{{\"session_id\":{f},\"block_id\":{f},\"key\":", .{ control.q(sid), control.q(bid) }) catch return;
        keys.writeKeyJson(&w, k, "press") catch return;
        if (prefix) |p| w.print(",\"prefix\":[{f}]", .{control.q(p)}) catch return;
        w.writeAll("}") catch return;
        self.ctl.send("client.key", w.buffered());
    }

    /// Another client took the size: input takes it back (whoever types decides).
    fn retake(self: *App, bid: []const u8) void {
        if (self.lost.fetchRemove(bid)) |kv| self.gpa.free(kv.key);
    }

    fn prefixCommand(self: *App, k: vaxis.Key) void {
        self.notice_len = 0;
        const sid = self.sid orelse return;
        if (k.matches('b', .{ .ctrl = true })) return self.sendKey(k, null);
        if (k.mods.ctrl or k.mods.alt) return;
        switch (k.codepoint) {
            'd' => self.exitWith("detached", "已断开(程序继续在 Rex 里运行)"),
            'c' => self.send("session.new_window", "{{\"session_id\":{f},\"layout\":{{\"block\":{{\"flavor\":\"com.superlogical.terminal.shell\"}}}}}}", .{control.q(sid)}),
            'n' => self.send("session.focus_next_window", "{{\"session_id\":{f}}}", .{control.q(sid)}),
            'p' => self.send("session.focus_previous_window", "{{\"session_id\":{f}}}", .{control.q(sid)}),
            '1'...'9' => self.focusWindowIndex(k.codepoint - '1'),
            'o' => self.send("session.focus_next_pane", "{{\"session_id\":{f}}}", .{control.q(sid)}),
            '%' => self.split("horizontal"),
            '"' => self.split("vertical"),
            'z' => self.send("session.zoom_block", "{{\"session_id\":{f}}}", .{control.q(sid)}),
            'x' => if (self.focusedBlock()) |b| self.send("block.close", "{{\"session_id\":{f},\"block_id\":{f}}}", .{ control.q(sid), control.q(b) }),
            's' => self.sidebar = !self.sidebarShown(),
            '(' => self.cycleSession(-1),
            ')' => self.cycleSession(1),
            'N' => self.newSession() catch {},
            vaxis.Key.left => self.focusDirection("left"),
            vaxis.Key.right => self.focusDirection("right"),
            vaxis.Key.up => self.focusDirection("up"),
            vaxis.Key.down => self.focusDirection("down"),
            'S' => self.openSettings(null),
            ' ', 'w', ':' => self.openPalette(),
            ',' => self.openRename("tab"),
            '$' => self.openRename("session"),
            '[' => {
                self.scroll_mode = true;
                self.say("scroll: ↑↓ j k · PgUp PgDn · Ctrl-u/d · g G · [ ] prompt · / search · q exit", "回滚:↑↓ j k · PgUp PgDn · Ctrl-u/d · g G · [ ] 提示符 · / 搜索 · q 退出", .{});
            },
            '?', '/' => self.toggleHelp(),
            else => {},
        }
    }

    /// palette commands: English, 中文, the key that does the same, and that key after Ctrl-b
    const commands = [_]struct { []const u8, []const u8, []const u8, u8 }{
        .{ "new tab", "新建 tab", "Ctrl-b c", 'c' },
        .{ "split right", "向右分屏", "Ctrl-b %", '%' },
        .{ "split down", "向下分屏", "Ctrl-b \"", '"' },
        .{ "zoom pane", "放大 pane / 还原", "Ctrl-b z", 'z' },
        .{ "close pane", "关闭 pane", "Ctrl-b x", 'x' },
        .{ "next tab", "下一个 tab", "Ctrl-b n", 'n' },
        .{ "previous tab", "上一个 tab", "Ctrl-b p", 'p' },
        .{ "new session", "新建 session", "Ctrl-b N", 'N' },
        .{ "rename tab…", "重命名 tab…", "Ctrl-b ,", ',' },
        .{ "rename session…", "重命名 session…", "Ctrl-b $", '$' },
        .{ "settings", "设置", "Ctrl-b S", 'S' },
        .{ "toggle sidebar", "显示 / 隐藏侧栏", "Ctrl-b s", 's' },
        .{ "scroll back", "回滚", "Ctrl-b [", '[' },
        .{ "detach", "退出 rexa (detach)", "Ctrl-b d", 'd' },
        .{ "key help", "快捷键手册", "F1", '?' },
    };

    fn openPalette(self: *App) void {
        var p: palette_mod.Palette = .init(self.gpa);
        for (self.model.sessions.items) |s| p.add(.session, s.label, s.id, "");
        for (self.windows(), 0..) |w, i| {
            const wid = control.getStr(w, "window_id") orelse continue;
            var b: [160]u8 = undefined;
            const t = std.fmt.bufPrint(&b, "{d} {s}", .{ i + 1, self.windowTitle(w) }) catch continue;
            p.add(.tab, t, wid, "");
        }
        for (self.machines(self.frame.allocator())) |m| {
            if (m.current) continue;
            p.add(.host, m.name, m.spec, if (m.source == .local) tr("this computer", "这台电脑") else m.spec);
        }
        for (commands) |c| {
            p.add(.command, tr(c[0], c[1]), &.{c[3]}, c[2]);
            if (i18n.isZh()) p.alias(c[0]);
        }
        p.filter();
        self.palette = p;
    }

    fn openRename(self: *App, what: []const u8) void {
        self.openRenameOf(what, null);
    }

    /// Asks for a new name for a tab or session (`target`: its id; null = the current one).
    fn openRenameOf(self: *App, what: []const u8, target: ?[]const u8) void {
        var p: palette_mod.Palette = .init(self.gpa);
        if (std.mem.eql(u8, what, "tab")) {
            var w = self.activeWindow() orelse return;
            if (target) |t| for (self.windows()) |ww| if (control.eqlStr(ww, "window_id", t)) {
                w = ww;
            };
            p.startPrompt(tr("rename tab:", "tab 新名字:"), "tab", self.windowTitle(w));
            p.target = p.arena.allocator().dupe(u8, control.getStr(w, "window_id") orelse "") catch "";
        } else {
            const sid = target orelse self.sid orelse return;
            const cur = if (self.model.findSession(sid)) |i| self.model.sessions.items[i].label else "";
            p.startPrompt(tr("rename session:", "session 新名字:"), "session", cur);
            p.target = p.arena.allocator().dupe(u8, sid) catch "";
        }
        self.palette = p;
    }

    fn closePalette(self: *App) void {
        if (self.palette) |*p| p.deinit();
        self.palette = null;
    }

    fn paletteKey(self: *App, k: vaxis.Key) void {
        const p = &self.palette.?;
        switch (p.key(k)) {
            .none => {},
            .cancel => self.closePalette(),
            .text => |t| {
                if (std.mem.eql(u8, p.prompt_for, "search")) {
                    const needle = self.gpa.dupe(u8, t) catch return self.closePalette();
                    defer self.gpa.free(needle);
                    self.closePalette();
                    if (needle.len == 0) return;
                    const bid = self.focusedBlock() orelse return;
                    const pane = self.panes.get(bid) orelse return;
                    self.showMatch(pane.searchFor(needle));
                    return;
                }
                if (std.mem.startsWith(u8, p.prompt_for, "machine_")) {
                    var a: std.heap.ArenaAllocator = .init(self.gpa);
                    defer a.deinit();
                    const for_what = a.allocator().dupe(u8, p.prompt_for) catch return self.closePalette();
                    const text = a.allocator().dupe(u8, t) catch return self.closePalette();
                    const tgt = a.allocator().dupe(u8, p.target) catch return self.closePalette();
                    self.closePalette();
                    _ = self.machinePrompt(for_what, text, tgt);
                    return;
                }
                const sid = self.sid orelse return self.closePalette();
                const for_tab = std.mem.eql(u8, p.prompt_for, "tab");
                const label = self.gpa.dupe(u8, std.mem.trim(u8, t, " ")) catch return self.closePalette();
                defer self.gpa.free(label);
                const target = self.gpa.dupe(u8, p.target) catch return self.closePalette();
                defer self.gpa.free(target);
                self.closePalette();
                if (target.len == 0) return;
                if (for_tab) {
                    self.send("session.set_window_label", "{{\"session_id\":{f},\"window_id\":{f},\"label\":{f}}}", .{ control.q(sid), control.q(target), control.q(label) });
                } else {
                    self.send("session.set_label", "{{\"session_id\":{f},\"label\":{f}}}", .{ control.q(target), control.q(label) });
                }
            },
            .pick => |it| self.pickPaletteItem(it),
        }
    }

    fn pickPaletteItem(self: *App, it: palette_mod.Item) void {
        // copy what we need: closing frees the palette's strings
        var buf: [512]u8 = undefined;
        const arg = buf[0..@min(it.arg.len, buf.len)];
        @memcpy(arg, it.arg[0..arg.len]);
        const kind = it.kind;
        self.closePalette();
        switch (kind) {
            .session => self.switchSession(arg) catch {},
            .tab => self.focusWindow(arg),
            .host => self.switchMachine(arg),
            .command => if (arg.len > 0) self.prefixCommand(.{ .codepoint = arg[0] }),
        }
    }

    /// Opens Settings, on `page` (null: where it was last time).
    fn openSettings(self: *App, page: ?settings_mod.Page) void {
        if (self.settings == null) self.settings = .{ .page = self.last_settings_page };
        if (page) |pg| self.settings.?.page = pg;
        self.settingsPageShown();
    }

    fn closeSettings(self: *App) void {
        if (self.settings) |st| self.last_settings_page = st.page;
        self.settings = null;
        self.closePanel();
    }

    /// The Rex server page reads the server when it is shown.
    fn settingsPageShown(self: *App) void {
        if (self.settings) |*s| if (s.page == .machines and !s.sel_set) {
            s.sel_set = true;
            for (self.machines(self.frame.allocator()), 0..) |m, i| if (m.current) {
                s.sel = i;
            };
        };
        const st = self.settings orelse return;
        if (st.page == .server and self.panel == null) {
            var p: panel_mod.Panel = .{ .gpa = self.gpa };
            p.refresh(self.ctl, self.io, self.ep);
            self.panel = p;
        }
    }

    fn settingsKey(self: *App, k: vaxis.Key) void {
        const st = &self.settings.?;
        const confirming = st.page == .server and self.panel != null and self.panel.?.confirm != .none;
        if (!confirming and k.codepoint == vaxis.Key.tab and !k.mods.ctrl and !k.mods.alt) {
            st.nextPage(if (k.mods.shift) -1 else 1);
            return self.settingsPageShown();
        }
        if (!confirming and !k.mods.ctrl and !k.mods.alt and k.codepoint >= '1' and k.codepoint <= '3') {
            st.page = @enumFromInt(k.codepoint - '1');
            return self.settingsPageShown();
        }
        switch (st.page) {
            .server => if (self.panel != null) self.panelKey(k),
            .general => switch (k.codepoint) {
                vaxis.Key.escape, 'q' => self.closeSettings(),
                vaxis.Key.left, vaxis.Key.right, vaxis.Key.enter, ' ', 'h', 'l' => self.setLang(if (i18n.lang == .zh) .en else .zh),
                else => if (k.matches('c', .{ .ctrl = true }) or k.matches('g', .{ .ctrl = true })) self.closeSettings(),
            },
            .machines => {
                const ms = self.machines(self.frame.allocator());
                if (st.sel >= ms.len) st.sel = ms.len -| 1;
                const m = ms[st.sel];
                switch (k.codepoint) {
                    vaxis.Key.escape, 'q' => self.closeSettings(),
                    vaxis.Key.up, 'k' => st.sel -|= 1,
                    vaxis.Key.down, 'j' => if (st.sel + 1 < ms.len) {
                        st.sel += 1;
                    },
                    vaxis.Key.enter => self.machineDo(.connect_machine, m.spec, 0, 0),
                    'a' => self.machineDo(.add_machine, "", 0, 0),
                    'r' => if (m.source == .saved) self.machineDo(.rename_machine, m.spec, 0, 0),
                    'e' => if (m.source == .saved) self.machineDo(.edit_machine, m.spec, 0, 0),
                    'd' => if (m.source == .saved) self.machineDo(.confirm_delete_machine, m.spec, self.area_x + settings_mod.nav_width + 4, self.area_y + 6 + @as(u16, @intCast(st.sel))),
                    's' => if (m.source == .rex_host or m.source == .adhoc) self.machineDo(.save_machine, m.spec, 0, 0),
                    else => if (k.matches('c', .{ .ctrl = true }) or k.matches('g', .{ .ctrl = true })) self.closeSettings(),
                }
            },
        }
    }

    fn setLang(self: *App, l: i18n.Lang) void {
        self.config.setLang(l);
        if (self.config.save_error) |e| self.say("couldn't save the settings: {s}", "设置保存失败:{s}", .{e});
    }

    /// This computer, the saved machines, the connected server's `rex hosts`, and the
    /// current connection if it is none of those.
    fn machines(self: *App, arena: std.mem.Allocator) []Machine {
        var list: std.ArrayList(Machine) = .empty;
        list.append(arena, .{ .name = tr("This computer", "本机"), .spec = "", .source = .local, .current = self.spec.len == 0 }) catch {};
        for (self.config.servers.items, 0..) |sv, i| {
            list.append(arena, .{ .name = sv.name, .spec = sv.endpoint, .source = .saved, .index = i, .current = std.mem.eql(u8, sv.endpoint, self.spec) }) catch {};
        }
        for (self.hosts.items) |h| {
            if (self.config.find(h.endpoint) != null or self.config.find(h.label) != null) continue;
            const cur = std.mem.eql(u8, h.endpoint, self.spec) or std.mem.eql(u8, h.label, self.spec);
            list.append(arena, .{ .name = h.label, .spec = h.endpoint, .source = .rex_host, .current = cur }) catch {};
        }
        for (list.items) |m| if (m.current) return list.items;
        list.append(arena, .{ .name = config_mod.defaultName(self.spec), .spec = self.spec, .source = .adhoc, .current = true }) catch {};
        return list.items;
    }

    fn currentMachineName(self: *App, arena: std.mem.Allocator) []const u8 {
        for (self.machines(arena)) |m| if (m.current) return m.name;
        return "";
    }

    fn findMachine(self: *App, arena: std.mem.Allocator, spec: []const u8) ?Machine {
        for (self.machines(arena)) |m| if (std.mem.eql(u8, m.spec, spec)) return m;
        return null;
    }

    /// Reconnects to another machine (main.zig does it once this run ends).
    fn switchMachine(self: *App, spec: []const u8) void {
        if (std.mem.eql(u8, spec, self.spec)) return;
        self.setExit(switch_prefix ++ "{s}", .{spec});
    }

    fn machineMenu(self: *App, x: u16, y: u16, spec: []const u8) void {
        const arena = self.frame.allocator();
        const m = self.findMachine(arena, spec) orelse return;
        const target = std.fmt.allocPrint(arena, "machine:{s}", .{spec}) catch return;
        var mn: menu_mod.Menu = .init(self.gpa, x, y, m.name, target);
        if (!m.current) mn.add(tr("Connect", "连接"), "", .connect_machine);
        switch (m.source) {
            .saved => {
                mn.add(tr("Rename…", "改名字…"), "", .rename_machine);
                mn.add(tr("Edit address…", "改地址…"), "", .edit_machine);
            },
            .rex_host, .adhoc => mn.add(tr("Save with a name…", "保存并命名…"), "", .save_machine),
            .local => {},
        }
        mn.separator();
        mn.add(tr("Add a machine…", "添加机器…"), "", .add_machine);
        mn.add(tr("Manage machines…", "管理机器…"), "Ctrl-b S", .settings);
        if (m.source == .saved) {
            mn.separator();
            mn.addDanger(tr("Delete…", "删除…"), "", .confirm_delete_machine);
        }
        self.openMenu(mn);
    }

    /// Machine actions from the sidebar menu and the Settings page.
    fn machineDo(self: *App, action: menu_mod.Action, spec: []const u8, mx: u16, my: u16) void {
        const arena = self.frame.allocator();
        switch (action) {
            .connect_machine => self.switchMachine(spec),
            .add_machine => {
                var p: palette_mod.Palette = .init(self.gpa);
                p.startPrompt(tr("address (e.g. ssh://me@mac.local):", "地址(例如 ssh://me@mac.local):"), "machine_new", "ssh://");
                self.openPrompt(p);
            },
            .rename_machine, .save_machine => {
                const m = self.findMachine(arena, spec) orelse return;
                var p: palette_mod.Palette = .init(self.gpa);
                p.startPrompt(tr("name:", "名字:"), "machine_name", if (m.source == .adhoc) config_mod.defaultName(spec) else m.name);
                p.target = p.arena.allocator().dupe(u8, spec) catch "";
                self.openPrompt(p);
            },
            .edit_machine => {
                var p: palette_mod.Palette = .init(self.gpa);
                p.startPrompt(tr("address:", "地址:"), "machine_addr", spec);
                p.target = p.arena.allocator().dupe(u8, spec) catch "";
                self.openPrompt(p);
            },
            .confirm_delete_machine => {
                const m = self.findMachine(arena, spec) orelse return;
                const title = i18n.fmt(arena, "Delete \"{s}\" from the list? (nothing on it is touched)", "从列表里删除「{s}」?(那台机器上的东西不受影响)", .{m.name});
                const target = std.fmt.allocPrint(arena, "machine:{s}", .{spec}) catch return;
                var mn: menu_mod.Menu = .init(self.gpa, mx, my, title, target);
                mn.addDanger(tr("Delete", "删除"), "", .delete_machine);
                mn.add(tr("Cancel", "取消"), "Esc", .none);
                mn.sel = 0;
                self.openMenu(mn);
            },
            .delete_machine => if (self.config.find(spec)) |i| {
                self.config.remove(i);
                if (self.config.save_error) |e| self.say("couldn't save the settings: {s}", "设置保存失败:{s}", .{e});
            },
            else => {},
        }
    }

    /// A text prompt over everything else (it may be opened from Settings).
    fn openPrompt(self: *App, p: palette_mod.Palette) void {
        self.closePalette();
        self.palette = p;
    }

    /// Answers to the machine prompts. Returns false if `for_what` isn't one.
    fn machinePrompt(self: *App, for_what: []const u8, text: []const u8, target: []const u8) bool {
        const v = std.mem.trim(u8, text, " \t");
        if (std.mem.eql(u8, for_what, "machine_new")) {
            if (v.len == 0 or std.mem.eql(u8, v, "ssh://")) return true;
            var a: std.heap.ArenaAllocator = .init(self.gpa);
            defer a.deinit();
            _ = transport.Endpoint.parse(a.allocator(), v) catch {
                self.say("not an address: {s}", "地址写法不对:{s}", .{v});
                return true;
            };
            var p: palette_mod.Palette = .init(self.gpa);
            p.startPrompt(tr("name:", "名字:"), "machine_name", config_mod.defaultName(v));
            p.target = p.arena.allocator().dupe(u8, v) catch "";
            self.openPrompt(p);
            return true;
        }
        if (std.mem.eql(u8, for_what, "machine_name")) {
            if (target.len == 0) return true;
            const name = if (v.len > 0) v else config_mod.defaultName(target);
            if (self.config.find(target)) |i| self.config.rename(i, name) else self.config.add(name, target);
            if (self.config.save_error) |e| self.say("couldn't save the settings: {s}", "设置保存失败:{s}", .{e}) else self.say("saved \"{s}\"", "已保存「{s}」", .{name});
            return true;
        }
        if (std.mem.eql(u8, for_what, "machine_addr")) {
            const i = self.config.find(target) orelse return true;
            if (v.len == 0) return true;
            var a: std.heap.ArenaAllocator = .init(self.gpa);
            defer a.deinit();
            _ = transport.Endpoint.parse(a.allocator(), v) catch {
                self.say("not an address: {s}", "地址写法不对:{s}", .{v});
                return true;
            };
            const was_current = std.mem.eql(u8, target, self.spec);
            self.config.setEndpoint(i, v);
            // still the same connection: the list should keep showing it as current
            if (was_current) self.spec = self.config.servers.items[i].endpoint;
            return true;
        }
        return false;
    }

    fn closePanel(self: *App) void {
        if (self.panel) |*p| p.deinit();
        self.panel = null;
    }

    fn panelKey(self: *App, k: vaxis.Key) void {
        const p = &self.panel.?;
        if (p.confirm != .none) {
            const what = p.confirm;
            p.confirm = .none;
            if (k.codepoint != 'y' or k.mods.ctrl or k.mods.alt) {
                p.sayT("cancelled", "已取消", .{});
                return;
            }
            self.stopServer(what == .restart);
            return;
        }
        if (k.mods.ctrl or k.mods.alt) return;
        switch (k.codepoint) {
            vaxis.Key.escape, 'q' => self.closeSettings(),
            'l' => {
                p.refresh(self.ctl, self.io, self.ep);
                p.sayT("refreshed", "已刷新", .{});
            },
            'c' => if (self.call("server.config_reload", "{{}}", .{})) |r| {
                r.deinit();
                p.refresh(self.ctl, self.io, self.ep);
                p.sayT("configuration reloaded", "配置已重新加载", .{});
            } else p.sayT("reload failed: {s}", "重新加载失败:{s}", .{self.ctl.lastError()}),
            'X' => p.confirm = .stop,
            'R' => p.confirm = .restart,
            'U' => {
                const sid = self.sid orelse return;
                self.send("session.new_window", "{{\"session_id\":{f},\"window_label\":\"update rex\",\"layout\":{{\"block\":{{\"flavor\":\"com.superlogical.terminal.shell\",\"options\":{{\"command\":[\"sh\",\"-c\",{f}]}}}}}}}}", .{ control.q(sid), control.q(panel_mod.update_script) });
                self.closeSettings();
                self.say("update runs in the new tab \"update rex\" (it may ask for your password)", "更新在新 tab「update rex」里运行(可能要输入密码)", .{});
            },
            else => {},
        }
    }

    /// Stops the server; with `restart`, starts it again the way we know for this
    /// endpoint. The control connection reconnects on its own once it is back.
    fn stopServer(self: *App, restart: bool) void {
        const p = &self.panel.?;
        var arena_state: std.heap.ArenaAllocator = .init(self.gpa);
        defer arena_state.deinit();
        const start_argv = if (restart) panel_mod.startCommand(arena_state.allocator(), self.ep, self.env) else null;
        if (restart and start_argv == null) {
            p.sayT("don't know how to start this server again; stop it and start it yourself", "不知道怎么重新启动这个服务端;请自己停止再启动", .{});
            return;
        }
        if (self.call("server.stop", "{{}}", .{})) |r| r.deinit() else {
            p.sayT("stop failed: {s}", "停止失败:{s}", .{self.ctl.lastError()});
            return;
        }
        if (start_argv) |argv| {
            // give it a moment to release the socket, then start it detached
            self.io.sleep(.fromMilliseconds(800), .awake) catch {};
            var child = std.process.spawn(self.io, .{ .argv = argv, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore }) catch {
                p.sayT("server stopped, but starting it failed", "服务端已停止,但重新启动失败", .{});
                return;
            };
            _ = child.wait(self.io) catch {};
            p.sayT("server restarting… rexa reconnects by itself", "服务端正在重启… rexa 会自动重连", .{});
        } else p.sayT("server stopped. rexa keeps trying to reconnect; Ctrl-b d to leave", "服务端已停止。rexa 会一直尝试重连;Ctrl-b d 退出", .{});
    }

    fn scrollKey(self: *App, k: vaxis.Key) void {
        const bid = self.focusedBlock() orelse return;
        const p = self.panes.get(bid) orelse return;
        const page: isize = if (self.last_layout) |l| if (l.find(bid)) |pl| @intCast(@max(1, pl.content.h -| 1)) else 20 else 20;
        if (k.matches('u', .{ .ctrl = true })) return p.scrollBy(-@divTrunc(page, 2));
        if (k.matches('d', .{ .ctrl = true })) return p.scrollBy(@divTrunc(page, 2));
        switch (k.codepoint) {
            'k', vaxis.Key.up => p.scrollBy(-1),
            'j', vaxis.Key.down => p.scrollBy(1),
            vaxis.Key.page_up, 'b' => p.scrollBy(-page),
            vaxis.Key.page_down, ' ', 'f' => p.scrollBy(page),
            'g', vaxis.Key.home => p.scrollTo(true),
            'G', vaxis.Key.end => p.scrollTo(false),
            '[' => p.scrollPrompt(-1),
            ']' => p.scrollPrompt(1),
            '/', '?' => {
                var pal: palette_mod.Palette = .init(self.gpa);
                pal.startPrompt(tr("search:", "搜索:"), "search", "");
                self.palette = pal;
            },
            'n' => self.showMatch(p.searchStep(true)),
            'N' => self.showMatch(p.searchStep(false)),
            'q', vaxis.Key.escape, vaxis.Key.enter => {
                p.searchClear();
                p.scrollTo(false);
                self.scroll_mode = false;
                self.notice_len = 0;
            },
            else => {},
        }
    }

    fn showMatch(self: *App, m: ?Pane.Match) void {
        const mm = m orelse return;
        if (mm.total == 0) self.say("no matches", "没有匹配", .{}) else self.say("match {d}/{d} · n older · N newer · q exit", "第 {d}/{d} 个匹配 · n 更早 · N 更新 · q 退出", .{ mm.index + 1, mm.total });
    }

    fn split(self: *App, direction: []const u8) void {
        const sid = self.sid orelse return;
        self.send("session.new_split", "{{\"session_id\":{f},\"direction\":\"{s}\",\"side\":\"after\",\"focus\":true,\"layout\":{{\"block\":{{\"flavor\":\"com.superlogical.terminal.shell\"}}}}}}", .{ control.q(sid), direction });
    }

    fn focusDirection(self: *App, dir: []const u8) void {
        const sid = self.sid orelse return;
        self.send("session.focus_direction", "{{\"session_id\":{f},\"direction\":\"{s}\",\"wrap\":true}}", .{ control.q(sid), dir });
    }

    fn focusWindowIndex(self: *App, i: usize) void {
        const ws = self.windows();
        if (i >= ws.len) return;
        self.focusWindow(control.getStr(ws[i], "window_id") orelse return);
    }

    fn focusWindow(self: *App, wid: []const u8) void {
        const sid = self.sid orelse return;
        self.send("session.focus_window", "{{\"session_id\":{f},\"window_id\":{f}}}", .{ control.q(sid), control.q(wid) });
    }

    fn focusBlock(self: *App, bid: []const u8) void {
        const sid = self.sid orelse return;
        if (self.focusedBlock()) |f| if (std.mem.eql(u8, f, bid)) return;
        self.send("session.focus_block", "{{\"session_id\":{f},\"block_id\":{f}}}", .{ control.q(sid), control.q(bid) });
    }

    // ---- mouse ----------------------------------------------------------------------

    fn onMouse(self: *App, m: vaxis.Mouse) void {
        if (m.col < 0 or m.row < 0) return;
        const x: u16 = @intCast(m.col);
        const y: u16 = @intCast(m.row);
        // overlays take the mouse first
        if (self.help) |*h| {
            if (h.mouse(m, x, y) == .close) self.help = null;
            return;
        }
        if (self.menu) |*mn| {
            switch (mn.mouse(m, x, y)) {
                .none => {},
                .close => self.closeMenu(),
                .pick => |a| self.pickMenu(a),
            }
            return;
        }
        if (self.drag != null) return self.dragSeparator(m, x, y);
        if (m.type == .motion) self.hover(x, y);
        const click = m.type == .press and m.button == .left;
        const clicks = if (click) self.countClick(x, y) else 0;
        // status line buttons
        if (y == self.status_y and x >= self.side_w) {
            if (click) for (self.status_buttons) |b| if (x >= b.x0 and x < b.x1) return self.pickMenu(b.action);
            return;
        }
        if (self.palette) |*pal| {
            switch (pal.mouse(m, x, y)) {
                .none, .text => {},
                .cancel => self.closePalette(),
                .pick => |it| self.pickPaletteItem(it),
            }
            return;
        }
        // sidebar
        if (self.side_w > 0 and x < self.side_w) return self.sidebarMouse(m, x, y);
        // tab bar
        if (y == self.tab_y) return self.tabMouse(m, x, clicks);
        if (self.settings) |*st| {
            if (!click) return;
            if (st.page == .server) if (self.panel) |*pn| if (pn.buttonAt(x, y)) |key| return self.panelKey(.{ .codepoint = key });
            if (st.hitAt(x, y)) |h| switch (h) {
                .page => |pg| {
                    st.page = pg;
                    self.settingsPageShown();
                },
                .lang => |l| self.setLang(l),
                .machine => |i| {
                    st.sel = i;
                    if (clicks >= 2) self.settingsKey(.{ .codepoint = vaxis.Key.enter });
                },
                .key => |key| self.settingsKey(.{ .codepoint = key }),
            };
            return;
        }
        const l = self.last_layout orelse return;
        if (x < self.area_x or y < self.area_y) return;
        const lx = x - self.area_x;
        const ly = y - self.area_y;
        // a separator: drag it to resize the split
        if (click) if (l.splitAt(lx, ly)) |sp| {
            if (l.hit(lx, ly) == null or !l.hit(lx, ly).?.content.contains(lx, ly)) {
                var d: @typeInfo(@TypeOf(self.drag)).optional.child = .{ .horizontal = sp.horizontal, .before = undefined, .before_len = 0, .after = undefined, .after_len = 0, .rect = sp.rect, .last = sp.line };
                if (sp.before_block.len > d.before.len or sp.after_block.len > d.after.len) return;
                @memcpy(d.before[0..sp.before_block.len], sp.before_block);
                d.before_len = sp.before_block.len;
                @memcpy(d.after[0..sp.after_block.len], sp.after_block);
                d.after_len = sp.after_block.len;
                self.drag = d;
                return;
            }
        };
        const p = l.hit(lx, ly) orelse return;
        if (!p.content.contains(lx, ly)) return;
        const cx = lx - p.content.x;
        const cy = ly - p.content.y;
        const pane = self.panes.get(p.block_id);
        // The program gets the mouse when it asks for it; Shift keeps it for rexa.
        const local_mouse = pane != null and (m.mods.shift or !pane.?.programWantsMouse());
        switch (m.button) {
            .left => switch (m.type) {
                .press => {
                    self.focusBlock(p.block_id);
                    self.endSelection(false);
                    if (local_mouse) {
                        if (clicks >= 2) return self.selectWord(pane.?, p.content, cx, cy, clicks >= 3);
                        self.sel_block = self.gpa.dupe(u8, p.block_id) catch null;
                        self.sel_anchor = .{ cx, cy };
                        self.sel_moved = false;
                    } else self.forwardButton(p.block_id, "press", "left", cx, cy, m.mods);
                },
                .drag => if (self.sel_block) |sb| if (std.mem.eql(u8, sb, p.block_id)) {
                    pane.?.select(self.sel_anchor[0], self.sel_anchor[1], cx, cy);
                    self.sel_moved = true;
                },
                .release => if (self.sel_block != null) self.endSelection(true) else if (!local_mouse) self.forwardButton(p.block_id, "release", "left", cx, cy, m.mods),
                else => {},
            },
            .wheel_up, .wheel_down, .wheel_left, .wheel_right => if (m.type == .press) {
                const dy: i32 = switch (m.button) {
                    .wheel_up => -1,
                    .wheel_down => 1,
                    else => 0,
                };
                const dx: i32 = switch (m.button) {
                    .wheel_left => -1,
                    .wheel_right => 1,
                    else => 0,
                };
                if (pane != null and dx == 0 and !pane.?.programWantsMouse()) {
                    pane.?.scrollBy(dy * 3);
                } else self.forwardWheel(p.block_id, cx, cy, dx, dy, m.mods);
            },
            .right => if (local_mouse) {
                if (m.type == .press) {
                    self.focusBlock(p.block_id);
                    self.paneMenu(x, y, p.block_id, pane.?);
                }
            } else if (m.type == .press or m.type == .release) {
                self.forwardButton(p.block_id, if (m.type == .press) "press" else "release", "right", cx, cy, m.mods);
            },
            .middle => if (local_mouse) {
                if (m.type == .press) {
                    self.focusBlock(p.block_id);
                    self.pasteSelection(p.block_id);
                }
            } else if (m.type == .press or m.type == .release) {
                self.forwardButton(p.block_id, if (m.type == .press) "press" else "release", "middle", cx, cy, m.mods);
            },
            else => {},
        }
    }

    /// Counts quick clicks on the same cell: 1, 2 (double), 3 (triple).
    fn countClick(self: *App, x: u16, y: u16) u8 {
        const now = std.Io.Clock.awake.now(self.io).nanoseconds;
        const lc = &self.last_click;
        const quick = now - lc.at < 400 * std.time.ns_per_ms;
        const near = lc.y == y and (if (x > lc.x) x - lc.x else lc.x - x) <= 1;
        lc.count = if (quick and near) @min(lc.count + 1, 3) else 1;
        lc.at = now;
        lc.x = x;
        lc.y = y;
        return lc.count;
    }

    /// The mouse pointer's shape: a resize arrow over separators, a hand over buttons.
    fn hover(self: *App, x: u16, y: u16) void {
        var shape: vaxis.Mouse.Shape = .default;
        if (self.settings == null and self.palette == null and y != self.tab_y and y != self.status_y and x >= self.side_w and x >= self.area_x and y >= self.area_y) {
            if (self.last_layout) |l| {
                const lx = x - self.area_x;
                const ly = y - self.area_y;
                const in_content = if (l.hit(lx, ly)) |p| p.content.contains(lx, ly) else false;
                if (!in_content) if (l.splitAt(lx, ly)) |sp| {
                    shape = if (sp.horizontal) .@"ew-resize" else .@"ns-resize";
                };
            }
        } else if (y == self.tab_y or y == self.status_y or x < self.side_w) shape = .pointer;
        if (self.settings) |*st| {
            if (st.hitAt(x, y) != null) shape = .pointer;
            if (st.page == .server) if (self.panel) |*pn| if (pn.buttonAt(x, y) != null) {
                shape = .pointer;
            };
        }
        if (shape != self.mouse_shape) {
            self.mouse_shape = shape;
            self.vx.setMouseShape(shape);
        }
    }

    fn dragSeparator(self: *App, m: vaxis.Mouse, x: u16, y: u16) void {
        const d = &self.drag.?;
        if (m.type == .release) {
            self.drag = null;
            return;
        }
        if (m.type != .drag) return;
        const pos = if (d.horizontal) x -| self.area_x else y -| self.area_y;
        if (pos == d.last) return;
        d.last = pos;
        const sid = self.sid orelse return;
        const sp: layout.Split = .{ .horizontal = d.horizontal, .before_block = d.before[0..d.before_len], .after_block = d.after[0..d.after_len], .rect = d.rect, .line = pos };
        const ratio = sp.ratioAt(pos);
        self.send("session.resize_pane", "{{\"session_id\":{f},\"direction\":\"{s}\",\"before_block_id\":{f},\"after_block_id\":{f},\"ratio\":{d:.4}}}", .{
            control.q(sid), if (d.horizontal) "horizontal" else "vertical", control.q(sp.before_block), control.q(sp.after_block), ratio,
        });
    }

    /// Double click: the word under the mouse; triple: the whole line. Copies it.
    fn selectWord(self: *App, pane: *Pane, content: layout.Rect, cx: u16, cy: u16, line: bool) void {
        var x0: u16 = 0;
        var x1: u16 = content.w -| 1;
        if (!line) {
            const sx = self.area_x + content.x;
            const sy = self.area_y + content.y + cy;
            const scr = self.vx.window();
            const cls = struct {
                fn word(w: vaxis.Window, xx: u16, yy: u16) bool {
                    const c = w.readCell(xx, yy) orelse return false;
                    const g = c.char.grapheme;
                    // the right half of a wide character counts as that character
                    if ((g.len == 0 or std.mem.eql(u8, g, " ")) and xx > 0) {
                        if (w.readCell(xx - 1, yy)) |prev| if (prev.char.width == 2) return word(w, xx - 1, yy);
                    }
                    if (g.len == 0) return false;
                    if (g.len == 1) return std.mem.indexOfScalar(u8, " \t'\"`()[]{}<>|,;:", g[0]) == null;
                    return true;
                }
            };
            if (!cls.word(scr, sx + cx, sy)) return;
            x0 = cx;
            while (x0 > 0 and cls.word(scr, sx + x0 - 1, sy)) x0 -= 1;
            x1 = cx;
            while (x1 + 1 < content.w and cls.word(scr, sx + x1 + 1, sy)) x1 += 1;
        }
        pane.select(x0, cy, x1, cy);
        const text = pane.selectionText(self.gpa) orelse return;
        defer self.gpa.free(text);
        self.copyText(text);
    }

    fn copyText(self: *App, text: []const u8) void {
        self.vx.copyToSystemClipboard(self.tty.writer(), text, self.gpa) catch {};
        self.last_copy.clearRetainingCapacity();
        self.last_copy.appendSlice(self.gpa, text) catch {};
        self.say("copied {d} characters", "已复制 {d} 个字符", .{std.unicode.utf8CountCodepoints(text) catch text.len});
    }

    /// Middle click: what rexa copied last, else the system selection.
    fn pasteSelection(self: *App, bid: []const u8) void {
        self.dbg("middle paste: last_copy {d} bytes", .{self.last_copy.items.len});
        if (self.last_copy.items.len > 0) {
            const t = self.gpa.dupe(u8, self.last_copy.items) catch return;
            defer self.gpa.free(t);
            return self.sendPasteTo(bid, t);
        }
        const t = upload.clipboardText(self.gpa, self.io, self.env, true) orelse return;
        defer self.gpa.free(t);
        self.sendPasteTo(bid, t);
    }

    /// Paste from the clipboard: an image is uploaded, text is pasted.
    fn pasteClipboard(self: *App, bid: []const u8) void {
        if (self.tryPasteImage()) return;
        const t = upload.clipboardText(self.gpa, self.io, self.env, false) orelse {
            self.say("the clipboard is empty (or wl-paste / xclip is missing)", "剪贴板是空的(或者没装 wl-paste / xclip)", .{});
            return;
        };
        defer self.gpa.free(t);
        if (bid.len > 0) self.sendPasteTo(bid, t) else self.sendPaste(t);
    }

    fn sidebarMouse(self: *App, m: vaxis.Mouse, x: u16, y: u16) void {
        if (m.type != .press) return;
        if (m.button == .wheel_up) return self.cycleSession(-1);
        if (m.button == .wheel_down) return self.cycleSession(1);
        for (self.side_hits) |h| if (h.y == y) switch (h.what) {
            .session => |sid| {
                if (m.button == .left) return self.switchSession(sid) catch {};
                if (m.button == .right) {
                    var mn: menu_mod.Menu = .init(self.gpa, x, y, "session", sid);
                    mn.add(tr("Switch to it", "切换到这个 session"), "", .switch_session);
                    mn.add(tr("Rename…", "重命名…"), "Ctrl-b $", .rename_session);
                    mn.add(tr("New session", "新建 session"), "Ctrl-b N", .new_session);
                    mn.separator();
                    mn.addDanger(tr("End this session…", "结束这个 session…"), "", .confirm_destroy_session);
                    return self.openMenu(mn);
                }
                return;
            },
            .new_session => if (m.button == .left) return self.newSession() catch {},
            .machine => |spec| {
                if (m.button == .left) return self.switchMachine(spec);
                if (m.button == .right) return self.machineMenu(x, y, spec);
                return;
            },
            .add_machine => if (m.button == .left) return self.machineDo(.add_machine, "", 0, 0),
        };
    }

    fn tabMouse(self: *App, m: vaxis.Mouse, x: u16, clicks: u8) void {
        if (m.type != .press) return;
        const sid = self.sid orelse return;
        switch (m.button) {
            .wheel_up => return self.send("session.focus_previous_window", "{{\"session_id\":{f}}}", .{control.q(sid)}),
            .wheel_down => return self.send("session.focus_next_window", "{{\"session_id\":{f}}}", .{control.q(sid)}),
            else => {},
        }
        for (self.tab_hits) |t| if (x >= t.x0 and x < t.x1) {
            switch (m.button) {
                .left => {
                    if (x == t.close_x or x == t.close_x + 1) return self.confirmCloseTab(x, t);
                    self.focusWindow(t.window_id);
                    if (clicks >= 2) self.openRenameOf("tab", t.window_id);
                },
                .middle => self.confirmCloseTab(x, t),
                .right => {
                    var mn: menu_mod.Menu = .init(self.gpa, x, self.tab_y + 1, "tab", t.window_id);
                    mn.add(tr("Rename…", "重命名…"), "Ctrl-b ,", .rename_tab);
                    mn.add(tr("New tab", "新建 tab"), "Ctrl-b c", .new_tab);
                    mn.separator();
                    mn.addDanger(tr("Close tab…", "关闭 tab…"), "", .confirm_close_tab);
                    self.openMenu(mn);
                },
                else => {},
            }
            return;
        };
        if (m.button == .left) if (self.tab_plus) |p| if (x >= p[0] and x < p[1]) self.pickMenu(.new_tab);
    }

    fn confirmCloseTab(self: *App, x: u16, t: TabHit) void {
        var b: [160]u8 = undefined;
        const title = i18n.bufFmt(&b, "Close tab \"{s}\"? Its programs will end", "关闭 tab「{s}」?里面的程序会结束", .{t.title});
        var mn: menu_mod.Menu = .init(self.gpa, x, self.tab_y + 1, title, t.window_id);
        mn.addDanger(tr("Close", "关闭"), "", .close_tab);
        mn.add(tr("Cancel", "取消"), "Esc", .none);
        mn.sel = 0;
        self.openMenu(mn);
    }

    fn paneMenu(self: *App, x: u16, y: u16, bid: []const u8, pane: *Pane) void {
        var mn: menu_mod.Menu = .init(self.gpa, x, y, "", bid);
        const has_sel = if (pane.selectionText(self.gpa)) |t| blk: {
            self.gpa.free(t);
            break :blk true;
        } else false;
        if (has_sel) mn.add(tr("Copy", "复制"), "", .copy);
        mn.add(tr("Paste", "粘贴"), "Ctrl-V", .paste);
        mn.separator();
        mn.add(tr("Split right", "向右分屏"), "Ctrl-b %", .split_right);
        mn.add(tr("Split down", "向下分屏"), "Ctrl-b \"", .split_down);
        const zoomed = if (self.activeWindow()) |w| control.getStr(w, "zoomed_block_id") != null else false;
        mn.add(if (zoomed) tr("Restore size", "还原大小") else tr("Zoom this pane", "放大这个 pane"), "Ctrl-b z", .zoom);
        mn.add(tr("New tab", "新建 tab"), "Ctrl-b c", .new_tab);
        mn.separator();
        mn.add(tr("Scroll back / search", "回滚 / 搜索"), "Ctrl-b [", .scroll_mode);
        mn.add(tr("Command palette", "命令面板"), "Ctrl-b Space", .palette);
        mn.add(tr("Settings", "设置"), "Ctrl-b S", .settings);
        mn.add(tr("Key manual", "快捷键手册"), "F1", .help);
        mn.separator();
        mn.addDanger(tr("Close this pane…", "关闭这个 pane…"), "Ctrl-b x", .confirm_close_pane);
        mn.add(tr("Leave rexa (detach)", "退出 rexa (detach)"), "Ctrl-b d", .detach);
        self.openMenu(mn);
    }

    fn openMenu(self: *App, mn: menu_mod.Menu) void {
        self.closeMenu();
        self.menu = mn;
    }

    fn closeMenu(self: *App) void {
        if (self.menu) |*mn| mn.deinit();
        self.menu = null;
    }

    fn toggleHelp(self: *App) void {
        self.closeMenu();
        self.help = if (self.help == null) .{} else null;
    }

    /// Does what a menu item, status button or click asks. The menu's target (a block,
    /// window or session id) is copied first: closing the menu frees it.
    fn pickMenu(self: *App, action: menu_mod.Action) void {
        var tb: [200]u8 = undefined;
        var target: []const u8 = "";
        var mx: u16 = 0;
        var my: u16 = 0;
        if (self.menu) |mn| {
            const n = @min(mn.target.len, tb.len);
            @memcpy(tb[0..n], mn.target[0..n]);
            target = tb[0..n];
            mx = mn.x;
            my = mn.y;
        }
        self.closeMenu();
        const sid = self.sid orelse return;
        const bid_or_focused = if (target.len > 0 and std.mem.startsWith(u8, target, "block:")) target else self.focusedBlock() orelse "";
        switch (action) {
            .none => {},
            .copy => if (self.panes.get(bid_or_focused)) |p| if (p.selectionText(self.gpa)) |t| {
                defer self.gpa.free(t);
                self.copyText(t);
            },
            .paste => self.pasteClipboard(bid_or_focused),
            .split_right => self.split("horizontal"),
            .split_down => self.split("vertical"),
            .zoom => self.send("session.zoom_block", "{{\"session_id\":{f}}}", .{control.q(sid)}),
            .confirm_close_pane => {
                var mn: menu_mod.Menu = .init(self.gpa, mx, my, tr("Close this pane? Its program will end", "关闭这个 pane?里面的程序会结束"), bid_or_focused);
                mn.addDanger(tr("Close", "关闭"), "", .close_pane);
                mn.add(tr("Cancel", "取消"), "Esc", .none);
                mn.sel = 0;
                self.openMenu(mn);
            },
            .close_pane => if (bid_or_focused.len > 0) self.send("block.close", "{{\"session_id\":{f},\"block_id\":{f}}}", .{ control.q(sid), control.q(bid_or_focused) }),
            .new_tab => self.send("session.new_window", "{{\"session_id\":{f},\"layout\":{{\"block\":{{\"flavor\":\"com.superlogical.terminal.shell\"}}}}}}", .{control.q(sid)}),
            .rename_tab => self.openRenameOf("tab", if (target.len > 0) target else null),
            .confirm_close_tab => {
                for (self.tab_hits) |t| if (std.mem.eql(u8, t.window_id, target)) return self.confirmCloseTab(mx, t);
            },
            .close_tab => if (target.len > 0) self.send("session.close_window", "{{\"session_id\":{f},\"window_id\":{f}}}", .{ control.q(sid), control.q(target) }),
            .new_session => self.newSession() catch {},
            .rename_session => self.openRenameOf("session", if (target.len > 0) target else null),
            .switch_session => if (target.len > 0) self.switchSession(target) catch {},
            .confirm_destroy_session => {
                const label = if (self.model.findSession(target)) |i| self.model.sessions.items[i].label else "";
                var b: [200]u8 = undefined;
                const t = i18n.bufFmt(&b, "End session \"{s}\"? Every program in it will end", "结束 session「{s}」?里面所有程序都会结束", .{label});
                var mn: menu_mod.Menu = .init(self.gpa, mx, my, t, target);
                mn.addDanger(tr("End session", "结束 session"), "", .destroy_session);
                mn.add(tr("Cancel", "取消"), "Esc", .none);
                mn.sel = 0;
                self.openMenu(mn);
            },
            .destroy_session => if (target.len > 0) {
                if (self.model.sessions.items.len <= 1) {
                    self.say("this is the last session; rexa would have nothing to show — end it with `rex` instead", "这是最后一个 session,结束后 rexa 就没东西可显示了;请用 `rex` 命令结束它", .{});
                    return;
                }
                self.send("session.destroy", "{{\"session_id\":{f}}}", .{control.q(target)});
            },
            .scroll_mode => self.prefixCommand(.{ .codepoint = '[' }),
            .search => {
                self.prefixCommand(.{ .codepoint = '[' });
                self.scrollKey(.{ .codepoint = '/' });
            },
            .palette => self.openPalette(),
            .help => self.toggleHelp(),
            .server_panel => self.openSettings(.server),
            .settings => if (std.mem.startsWith(u8, target, "machine:")) self.openSettings(.machines) else if (self.settings != null) self.closeSettings() else self.openSettings(null),
            .toggle_sidebar => self.sidebar = !self.sidebarShown(),
            .detach => self.exitWith("detached", "已断开(程序继续在 Rex 里运行)"),
            .exit_scroll => self.scrollKey(.{ .codepoint = 'q' }),
            .connect_machine, .rename_machine, .edit_machine, .save_machine, .confirm_delete_machine, .delete_machine, .add_machine => {
                const spec = if (std.mem.startsWith(u8, target, "machine:")) target["machine:".len..] else "";
                self.machineDo(action, spec, mx, my);
            },
        }
    }

    /// Finishes a mouse selection; with `copy`, puts the text on the local clipboard.
    fn endSelection(self: *App, copy: bool) void {
        const sb = self.sel_block orelse {
            return;
        };
        defer {
            self.gpa.free(sb);
            self.sel_block = null;
        }
        const pane = self.panes.get(sb) orelse return;
        if (!copy or !self.sel_moved) {
            pane.clearSelection();
            return;
        }
        const text = pane.selectionText(self.gpa) orelse return;
        defer self.gpa.free(text);
        self.copyText(text);
    }

    fn mouseMods(buf: []u8, m: vaxis.Mouse.Modifiers) []const u8 {
        return keys.modsString(buf, .{ .shift = m.shift, .alt = m.alt, .ctrl = m.ctrl });
    }

    fn forwardWheel(self: *App, bid: []const u8, x: u16, y: u16, dx: i32, dy: i32, mods: vaxis.Mouse.Modifiers) void {
        const sid = self.sid orelse return;
        var mb: [64]u8 = undefined;
        self.send("block.event", "{{\"session_id\":{f},\"block_id\":{f},\"event\":{{\"type\":\"wheel\",\"payload\":{{\"delta_x\":{d},\"delta_y\":{d},\"x\":{d},\"y\":{d},\"mods\":\"{s}\"}}}}}}", .{ control.q(sid), control.q(bid), dx, dy, x, y, mouseMods(&mb, mods) });
    }

    fn forwardButton(self: *App, bid: []const u8, action: []const u8, button: []const u8, x: u16, y: u16, mods: vaxis.Mouse.Modifiers) void {
        const sid = self.sid orelse return;
        var mb: [64]u8 = undefined;
        const pressed = std.mem.eql(u8, action, "press");
        self.send("block.event", "{{\"session_id\":{f},\"block_id\":{f},\"event\":{{\"type\":\"mouse\",\"payload\":{{\"action\":\"{s}\",\"button\":\"{s}\",\"buttons\":{{{s}}},\"x\":{d},\"y\":{d},\"mods\":\"{s}\"}}}}}}", .{
            control.q(sid), control.q(bid), action, button, if (pressed) (if (std.mem.eql(u8, button, "middle")) "\"middle\":true" else "\"right\":true") else "", x, y, mouseMods(&mb, mods),
        });
    }

    fn uploadFile(self: *App, sid: []const u8, bid: []const u8, path: []const u8) void {
        const bytes = std.Io.Dir.cwd().readFileAlloc(self.io, path, self.gpa, .limited(512 * 1024 * 1024)) catch {
            self.say("can't read {s}", "读不了 {s}", .{path});
            return;
        };
        const job = self.gpa.create(UploadJob) catch return self.gpa.free(bytes);
        job.* = .{ .app = self, .png = bytes, .sid = self.gpa.dupe(u8, sid) catch "", .bid = self.gpa.dupe(u8, bid) catch "", .name = self.gpa.dupe(u8, std.fs.path.basename(path)) catch "" };
        const t = std.Thread.spawn(.{}, UploadJob.run, .{job}) catch return job.free();
        t.detach();
        self.say("uploading {s} ({d} KiB)…", "正在上传 {s}({d} KiB)…", .{ std.fs.path.basename(path), bytes.len / 1024 });
    }

    fn tryPasteImage(self: *App) bool {
        const png = upload.clipboardImage(self.gpa, self.io, self.env) orelse return false;
        const sid = self.sid orelse {
            self.gpa.free(png);
            return false;
        };
        const bid = self.focusedBlock() orelse {
            self.gpa.free(png);
            return false;
        };
        const job = self.gpa.create(UploadJob) catch {
            self.gpa.free(png);
            return false;
        };
        job.* = .{ .app = self, .png = png, .sid = self.gpa.dupe(u8, sid) catch "", .bid = self.gpa.dupe(u8, bid) catch "" };
        const t = std.Thread.spawn(.{}, UploadJob.run, .{job}) catch {
            job.free();
            return false;
        };
        t.detach();
        self.say("uploading image ({d} KiB)…", "正在上传图片({d} KiB)…", .{png.len / 1024});
        return true;
    }

    // ---- render ---------------------------------------------------------------------

    fn sidebarShown(self: *App) bool {
        return self.sidebar orelse (self.vx.window().width >= 110);
    }

    fn render(self: *App) void {
        _ = self.frame.reset(.retain_capacity);
        const arena = self.frame.allocator();
        const win = self.vx.window();
        win.clear();
        win.hideCursor();
        self.last_layout = null;
        self.tab_hits = &.{};
        self.tab_plus = null;
        self.side_w = 0;
        self.side_hits = &.{};
        self.status_buttons = &.{};
        if (win.height < 4 or win.width < 20) return;

        var main = win;
        if (self.sidebarShown()) {
            const sw: u16 = @min(sidebar_width, win.width / 3);
            self.drawSidebar(win.child(.{ .width = sw }), arena);
            self.side_w = sw;
            main = win.child(.{ .x_off = sw, .width = win.width - sw });
        }
        const tabs = main.child(.{ .height = 1 });
        const area = main.child(.{ .y_off = 1, .height = main.height - 2 });
        const status = main.child(.{ .y_off = main.height - 1, .height = 1 });
        self.tab_y = 0;
        self.area_x = @intCast(area.x_off);
        self.area_y = @intCast(area.y_off);

        var keep: std.ArrayList([]const u8) = .empty;
        if (self.settings) |*st| {
            const content = st.draw(area, arena, self.machines(arena), self.config.path, self.config.save_error);
            if (st.page == .server) if (self.panel) |*p| p.draw(content, arena, self.currentMachineName(arena));
            // keep the panes alive behind the panel
            var it = self.panes.keyIterator();
            while (it.next()) |k| keep.append(arena, k.*) catch {};
        } else if (self.activeWindow()) |w| {
            self.drawWindow(w, area, arena, &keep);
        } else {
            _ = area.printSegment(.{ .text = tr("no window — Ctrl-b c to create one", "没有窗口 — 按 Ctrl-b c 新建"), .style = Style.dim }, .{});
        }
        self.prunePanes(keep.items);
        if (self.palette) |*pal| pal.draw(area, arena);
        self.drawTabs(tabs, arena);
        self.status_y = @intCast(status.y_off);
        self.drawStatus(status, arena);
        if (self.help) |*h| h.draw(win, arena);
        if (self.menu) |*mn| mn.draw(win, arena);
        self.vx.render(self.tty.writer()) catch {};
        self.tty.writer().flush() catch {};
    }

    fn drawWindow(self: *App, w: json.Value, area: vaxis.Window, arena: std.mem.Allocator, keep: *std.ArrayList([]const u8)) void {
        const l = layout.compute(arena, w, area.width, area.height) catch return;
        self.last_layout = l;
        const focused = control.getStr(w, "focused_block_id") orelse "";
        const fp = l.find(focused);

        // separators, highlighted around the focused tiled pane
        var y: u16 = 0;
        while (y < area.height) : (y += 1) {
            var x: u16 = 0;
            while (x < area.width) : (x += 1) {
                const f = l.borderAt(x, y);
                if (f == 0) continue;
                var style = Style.border;
                if (fp) |p| if (!p.floating) {
                    const o = p.outer;
                    const near_x = x + 1 >= o.x and x <= o.x + o.w;
                    const near_y = y + 1 >= o.y and y <= o.y + o.h;
                    if (near_x and near_y) style = Style.border_focus;
                };
                area.writeCell(x, y, .{ .char = .{ .grapheme = layout.borderChar(f) }, .style = style });
            }
        }

        var cursor: ?struct { x: u16, y: u16, shape: vaxis.Cell.CursorShape } = null;
        for (l.placements.items) |p| {
            if (p.floating) self.drawBox(area, p.outer, std.mem.eql(u8, p.block_id, focused));
            const cw = area.child(.{ .x_off = p.content.x, .y_off = p.content.y, .width = p.content.w, .height = p.content.h });
            if (p.floating) cw.fill(.{ .char = .{ .grapheme = " " } });
            if (self.self_block) |own| if (std.mem.eql(u8, own, p.block_id)) {
                _ = cw.printSegment(.{ .text = tr("(this pane runs rexa)", "(这个 pane 里运行的就是 rexa)"), .style = Style.dim }, .{});
                continue;
            };
            if (!p.terminal) {
                const msg = i18n.fmt(arena, "{s} (not a terminal)", "{s}(不是终端)", .{p.label});
                _ = cw.printSegment(.{ .text = msg, .style = Style.dim }, .{});
                continue;
            }
            keep.append(arena, p.block_id) catch {};
            self.sizeBlock(p.block_id, p.content.w, p.content.h);
            const pane = self.paneFor(p.block_id) orelse continue;
            switch (pane.status.load(.acquire)) {
                .connecting => _ = cw.printSegment(.{ .text = tr("connecting…", "连接中…"), .style = Style.dim }, .{}),
                .failed => _ = cw.printSegment(.{ .text = arena.dupe(u8, pane.errorText()) catch "", .style = .{ .fg = .{ .index = 203 } } }, .{}),
                .live, .closed => {
                    const r = pane.draw(cw, arena);
                    if (std.mem.eql(u8, p.block_id, focused)) {
                        if (r.cursor) |c| cursor = .{ .x = p.content.x + c.x, .y = p.content.y + c.y, .shape = c.shape };
                    }
                    if (pane.exit_code) |code| {
                        const msg = i18n.fmt(arena, " exited ({d}) ", " 已退出({d}) ", .{code});
                        _ = cw.child(.{ .y_off = cw.height -| 1, .height = 1 }).printSegment(.{ .text = msg, .style = .{ .reverse = true } }, .{});
                    }
                },
            }
        }
        if (cursor) |c| {
            area.showCursor(c.x, c.y);
            area.setCursorShape(c.shape);
        }
    }

    fn drawBox(self: *App, area: vaxis.Window, r: layout.Rect, focused: bool) void {
        _ = self;
        const st = if (focused) Style.border_focus else Style.border;
        if (r.w < 2 or r.h < 2) return;
        var x: u16 = r.x + 1;
        while (x + 1 < r.x + r.w) : (x += 1) {
            area.writeCell(x, r.y, .{ .char = .{ .grapheme = "─" }, .style = st });
            area.writeCell(x, r.y + r.h - 1, .{ .char = .{ .grapheme = "─" }, .style = st });
        }
        var y: u16 = r.y + 1;
        while (y + 1 < r.y + r.h) : (y += 1) {
            area.writeCell(r.x, y, .{ .char = .{ .grapheme = "│" }, .style = st });
            area.writeCell(r.x + r.w - 1, y, .{ .char = .{ .grapheme = "│" }, .style = st });
        }
        area.writeCell(r.x, r.y, .{ .char = .{ .grapheme = "╭" }, .style = st });
        area.writeCell(r.x + r.w - 1, r.y, .{ .char = .{ .grapheme = "╮" }, .style = st });
        area.writeCell(r.x, r.y + r.h - 1, .{ .char = .{ .grapheme = "╰" }, .style = st });
        area.writeCell(r.x + r.w - 1, r.y + r.h - 1, .{ .char = .{ .grapheme = "╯" }, .style = st });
    }

    fn drawTabs(self: *App, win: vaxis.Window, arena: std.mem.Allocator) void {
        win.fill(.{ .style = Style.bar });
        const active = if (self.activeWindow()) |w| control.getStr(w, "window_id") orelse "" else "";
        var hits: std.ArrayList(TabHit) = .empty;
        var col: u16 = 0;
        for (self.windows(), 0..) |w, i| {
            const wid = control.getStr(w, "window_id") orelse continue;
            const is_active = std.mem.eql(u8, wid, active);
            const st = if (is_active) Style.tab_active else Style.tab;
            const icon = stateIcon(self.windowState(w), self.spin);
            var title = self.windowTitle(w);
            if (title.len > 24) title = title[0..24];
            const text = std.fmt.allocPrint(arena, " {d} {s} ", .{ i + 1, title }) catch continue;
            const x0 = col;
            const r = win.print(&.{
                .{ .text = text, .style = st },
                .{ .text = icon[0], .style = .{ .bg = st.bg, .fg = icon[1], .bold = true } },
                .{ .text = " ", .style = st },
            }, .{ .col_offset = col, .wrap = .none });
            // a close button on each tab
            const close_x = r.col;
            const r2 = win.print(&.{.{ .text = "× ", .style = .{ .bg = st.bg, .fg = .{ .index = if (is_active) 252 else 244 } } }}, .{ .col_offset = close_x, .wrap = .none });
            col = r2.col + 1;
            const ox: u16 = @intCast(win.x_off); // hits are in screen columns
            hits.append(arena, .{ .x0 = ox + x0, .x1 = ox + r2.col, .window_id = wid, .title = title, .close_x = ox + close_x }) catch {};
            if (col >= win.width) break;
        }
        if (col + 3 < win.width) {
            _ = win.print(&.{.{ .text = " + ", .style = Style.tab }}, .{ .col_offset = col, .wrap = .none });
            self.tab_plus = .{ @as(u16, @intCast(win.x_off)) + col, @as(u16, @intCast(win.x_off)) + col + 3 };
        }
        self.tab_hits = hits.items;
    }

    /// The machines, with the current one opened up into its sessions:
    ///   ▾ 本机 / ⠋ work / ✓ notes / + new session / ▸ MacBook / + add machine
    fn drawSidebar(self: *App, win: vaxis.Window, arena: std.mem.Allocator) void {
        win.fill(.{ .style = Style.side });
        const head: vaxis.Style = .{ .bg = Style.side.bg, .fg = .{ .index = 244 }, .bold = true };
        const faint: vaxis.Style = .{ .bg = Style.side.bg, .fg = .{ .index = 244 } };
        _ = win.print(&.{.{ .text = tr(" MACHINES", " 机器"), .style = head }}, .{ .wrap = .none });
        var hits: std.ArrayList(SideHit) = .empty;
        const oy: u16 = @intCast(win.y_off);
        const bottom = win.height -| 1; // the last row is the hint
        var row: u16 = 1;
        const cur = self.sid orelse "";
        for (self.machines(arena)) |m| {
            if (row >= bottom) break;
            const mst: vaxis.Style = if (m.current) .{ .bg = .{ .index = 238 }, .fg = .{ .index = 255 }, .bold = true } else Style.side;
            const ml = win.child(.{ .y_off = row, .height = 1 });
            ml.fill(.{ .style = mst });
            _ = ml.print(&.{
                .{ .text = if (m.current) " ▾ " else " ▸ ", .style = .{ .bg = mst.bg, .fg = .{ .index = if (m.current) 114 else 244 } } },
                .{ .text = m.name, .style = mst },
            }, .{ .wrap = .none });
            hits.append(arena, .{ .y = oy + row, .what = .{ .machine = m.spec } }) catch {};
            row += 1;
            if (!m.current) continue;
            for (self.model.sessions.items) |s| {
                if (row >= bottom) break;
                const is_cur = std.mem.eql(u8, s.id, cur);
                const st = if (is_cur) Style.side_current else Style.side;
                const line = win.child(.{ .y_off = row, .height = 1 });
                line.fill(.{ .style = st });
                const icon = stateIcon(self.model.sessionState(s.id), self.spin);
                _ = line.print(&.{
                    .{ .text = "   ", .style = st },
                    .{ .text = icon[0], .style = .{ .bg = st.bg, .fg = icon[1], .bold = true } },
                    .{ .text = " ", .style = st },
                    .{ .text = arena.dupe(u8, s.label) catch "", .style = st },
                }, .{ .wrap = .none });
                hits.append(arena, .{ .y = oy + row, .what = .{ .session = s.id } }) catch {};
                row += 1;
            }
            if (row < bottom) {
                _ = win.child(.{ .y_off = row, .height = 1 }).print(&.{.{ .text = tr("   + new session", "   + 新建 session"), .style = faint }}, .{ .wrap = .none });
                hits.append(arena, .{ .y = oy + row, .what = .new_session }) catch {};
                row += 1;
            }
        }
        if (row < bottom) {
            _ = win.child(.{ .y_off = row, .height = 1 }).print(&.{.{ .text = tr(" + add machine", " + 添加机器"), .style = faint }}, .{ .wrap = .none });
            hits.append(arena, .{ .y = oy + row, .what = .add_machine }) catch {};
        }
        self.side_hits = hits.items;
        _ = win.child(.{ .y_off = win.height - 1, .height = 1 }).print(&.{.{ .text = tr(" right-click for more", " 右键查看更多"), .style = .{ .bg = Style.side.bg, .fg = .{ .index = 242 } } }}, .{ .wrap = .none });
    }

    fn drawStatus(self: *App, win: vaxis.Window, arena: std.mem.Allocator) void {
        win.fill(.{ .style = Style.bar });
        var segs: std.ArrayList(vaxis.Segment) = .empty;
        const label = if (self.sid) |sid| if (self.model.findSession(sid)) |i| self.model.sessions.items[i].label else "" else "";
        segs.append(arena, .{ .text = " ", .style = Style.bar }) catch {};
        segs.append(arena, .{ .text = arena.dupe(u8, label) catch "", .style = Style.tab_active }) catch {};
        segs.append(arena, .{ .text = " ", .style = Style.bar }) catch {};
        if (self.prefix) segs.append(arena, .{ .text = " Ctrl-b ", .style = Style.prefix }) catch {};
        if (self.focusedBlock()) |bid| if (self.panes.get(bid)) |p| if (p.scrollbar()) |sb| {
            if (self.scroll_mode or sb.offset + sb.len < sb.total) {
                const above = sb.total -| (sb.offset + sb.len);
                const t = i18n.fmt(arena, " SCROLL -{d}/{d} ", " 回滚 -{d}/{d} ", .{ above, sb.total -| sb.len });
                segs.append(arena, .{ .text = t, .style = Style.prefix }) catch {};
            }
        };
        if (self.notice_len > 0) {
            segs.append(arena, .{ .text = " ", .style = Style.bar }) catch {};
            segs.append(arena, .{ .text = arena.dupe(u8, self.notice_buf[0..self.notice_len]) catch "", .style = Style.notice }) catch {};
        }
        // clickable buttons on the right; the text on the left is cut short to fit
        const defs = [_]struct { []const u8, menu_mod.Action }{
            .{ if (self.scroll_mode) tr(" ✕ exit scroll ", " ✕ 退出回滚 ") else tr(" ⇅ scroll ", " ⇅ 回滚 "), if (self.scroll_mode) .exit_scroll else .scroll_mode },
            .{ tr(" ⌕ palette ", " ⌕ 命令面板 "), .palette },
            .{ tr(" ⚙ settings ", " ⚙ 设置 "), .settings },
            .{ tr(" ? keys F1 ", " ? 快捷键 F1 "), .help },
        };
        var width: u16 = 0;
        for (defs) |d| width += win.gwidth(d[0]) + 1;
        if (width + 20 > win.width) {
            _ = win.print(segs.items, .{ .wrap = .none });
            return;
        }
        _ = win.child(.{ .width = win.width - width - 1 }).print(segs.items, .{ .wrap = .none });
        var buttons: std.ArrayList(Button) = .empty;
        var col: u16 = win.width - width;
        const chip: vaxis.Style = .{ .bg = .{ .index = 238 }, .fg = .{ .index = 250 } };
        for (defs) |d| {
            const r = win.print(&.{.{ .text = d[0], .style = chip }}, .{ .col_offset = col, .wrap = .none });
            buttons.append(arena, .{ .x0 = @intCast(win.x_off + col), .x1 = @intCast(win.x_off + r.col), .action = d[1] }) catch {};
            col = r.col + 1;
        }
        self.status_buttons = buttons.items;
    }
};

const UploadJob = struct {
    app: *App,
    png: []u8,
    sid: []const u8,
    bid: []const u8,
    name: []const u8 = "",

    fn run(job: *UploadJob) void {
        defer job.free();
        const a = job.app;
        const name = if (job.name.len > 0) job.name else "screenshot.png";
        const zh = i18n.isZh();
        const msg = if (upload.upload(a.gpa, a.io, a.ctl, a.ep, job.sid, job.bid, name, job.png))
            (if (zh) std.fmt.allocPrint(a.gpa, "已上传 {s}", .{name}) else std.fmt.allocPrint(a.gpa, "uploaded {s}", .{name})) catch return
        else |e|
            (if (zh) std.fmt.allocPrint(a.gpa, "上传失败:{s}", .{@errorName(e)}) else std.fmt.allocPrint(a.gpa, "upload failed: {s}", .{@errorName(e)})) catch return;
        a.loop.postEvent(.{ .notice = msg }) catch a.gpa.free(msg);
    }

    fn free(job: *UploadJob) void {
        const g = job.app.gpa;
        g.free(job.png);
        if (job.sid.len > 0) g.free(job.sid);
        if (job.bid.len > 0) g.free(job.bid);
        if (job.name.len > 0) g.free(job.name);
        g.destroy(job);
    }
};
