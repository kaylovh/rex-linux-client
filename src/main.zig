const std = @import("std");
const transport = @import("transport.zig");
const control = @import("control.zig");
const data = @import("data.zig");
const app_mod = @import("app.zig");
const App = app_mod.App;
const config_mod = @import("config.zig");
const i18n = @import("i18n.zig");

pub const usage =
    \\usage: rexa [-S ENDPOINT] [COMMAND]
    \\
    \\  (no command)          open the full client
    \\  attach [SESSION]      open the client on SESSION
    \\  ls                    list sessions
    \\  upload SESSION FILE   upload FILE to the focused pane's machine; its path is typed
    \\  dump SESSION FILE [SECS]
    \\                        save a pane's raw terminal stream (debugging)
    \\
    \\ENDPOINT: unix:///path, ssh://[user@]host[:port], cmd:<bridge command>
    \\Default: $REX_SERVER, then /run/user/$UID/rex/server.sock
    \\
;

pub const Cli = struct {
    ep: transport.Endpoint,
    cmd: []const u8,
    args: []const [:0]const u8,
};

pub fn main(init: std.process.Init) !void {
    const arena = init.arena.allocator();
    const argv = try init.minimal.args.toSlice(arena);
    var i: usize = 1;
    var ep: ?transport.Endpoint = null;
    var server_arg: ?[]const u8 = null;
    while (i < argv.len) : (i += 1) {
        const a = argv[i];
        if (std.mem.eql(u8, a, "-S") or std.mem.eql(u8, a, "--server")) {
            i += 1;
            if (i >= argv.len) return fail("-S needs an endpoint", .{});
            ep = transport.Endpoint.parse(arena, argv[i]) catch return fail("bad endpoint: {s}", .{argv[i]});
            server_arg = argv[i];
        } else if (std.mem.eql(u8, a, "-h") or std.mem.eql(u8, a, "--help")) {
            std.debug.print("{s}", .{usage});
            return;
        } else break;
    }
    const endpoint = ep orelse try transport.Endpoint.defaultLocal(arena, init.environ_map);
    const cmd: []const u8 = if (i < argv.len) argv[i] else "";
    const rest = if (i + 1 <= argv.len) argv[@min(i + 1, argv.len)..] else argv[0..0];

    if (cmd.len == 0 or std.mem.eql(u8, cmd, "attach")) {
        var cfg = config_mod.Config.load(init.gpa, init.io, init.environ_map);
        defer cfg.deinit();
        i18n.lang = cfg.lang;
        var session: ?[]const u8 = if (cmd.len > 0 and rest.len > 0) rest[0] else null;
        var ep_now = endpoint;
        var spec: []const u8 = server_arg orelse "";
        // where we were before switching machines: go back there if the new one fails
        var prev: ?struct { ep: transport.Endpoint, spec: []const u8 } = null;
        var start_notice: []const u8 = "";
        while (true) {
            var msg_buf: [256]u8 = undefined;
            const msg = App.run(init.gpa, init.io, init.environ_map, ep_now, .{ .session = session, .spec = spec, .config = &cfg, .notice = start_notice }, &msg_buf) catch |e| {
                const why: []const u8 = switch (e) {
                    error.NoSession => i18n.t("no session matches", "没有匹配的 session"),
                    error.ServerNotRunning => i18n.t("no Rex server is running there", "那里没有在运行的 Rex 服务端"),
                    error.AccessDenied => i18n.t("not allowed to connect to that Rex server", "没有权限连接那个 Rex 服务端"),
                    else => if (std.mem.startsWith(u8, spec, "ssh://"))
                        // the bridge (ssh) died before the handshake: ssh or rex failed over there
                        i18n.t("ssh failed (try `ssh HOST rex ls` in a terminal)", "ssh 失败(在终端里试试 `ssh 主机 rex ls`)")
                    else
                        @errorName(e),
                };
                if (prev) |pv| {
                    // the machine the user picked can't be reached: back to the last one
                    const name = if (spec.len == 0) i18n.t("this computer", "本机") else if (cfg.find(spec)) |si| cfg.servers.items[si].name else spec;
                    start_notice = try std.fmt.allocPrint(arena, "{s} {s}: {s}", .{ i18n.t("can't connect to", "连不上"), name, why });
                    ep_now = pv.ep;
                    spec = pv.spec;
                    prev = null;
                    session = null;
                    continue;
                }
                if (e == error.NoSession) return fail("{s} \"{s}\" (rexa ls)", .{ why, session orelse "" });
                return fail("{s}", .{why});
            };
            start_notice = "";
            if (std.mem.startsWith(u8, msg, app_mod.switch_prefix)) {
                // The user picked another machine: reconnect there ("" = this computer).
                const target = try arena.dupe(u8, msg[app_mod.switch_prefix.len..]);
                const next_ep = if (target.len == 0)
                    try transport.Endpoint.defaultLocal(arena, init.environ_map)
                else
                    transport.Endpoint.parse(arena, target) catch {
                        start_notice = try std.fmt.allocPrint(arena, "{s}: {s}", .{ i18n.t("bad address", "地址写法不对"), target });
                        continue;
                    };
                prev = .{ .ep = ep_now, .spec = spec };
                std.debug.print("rexa: {s} {s}…\n", .{ i18n.t("connecting to", "正在连接"), if (target.len > 0) target else i18n.t("this computer", "本机") });
                ep_now = next_ep;
                spec = target;
                session = null;
                continue;
            }
            prev = null;
            if (msg.len > 0) std.debug.print("[rexa: {s}]\n", .{msg});
            return;
        }
    }
    if (std.mem.eql(u8, cmd, "ls")) return ls(init, endpoint);
    if (std.mem.eql(u8, cmd, "upload")) return uploadCmd(init, endpoint, rest);
    if (std.mem.eql(u8, cmd, "dump")) return dump(init, endpoint, rest);
    return fail("unknown command: {s}\n{s}", .{ cmd, usage });
}

fn fail(comptime fmt: []const u8, args: anytype) error{Failed} {
    std.debug.print("rexa: " ++ fmt ++ "\n", args);
    return error.Failed;
}

fn connect(init: std.process.Init, ep: transport.Endpoint) !*control.Control {
    return control.Control.connect(init.gpa, init.io, ep) catch |e|
        return fail("cannot connect to the Rex server: {s}", .{@errorName(e)});
}

fn ls(init: std.process.Init, ep: transport.Endpoint) !void {
    const ctl = try connect(init, ep);
    defer ctl.close();
    try ctl.start(null, null);
    const r = ctl.call("session.list", "{}") catch return fail("{s}", .{ctl.lastError()});
    defer r.deinit();
    for (control.getArr(r.value, "sessions")) |s| {
        std.debug.print("{s}  {s}\n", .{ control.getStr(s, "session_id") orelse "?", control.getStr(s, "label") orelse "" });
    }
}

fn dump(init: std.process.Init, ep: transport.Endpoint, args: []const [:0]const u8) !void {
    if (args.len < 2) return fail("dump SESSION FILE [SECS]", .{});
    const gpa = init.gpa;
    const io = init.io;
    const secs: i64 = if (args.len > 2) std.fmt.parseInt(i64, args[2], 10) catch 2 else 2;
    const ctl = try connect(init, ep);
    defer ctl.close();
    try ctl.start(null, null);

    const sid = try resolveSession(init.arena.allocator(), ctl, args[0]);
    var buf: [512]u8 = undefined;
    const att = ctl.call("session.attach", try std.fmt.bufPrint(&buf, "{{\"session_id\":{f}}}", .{control.q(sid)})) catch
        return fail("{s}", .{ctl.lastError()});
    defer att.deinit();
    const view = control.getObj(att.value, "view") orelse return fail("no view", .{});
    const bid = focusedBlock(view) orelse return fail("session has no focused block", .{});

    const t = ctl.call("data.connect", try std.fmt.bufPrint(&buf,
        \\{{"session_id":{f},"block_id":{f},"data_ref_id":"terminal","subprotocol":"rex.data.terminal.v1"}}
    , .{ control.q(sid), control.q(bid) })) catch return fail("{s}", .{ctl.lastError()});
    defer t.deinit();
    var dc = data.DataConn.open(gpa, io, ep, control.getStr(t.value, "ticket") orelse "", data.terminal_v1) catch |e|
        return fail("data connection: {s}", .{@errorName(e)});

    const file = try std.Io.Dir.cwd().createFile(io, args[1], .{});
    defer file.close(io);
    const Reader = struct {
        fn run(d: *data.DataConn, g: std.mem.Allocator, f: std.Io.File, iio: std.Io, total: *usize) void {
            while (true) {
                const item = d.next(g) catch return;
                switch (item) {
                    .bytes => |b| {
                        defer g.free(b);
                        f.writeStreamingAll(iio, b) catch return;
                        total.* += b.len;
                    },
                    .closed => return,
                }
            }
        }
    };
    var total: usize = 0;
    const th = try std.Thread.spawn(.{}, Reader.run, .{ &dc, gpa, file, io, &total });
    io.sleep(.fromSeconds(secs), .awake) catch {};
    dc.shutdown();
    th.join();
    std.debug.print("nonce={d} received={d} bytes -> {s}\n", .{ dc.nonce, total, args[1] });
    dc.sock.destroy(gpa);
}

fn uploadCmd(init: std.process.Init, ep: transport.Endpoint, args: []const [:0]const u8) !void {
    if (args.len < 2) return fail("upload SESSION FILE", .{});
    const gpa = init.gpa;
    const ctl = try connect(init, ep);
    defer ctl.close();
    try ctl.start(null, null);
    const sid = try resolveSession(init.arena.allocator(), ctl, args[0]);
    var buf: [512]u8 = undefined;
    const att = ctl.call("session.attach", try std.fmt.bufPrint(&buf, "{{\"session_id\":{f}}}", .{control.q(sid)})) catch
        return fail("{s}", .{ctl.lastError()});
    defer att.deinit();
    const bid = focusedBlock(control.getObj(att.value, "view") orelse return fail("no view", .{})) orelse return fail("no focused block", .{});
    const bytes = std.Io.Dir.cwd().readFileAlloc(init.io, args[1], gpa, .unlimited) catch |e| return fail("{s}: {s}", .{ args[1], @errorName(e) });
    defer gpa.free(bytes);
    const upload = @import("upload.zig");
    upload.upload(gpa, init.io, ctl, ep, sid, bid, std.fs.path.basename(args[1]), bytes) catch |e| return fail("upload: {s}", .{@errorName(e)});
    std.debug.print("uploaded {s} ({d} bytes)\n", .{ args[1], bytes.len });
}

pub fn resolveSession(arena: std.mem.Allocator, ctl: *control.Control, q: []const u8) ![]const u8 {
    if (std.mem.startsWith(u8, q, "session:")) return q;
    const payload = try std.fmt.allocPrint(arena, "{{\"kinds\":[\"session\"],\"queries\":[{f}]}}", .{control.q(q)});
    const r = ctl.call("target.resolve", payload) catch return fail("{s}", .{ctl.lastError()});
    defer r.deinit();
    const res = control.getArr(r.value, "results");
    if (res.len == 0 or !control.eqlStr(res[0], "status", "resolved")) return fail("no session matches {s}", .{q});
    const target = control.getObj(res[0], "target") orelse return fail("bad resolve result", .{});
    return arena.dupe(u8, control.getStr(target, "session_id") orelse return fail("bad resolve result", .{}));
}

pub fn focusedBlock(view: std.json.Value) ?[]const u8 {
    const active = control.getStr(view, "active_window_id") orelse return null;
    for (control.getArr(view, "windows")) |w| {
        if (control.eqlStr(w, "window_id", active)) return control.getStr(w, "focused_block_id");
    }
    return null;
}

test {
    _ = transport;
    _ = @import("ws.zig");
    _ = @import("keys.zig");
    _ = @import("rexscan.zig");
    _ = @import("pane.zig");
    _ = @import("app.zig");
    _ = @import("layout.zig");
    _ = @import("model.zig");
    _ = @import("panel.zig");
    _ = @import("palette.zig");
    _ = @import("config.zig");
}
