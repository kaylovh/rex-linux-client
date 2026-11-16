//! Upload a file to a terminal's machine (`upload_begin` + `rex.data.bytes.v1`). When it
//! arrives the terminal types its path as a paste, which is how a screenshot reaches a
//! program such as Claude Code, on this machine or a remote one.

const std = @import("std");
const control = @import("control.zig");
const data = @import("data.zig");
const transport = @import("transport.zig");

pub fn uuid(io: std.Io, out: *[36]u8) void {
    var b: [16]u8 = undefined;
    io.random(&b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    const hex = std.fmt.bytesToHex(b, .lower);
    _ = std.fmt.bufPrint(out, "{s}-{s}-{s}-{s}-{s}", .{ hex[0..8], hex[8..12], hex[12..16], hex[16..20], hex[20..32] }) catch unreachable;
}

pub fn upload(gpa: std.mem.Allocator, io: std.Io, ctl: *control.Control, ep: transport.Endpoint, sid: []const u8, bid: []const u8, name: []const u8, bytes: []const u8) !void {
    var id: [36]u8 = undefined;
    uuid(io, &id);
    const payload = try std.fmt.allocPrint(gpa,
        \\{{"session_id":{f},"block_id":{f},"args":{{"ref":"upload:{s}","name":{f},"size":{d}}}}}
    , .{ control.q(sid), control.q(bid), &id, control.q(name), bytes.len });
    defer gpa.free(payload);
    const r = ctl.call("com.superlogical.terminal.upload_begin", payload) catch return error.UploadRefused;
    defer r.deinit();
    const ticket = control.getStr(r.value, "ticket") orelse return error.UploadRefused;
    var dc = try data.DataConn.open(gpa, io, ep, ticket, data.bytes_v1);
    defer dc.sock.destroy(gpa);
    var off: usize = 0;
    while (off < bytes.len) {
        const n = @min(64 * 1024, bytes.len - off);
        try dc.send(bytes[off..][0..n]);
        off += n;
    }
    try dc.send(""); // half-close: end of file
    while (true) {
        const item = try dc.next(gpa);
        switch (item) {
            .bytes => |b| gpa.free(b),
            .closed => |c| return if (c.code == 1000) {} else error.UploadFailed,
        }
    }
}

/// The clipboard's image as PNG, if it holds one. Caller frees.
pub fn clipboardImage(gpa: std.mem.Allocator, io: std.Io, env: *const std.process.Environ.Map) ?[]u8 {
    if (env.get("WAYLAND_DISPLAY") != null) {
        const types = runOut(gpa, io, &.{ "wl-paste", "--list-types" }) orelse return null;
        defer gpa.free(types);
        if (!hasLine(types, "image/png")) return null;
        return runOut(gpa, io, &.{ "wl-paste", "--no-newline", "--type", "image/png" });
    }
    if (env.get("DISPLAY") != null) {
        const targets = runOut(gpa, io, &.{ "xclip", "-selection", "clipboard", "-t", "TARGETS", "-o" }) orelse return null;
        defer gpa.free(targets);
        if (!hasLine(targets, "image/png")) return null;
        return runOut(gpa, io, &.{ "xclip", "-selection", "clipboard", "-t", "image/png", "-o" });
    }
    return null;
}

/// The clipboard's text (or, with `primary`, the current selection's). Caller frees.
pub fn clipboardText(gpa: std.mem.Allocator, io: std.Io, env: *const std.process.Environ.Map, primary: bool) ?[]u8 {
    if (env.get("WAYLAND_DISPLAY") != null) {
        const argv: []const []const u8 = if (primary) &.{ "wl-paste", "--no-newline", "--primary", "--type", "text" } else &.{ "wl-paste", "--no-newline", "--type", "text" };
        if (runOut(gpa, io, argv)) |t| return t;
    }
    if (env.get("DISPLAY") != null) {
        return runOut(gpa, io, &.{ "xclip", "-selection", if (primary) "primary" else "clipboard", "-o" });
    }
    return null;
}

fn hasLine(text: []const u8, want: []const u8) bool {
    var it = std.mem.tokenizeAny(u8, text, "\r\n");
    while (it.next()) |l| if (std.mem.eql(u8, std.mem.trim(u8, l, " "), want)) return true;
    return false;
}

fn runOut(gpa: std.mem.Allocator, io: std.Io, argv: []const []const u8) ?[]u8 {
    const r = std.process.run(gpa, io, .{ .argv = argv }) catch return null;
    gpa.free(r.stderr);
    const ok = switch (r.term) {
        .exited => |c| c == 0,
        else => false,
    };
    if (!ok or r.stdout.len == 0) {
        gpa.free(r.stdout);
        return null;
    }
    return r.stdout;
}

/// Splits pasted text the way a terminal pastes dropped files: paths separated by
/// spaces or newlines, with shell escaping ('a b', "a b", a\ b). Returns null unless
/// every word is an absolute path to an existing regular file on this machine.
pub fn droppedFiles(arena: std.mem.Allocator, io: std.Io, text: []const u8) ?[]const []const u8 {
    var out: std.ArrayList([]const u8) = .empty;
    var cur: std.ArrayList(u8) = .empty;
    var quote: u8 = 0;
    var i: usize = 0;
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0 or t.len > 16 * 1024) return null;
    while (i < t.len) : (i += 1) {
        const c = t[i];
        if (quote != 0) {
            if (c == quote) quote = 0 else cur.append(arena, c) catch return null;
        } else if (c == '\'' or c == '"') {
            quote = c;
        } else if (c == '\\' and i + 1 < t.len) {
            i += 1;
            cur.append(arena, t[i]) catch return null;
        } else if (c == ' ' or c == '\n' or c == '\r' or c == '\t') {
            if (cur.items.len > 0) {
                out.append(arena, cur.toOwnedSlice(arena) catch return null) catch return null;
            }
        } else cur.append(arena, c) catch return null;
    }
    if (quote != 0) return null;
    if (cur.items.len > 0) out.append(arena, cur.toOwnedSlice(arena) catch return null) catch return null;
    if (out.items.len == 0 or out.items.len > 32) return null;
    for (out.items, 0..) |path, j| {
        var p = path;
        if (std.mem.startsWith(u8, p, "file://")) p = p["file://".len..];
        if (!std.mem.startsWith(u8, p, "/")) return null;
        const st = std.Io.Dir.cwd().statFile(io, p, .{}) catch return null;
        if (st.kind != .file) return null;
        out.items[j] = p;
    }
    return out.items;
}

test "dropped file parsing rejects non-paths" {
    var arena: std.heap.ArenaAllocator = .init(std.testing.allocator);
    defer arena.deinit();
    try std.testing.expect(droppedFiles(arena.allocator(), std.testing.io, "echo hello") == null);
    try std.testing.expect(droppedFiles(arena.allocator(), std.testing.io, "/definitely/not/here.png") == null);
}
