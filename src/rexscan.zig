//! Splits a terminal stream into VT bytes and Rex's own in-band messages
//! (`ESC _ rex1;<nonce>;<json> ESC \`), preserving order across chunk boundaries.
//! APCs that aren't ours (wrong nonce, other protocols) stay in the byte stream.

const std = @import("std");

const ESC = 0x1b;
const max_apc = 1 << 20;

pub const Sink = struct {
    ctx: *anyopaque,
    bytes: *const fn (ctx: *anyopaque, b: []const u8) void,
    message: *const fn (ctx: *anyopaque, json: []const u8) void,
};

pub const Scanner = struct {
    gpa: std.mem.Allocator,
    nonce_buf: [24]u8 = undefined,
    nonce_len: usize = 0,
    state: enum { normal, esc, apc } = .normal,
    apc: std.ArrayList(u8) = .empty,
    apc_esc: bool = false,

    pub fn init(gpa: std.mem.Allocator, nonce: u64) Scanner {
        var s: Scanner = .{ .gpa = gpa };
        s.nonce_len = (std.fmt.bufPrint(&s.nonce_buf, "{d}", .{nonce}) catch unreachable).len;
        return s;
    }

    pub fn deinit(self: *Scanner) void {
        self.apc.deinit(self.gpa);
    }

    pub fn feed(self: *Scanner, input: []const u8, sink: Sink) void {
        var run_start: usize = 0;
        var i: usize = 0;
        while (i < input.len) : (i += 1) {
            const b = input[i];
            switch (self.state) {
                .normal => if (b == ESC) {
                    // Flush the plain run before this ESC; the ESC is held.
                    if (i > run_start) sink.bytes(sink.ctx, input[run_start..i]);
                    self.state = .esc;
                },
                .esc => {
                    if (b == '_') {
                        self.state = .apc;
                        self.apc.clearRetainingCapacity();
                        self.apc_esc = false;
                    } else {
                        sink.bytes(sink.ctx, "\x1b");
                        if (b == ESC) {
                            self.state = .esc;
                        } else {
                            self.state = .normal;
                            run_start = i;
                            continue;
                        }
                    }
                    run_start = i + 1;
                },
                .apc => {
                    if (self.apc_esc and b == '\\') {
                        self.finishApc(sink);
                        self.state = .normal;
                        run_start = i + 1;
                        continue;
                    }
                    if (self.apc_esc) self.apc.append(self.gpa, ESC) catch {};
                    self.apc_esc = b == ESC;
                    if (!self.apc_esc) self.apc.append(self.gpa, b) catch {};
                    if (self.apc.items.len > max_apc) {
                        sink.bytes(sink.ctx, "\x1b_");
                        sink.bytes(sink.ctx, self.apc.items);
                        self.apc.clearRetainingCapacity();
                        self.state = .normal;
                    }
                    run_start = i + 1;
                },
            }
        }
        if (self.state == .normal and run_start < input.len) sink.bytes(sink.ctx, input[run_start..]);
    }

    fn finishApc(self: *Scanner, sink: Sink) void {
        const body = self.apc.items;
        if (std.mem.startsWith(u8, body, "rex1;")) {
            const rest = body[5..];
            if (std.mem.indexOfScalar(u8, rest, ';')) |semi| {
                if (std.mem.eql(u8, rest[0..semi], self.nonce_buf[0..self.nonce_len])) {
                    sink.message(sink.ctx, rest[semi + 1 ..]);
                    return;
                }
            }
        }
        sink.bytes(sink.ctx, "\x1b_");
        sink.bytes(sink.ctx, body);
        sink.bytes(sink.ctx, "\x1b\\");
    }
};

const TestSink = struct {
    out: std.ArrayList(u8) = .empty,
    msgs: std.ArrayList(u8) = .empty,
    fn b(ctx: *anyopaque, x: []const u8) void {
        const s: *TestSink = @ptrCast(@alignCast(ctx));
        s.out.appendSlice(std.testing.allocator, x) catch {};
    }
    fn m(ctx: *anyopaque, x: []const u8) void {
        const s: *TestSink = @ptrCast(@alignCast(ctx));
        s.out.appendSlice(std.testing.allocator, "<MSG>") catch {};
        s.msgs.appendSlice(std.testing.allocator, x) catch {};
    }
};

test "messages split across chunks keep their place" {
    var s: Scanner = .init(std.testing.allocator, 42);
    defer s.deinit();
    var ts: TestSink = .{};
    defer ts.out.deinit(std.testing.allocator);
    defer ts.msgs.deinit(std.testing.allocator);
    const sink: Sink = .{ .ctx = &ts, .bytes = TestSink.b, .message = TestSink.m };
    s.feed("hi\x1b[31m\x1b_rex1;42;{\"type\":\"re", sink);
    s.feed("size\"}\x1b", sink);
    s.feed("\\there\x1b_rex1;7;{}\x1b\\!", sink);
    try std.testing.expectEqualStrings("hi\x1b[31m<MSG>there\x1b_rex1;7;{}\x1b\\!", ts.out.items);
    try std.testing.expectEqualStrings("{\"type\":\"resize\"}", ts.msgs.items);
}
