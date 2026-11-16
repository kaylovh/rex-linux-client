//! Control connection (`rex.control.v1`): JSON hello/welcome, then request/response
//! with string ids, plus pushed events. A reader thread dispatches responses to waiting
//! callers and events to a callback. See the server's own docs at GET /llms.txt.

const std = @import("std");
const ws = @import("ws.zig");
const transport = @import("transport.zig");
const json = std.json;

pub const api_revision = 14;
pub const Parsed = json.Parsed(json.Value);

pub const EventFn = *const fn (ctx: ?*anyopaque, ev: Parsed) void;

/// A method's result: `value` is the response's payload.
pub const Result = struct {
    parsed: Parsed,
    value: json.Value,

    pub fn deinit(self: Result) void {
        self.parsed.deinit();
    }
};

pub const CallError = error{ RequestFailed, ConnectionClosed, OutOfMemory, WriteFailed, BadResponse };

const Pending = struct {
    done: std.Io.Event = .unset,
    result: ?Parsed = null,
    err_code: [48]u8 = undefined,
    err_code_len: usize = 0,
    err_msg: [256]u8 = undefined,
    err_msg_len: usize = 0,
    failed: bool = false,
};

pub const Control = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    ep: transport.Endpoint,
    sock: *ws.Ws,
    /// sockets replaced by reconnects; freed at close (a sender may still hold one)
    old_socks: std.ArrayList(*ws.Ws) = .empty,
    sock_lock: std.Io.Mutex = .init,
    client_id: []u8,
    reconnect_token: [96]u8 = undefined,
    reconnect_token_len: usize = 0,
    closing: std.atomic.Value(bool) = .init(false),
    next_id: std.atomic.Value(u64) = .init(1),
    lock: std.Io.Mutex = .init,
    pending: std.AutoHashMapUnmanaged(u64, *Pending) = .empty,
    on_event: ?EventFn = null,
    event_ctx: ?*anyopaque = null,
    reader: ?std.Thread = null,
    alive: std.atomic.Value(bool) = .init(true),
    /// Last error from a failed call, for messages to the user.
    last_error: [320]u8 = undefined,
    last_error_len: usize = 0,

    const Welcome = struct { sock: *ws.Ws, client_id: []u8, token: [96]u8, token_len: usize, resumed: bool };

    fn handshake(gpa: std.mem.Allocator, io: std.Io, ep: transport.Endpoint, token: ?[]const u8) !Welcome {
        const sock = try ws.Ws.connect(gpa, io, ep, "/api/control", "rex.control.v1");
        errdefer sock.destroy(gpa);
        var hb: [512]u8 = undefined;
        const hello = if (token) |t| try std.fmt.bufPrint(&hb,
            \\{{"type":"hello","client":{{"kind":"cli","platform":"linux","name":"rexa","version":"0.1.0","reconnect_token":{f}}},"api_revision":{d}}}
        , .{ q(t), api_revision }) else try std.fmt.bufPrint(&hb,
            \\{{"type":"hello","client":{{"kind":"cli","platform":"linux","name":"rexa","version":"0.1.0"}},"api_revision":{d}}}
        , .{api_revision});
        try sock.send(.text, hello);
        const msg = try sock.readMessage(gpa);
        defer msg.deinit(gpa);
        if (msg.op != .text) return error.NoWelcome;
        const parsed = try json.parseFromSlice(json.Value, gpa, msg.data, .{});
        defer parsed.deinit();
        const v = parsed.value;
        if (!eqlStr(v, "type", "welcome") or !eqlStr(v, "protocol", "rex.control.v1")) return error.NoWelcome;
        const client = getObj(v, "client") orelse return error.NoWelcome;
        const cid = getStr(client, "id") orelse return error.NoWelcome;
        var w: Welcome = .{ .sock = sock, .client_id = try gpa.dupe(u8, cid), .token = undefined, .token_len = 0, .resumed = getBool(client, "resumed") orelse false };
        const tok = getStr(client, "reconnect_token") orelse "";
        w.token_len = @min(tok.len, w.token.len);
        @memcpy(w.token[0..w.token_len], tok[0..w.token_len]);
        return w;
    }

    pub fn connect(gpa: std.mem.Allocator, io: std.Io, ep: transport.Endpoint) !*Control {
        const w = try handshake(gpa, io, ep, null);
        const self = try gpa.create(Control);
        self.* = .{ .gpa = gpa, .io = io, .ep = ep, .sock = w.sock, .client_id = w.client_id, .reconnect_token = w.token, .reconnect_token_len = w.token_len };
        return self;
    }

    fn currentSock(self: *Control) *ws.Ws {
        self.sock_lock.lockUncancelable(self.io);
        defer self.sock_lock.unlock(self.io);
        return self.sock;
    }

    fn emit(self: *Control, comptime text: []const u8, args: anytype) void {
        const f = self.on_event orelse return;
        var b: [256]u8 = undefined;
        const s2 = std.fmt.bufPrint(&b, text, args) catch return;
        const ev = json.parseFromSlice(json.Value, self.gpa, s2, .{}) catch return;
        f(self.event_ctx, ev);
    }

    /// After the connection dropped: try again (resuming this client) until it works
    /// or close() is called.
    fn reconnect(self: *Control) bool {
        self.emit("{{\"type\":\"rexa_reconnecting\"}}", .{});
        var delay_ms: i64 = 200;
        while (!self.closing.load(.acquire)) {
            self.io.sleep(.fromMilliseconds(delay_ms), .awake) catch {};
            if (self.closing.load(.acquire)) return false;
            const w = handshake(self.gpa, self.io, self.ep, self.reconnect_token[0..self.reconnect_token_len]) catch {
                delay_ms = @min(delay_ms * 2, 5000);
                continue;
            };
            self.sock_lock.lockUncancelable(self.io);
            self.sock.conn.shutdown();
            self.sock.conn.reap();
            self.old_socks.append(self.gpa, self.sock) catch {};
            self.sock = w.sock;
            self.reconnect_token = w.token;
            self.reconnect_token_len = w.token_len;
            const changed = !std.mem.eql(u8, self.client_id, w.client_id);
            if (changed) {
                self.gpa.free(self.client_id);
                self.client_id = w.client_id;
            } else self.gpa.free(w.client_id);
            self.sock_lock.unlock(self.io);
            self.alive.store(true, .release);
            self.emit("{{\"type\":\"rexa_reconnected\",\"resumed\":{}}}", .{w.resumed and !changed});
            return true;
        }
        return false;
    }

    /// Starts the reader thread. Set on_event first if you want events.
    pub fn start(self: *Control, on_event: ?EventFn, ctx: ?*anyopaque) !void {
        self.on_event = on_event;
        self.event_ctx = ctx;
        self.reader = try std.Thread.spawn(.{}, readLoop, .{self});
    }

    fn readLoop(self: *Control) void {
        while (true) {
            self.readUntilClosed();
            self.failPending();
            if (self.closing.load(.acquire) or !self.reconnect()) break;
        }
        if (self.on_event) |f| {
            const closed = json.parseFromSlice(json.Value, self.gpa,
                \\{"type":"rexa_disconnected"}
            , .{}) catch return;
            f(self.event_ctx, closed);
        }
    }

    fn readUntilClosed(self: *Control) void {
        const sock = self.currentSock();
        while (true) {
            const msg = sock.readMessage(self.gpa) catch break;
            if (msg.op == .close) {
                msg.deinit(self.gpa);
                break;
            }
            defer msg.deinit(self.gpa);
            if (msg.op != .text) continue;
            const parsed = json.parseFromSlice(json.Value, self.gpa, msg.data, .{}) catch continue;
            const v = parsed.value;
            const typ = getStr(v, "type") orelse "";
            if (std.mem.eql(u8, typ, "response")) {
                const id_s = getStr(v, "id") orelse "";
                const id = parseId(id_s) orelse {
                    parsed.deinit();
                    continue;
                };
                self.lock.lockUncancelable(self.io);
                const p = self.pending.fetchRemove(id);
                self.lock.unlock(self.io);
                if (p) |kv| {
                    const pend = kv.value;
                    if (getObj(v, "error")) |e| {
                        pend.failed = true;
                        const code = getStr(e, "code") orelse "error";
                        const m = getStr(e, "message") orelse "";
                        pend.err_code_len = @min(code.len, pend.err_code.len);
                        @memcpy(pend.err_code[0..pend.err_code_len], code[0..pend.err_code_len]);
                        pend.err_msg_len = @min(m.len, pend.err_msg.len);
                        @memcpy(pend.err_msg[0..pend.err_msg_len], m[0..pend.err_msg_len]);
                        parsed.deinit();
                    } else {
                        pend.result = parsed;
                    }
                    pend.done.set(self.io);
                } else parsed.deinit();
            } else if (std.mem.eql(u8, typ, "event")) {
                if (self.on_event) |f| f(self.event_ctx, parsed) else parsed.deinit();
            } else parsed.deinit();
        }
    }

    fn failPending(self: *Control) void {
        self.alive.store(false, .release);
        self.lock.lockUncancelable(self.io);
        var it = self.pending.valueIterator();
        while (it.next()) |p| {
            p.*.failed = true;
            const m = "control connection closed";
            @memcpy(p.*.err_msg[0..m.len], m);
            p.*.err_msg_len = m.len;
            p.*.done.set(self.io);
        }
        self.pending.clearRetainingCapacity();
        self.lock.unlock(self.io);
    }

    fn parseId(s: []const u8) ?u64 {
        if (!std.mem.startsWith(u8, s, "x")) return null;
        return std.fmt.parseInt(u64, s[1..], 10) catch null;
    }

    fn sendRequest(self: *Control, id: u64, method: []const u8, payload: ?[]const u8) !void {
        var buf: std.Io.Writer.Allocating = .init(self.gpa);
        defer buf.deinit();
        const w = &buf.writer;
        try w.print("{{\"type\":\"request\",\"id\":\"x{d}\",\"method\":\"{s}\"", .{ id, method });
        if (payload) |p| try w.print(",\"payload\":{s}", .{p});
        try w.writeAll("}");
        try self.currentSock().send(.text, buf.written());
    }

    /// Blocking call. Caller owns the result (deinit it).
    pub fn call(self: *Control, method: []const u8, payload: ?[]const u8) CallError!Result {
        if (!self.alive.load(.acquire)) return error.ConnectionClosed;
        const id = self.next_id.fetchAdd(1, .monotonic);
        var pend: Pending = .{};
        self.lock.lockUncancelable(self.io);
        self.pending.put(self.gpa, id, &pend) catch {
            self.lock.unlock(self.io);
            return error.OutOfMemory;
        };
        self.lock.unlock(self.io);
        self.sendRequest(id, method, payload) catch {
            self.lock.lockUncancelable(self.io);
            _ = self.pending.remove(id);
            self.lock.unlock(self.io);
            return error.ConnectionClosed;
        };
        pend.done.waitUncancelable(self.io);
        if (pend.failed) {
            self.lock.lockUncancelable(self.io);
            const m = std.fmt.bufPrint(&self.last_error, "{s}: {s}: {s}", .{
                method, pend.err_code[0..pend.err_code_len], pend.err_msg[0..pend.err_msg_len],
            }) catch self.last_error[0..0];
            self.last_error_len = m.len;
            self.lock.unlock(self.io);
            return error.RequestFailed;
        }
        const parsed = pend.result orelse return error.BadResponse;
        return .{ .parsed = parsed, .value = get(parsed.value, "payload") orelse .null };
    }

    /// Fire and forget: the response is dropped. Ordered methods (client.key,
    /// block.event, ...) still run in send order on the server.
    pub fn send(self: *Control, method: []const u8, payload: ?[]const u8) void {
        const id = self.next_id.fetchAdd(1, .monotonic);
        self.sendRequest(id, method, payload) catch {};
    }

    pub fn lastError(self: *Control) []const u8 {
        return self.last_error[0..self.last_error_len];
    }

    pub fn close(self: *Control) void {
        self.closing.store(true, .release);
        const sock = self.currentSock();
        sock.sendClose(1000);
        sock.shutdown();
        if (self.reader) |t| t.join();
        self.sock.destroy(self.gpa);
        for (self.old_socks.items) |s2| s2.destroy(self.gpa);
        self.old_socks.deinit(self.gpa);
        self.pending.deinit(self.gpa);
        self.gpa.free(self.client_id);
        self.gpa.destroy(self);
    }
};

// ---- JSON helpers ----------------------------------------------------------------

pub fn getObj(v: json.Value, key: []const u8) ?json.Value {
    if (v != .object) return null;
    const x = v.object.get(key) orelse return null;
    return if (x == .object) x else null;
}

pub fn getArr(v: json.Value, key: []const u8) []json.Value {
    if (v != .object) return &.{};
    const x = v.object.get(key) orelse return &.{};
    return if (x == .array) x.array.items else &.{};
}

pub fn getStr(v: json.Value, key: []const u8) ?[]const u8 {
    if (v != .object) return null;
    const x = v.object.get(key) orelse return null;
    return if (x == .string) x.string else null;
}

pub fn getInt(v: json.Value, key: []const u8) ?i64 {
    if (v != .object) return null;
    const x = v.object.get(key) orelse return null;
    return switch (x) {
        .integer => |i| i,
        .float => |f| @intFromFloat(f),
        else => null,
    };
}

pub fn getFloat(v: json.Value, key: []const u8) ?f64 {
    if (v != .object) return null;
    const x = v.object.get(key) orelse return null;
    return switch (x) {
        .integer => |i| @floatFromInt(i),
        .float => |f| f,
        else => null,
    };
}

pub fn getBool(v: json.Value, key: []const u8) ?bool {
    if (v != .object) return null;
    const x = v.object.get(key) orelse return null;
    return if (x == .bool) x.bool else null;
}

pub fn get(v: json.Value, key: []const u8) ?json.Value {
    if (v != .object) return null;
    return v.object.get(key);
}

pub fn eqlStr(v: json.Value, key: []const u8, want: []const u8) bool {
    const s = getStr(v, key) orelse return false;
    return std.mem.eql(u8, s, want);
}

/// JSON-quoted string for building payloads with std.fmt: `{f}`.
pub fn q(s: []const u8) std.json.Formatter([]const u8) {
    return std.json.fmt(s, .{});
}
