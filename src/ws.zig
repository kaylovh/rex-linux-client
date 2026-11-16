//! Minimal RFC 6455 WebSocket client over a transport Conn. No extensions (we never
//! offer permessage-deflate). Reads are single-threaded; writes are serialized by a
//! mutex so any thread may send.

const std = @import("std");
const transport = @import("transport.zig");

pub const Opcode = enum(u4) {
    continuation = 0,
    text = 1,
    binary = 2,
    close = 8,
    ping = 9,
    pong = 10,
    _,
};

pub const Message = struct {
    op: Opcode,
    data: []u8,
    /// For close messages: the status code (1005 if none was given).
    close_code: u16 = 0,

    pub fn deinit(m: Message, gpa: std.mem.Allocator) void {
        gpa.free(m.data);
    }
};

const guid = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const max_message = 8 * 1024 * 1024;

pub const Ws = struct {
    io: std.Io,
    conn: transport.Conn,
    rbuf: [64 * 1024]u8 = undefined,
    rstart: usize = 0,
    rend: usize = 0,
    wlock: std.Io.Mutex = .init,
    prng: std.Random.DefaultPrng,
    closed: std.atomic.Value(bool) = .init(false),

    pub fn connect(gpa: std.mem.Allocator, io: std.Io, ep: transport.Endpoint, path: []const u8, subprotocol: []const u8) !*Ws {
        const conn = try ep.dial(io);
        errdefer conn.close();
        var seed: [8]u8 = undefined;
        io.random(&seed);
        const self = try gpa.create(Ws);
        errdefer gpa.destroy(self);
        self.* = .{ .io = io, .conn = conn, .prng = .init(std.mem.readInt(u64, &seed, .little)) };
        try self.handshake(path, subprotocol);
        return self;
    }

    fn handshake(self: *Ws, path: []const u8, subprotocol: []const u8) !void {
        var raw_key: [16]u8 = undefined;
        self.io.random(&raw_key);
        var key: [24]u8 = undefined;
        _ = std.base64.standard.Encoder.encode(&key, &raw_key);

        var req_buf: [512]u8 = undefined;
        const req = try std.fmt.bufPrint(&req_buf, "GET {s} HTTP/1.1\r\nHost: rex\r\nUpgrade: websocket\r\n" ++
            "Connection: Upgrade\r\nSec-WebSocket-Key: {s}\r\nSec-WebSocket-Version: 13\r\n" ++
            "Sec-WebSocket-Protocol: {s}\r\nUser-Agent: rexa\r\n\r\n", .{ path, &key, subprotocol });
        try self.conn.writeAll(req);

        // Read the response head.
        while (std.mem.indexOf(u8, self.rbuf[0..self.rend], "\r\n\r\n") == null) {
            if (self.rend == self.rbuf.len) return error.HandshakeTooLarge;
            const n = try self.conn.read(self.rbuf[self.rend..]);
            if (n == 0) return error.HandshakeClosed;
            self.rend += n;
        }
        const head_end = std.mem.indexOf(u8, self.rbuf[0..self.rend], "\r\n\r\n").? + 4;
        const head = self.rbuf[0..head_end];
        if (!std.mem.startsWith(u8, head, "HTTP/1.1 101")) return error.HandshakeRejected;

        var sha = std.crypto.hash.Sha1.init(.{});
        sha.update(&key);
        sha.update(guid);
        var digest: [20]u8 = undefined;
        sha.final(&digest);
        var accept: [28]u8 = undefined;
        _ = std.base64.standard.Encoder.encode(&accept, &digest);

        var got_accept = false;
        var got_proto = false;
        var lines = std.mem.splitSequence(u8, head, "\r\n");
        while (lines.next()) |line| {
            const colon = std.mem.indexOfScalar(u8, line, ':') orelse continue;
            const name = std.mem.trim(u8, line[0..colon], " ");
            const value = std.mem.trim(u8, line[colon + 1 ..], " ");
            if (std.ascii.eqlIgnoreCase(name, "sec-websocket-accept")) got_accept = std.mem.eql(u8, value, &accept);
            if (std.ascii.eqlIgnoreCase(name, "sec-websocket-protocol")) got_proto = std.mem.eql(u8, value, subprotocol);
        }
        if (!got_accept) return error.HandshakeBadAccept;
        if (!got_proto) return error.HandshakeBadProtocol;
        self.rstart = head_end;
    }

    fn fill(self: *Ws, need: usize) !void {
        while (self.rend - self.rstart < need) {
            if (self.rstart > 0 and self.rbuf.len - self.rstart < need) {
                std.mem.copyForwards(u8, self.rbuf[0 .. self.rend - self.rstart], self.rbuf[self.rstart..self.rend]);
                self.rend -= self.rstart;
                self.rstart = 0;
            }
            if (self.rend == self.rbuf.len) {
                std.mem.copyForwards(u8, self.rbuf[0 .. self.rend - self.rstart], self.rbuf[self.rstart..self.rend]);
                self.rend -= self.rstart;
                self.rstart = 0;
            }
            const n = try self.conn.read(self.rbuf[self.rend..]);
            if (n == 0) return error.ConnectionClosed;
            self.rend += n;
        }
    }

    /// Copies `out.len` payload bytes, reading through the buffer.
    fn readPayload(self: *Ws, out: []u8) !void {
        var off: usize = 0;
        while (off < out.len) {
            if (self.rend == self.rstart) {
                self.rstart = 0;
                self.rend = 0;
                const n = try self.conn.read(self.rbuf[0..]);
                if (n == 0) return error.ConnectionClosed;
                self.rend = n;
            }
            const take = @min(out.len - off, self.rend - self.rstart);
            @memcpy(out[off..][0..take], self.rbuf[self.rstart..][0..take]);
            self.rstart += take;
            off += take;
        }
    }

    /// Next complete data or close message. Pings are answered here.
    pub fn readMessage(self: *Ws, gpa: std.mem.Allocator) !Message {
        var acc: std.ArrayList(u8) = .empty;
        errdefer acc.deinit(gpa);
        var msg_op: ?Opcode = null;
        while (true) {
            try self.fill(2);
            const b0 = self.rbuf[self.rstart];
            const b1 = self.rbuf[self.rstart + 1];
            const fin = b0 & 0x80 != 0;
            const op: Opcode = @enumFromInt(@as(u4, @truncate(b0 & 0x0f)));
            const masked = b1 & 0x80 != 0;
            var len: u64 = b1 & 0x7f;
            var hdr: usize = 2;
            if (len == 126) {
                try self.fill(4);
                len = std.mem.readInt(u16, self.rbuf[self.rstart + 2 ..][0..2], .big);
                hdr = 4;
            } else if (len == 127) {
                try self.fill(10);
                len = std.mem.readInt(u64, self.rbuf[self.rstart + 2 ..][0..8], .big);
                hdr = 10;
            }
            if (masked) return error.ProtocolError; // servers never mask
            if (len > max_message) return error.MessageTooLarge;
            self.rstart += hdr;
            const plen: usize = @intCast(len);

            switch (op) {
                .ping, .pong, .close => {
                    var small: [125]u8 = undefined;
                    if (plen > small.len) return error.ProtocolError;
                    try self.readPayload(small[0..plen]);
                    if (op == .ping) {
                        self.send(.pong, small[0..plen]) catch {};
                        continue;
                    }
                    if (op == .pong) continue;
                    const code: u16 = if (plen >= 2) std.mem.readInt(u16, small[0..2], .big) else 1005;
                    const reason = try gpa.dupe(u8, if (plen > 2) small[2..plen] else "");
                    acc.deinit(gpa);
                    return .{ .op = .close, .data = reason, .close_code = code };
                },
                .text, .binary => {
                    if (msg_op != null) return error.ProtocolError;
                    msg_op = op;
                },
                .continuation => if (msg_op == null) return error.ProtocolError,
                else => return error.ProtocolError,
            }
            const start = acc.items.len;
            try acc.resize(gpa, start + plen);
            try self.readPayload(acc.items[start..]);
            if (fin) return .{ .op = msg_op.?, .data = try acc.toOwnedSlice(gpa) };
        }
    }

    pub fn send(self: *Ws, op: Opcode, payload: []const u8) !void {
        if (self.closed.load(.acquire) and op != .close) return error.ConnectionClosed;
        self.wlock.lockUncancelable(self.io);
        defer self.wlock.unlock(self.io);
        var hdr: [14]u8 = undefined;
        hdr[0] = 0x80 | @as(u8, @intFromEnum(op));
        var n: usize = 2;
        if (payload.len < 126) {
            hdr[1] = 0x80 | @as(u8, @intCast(payload.len));
        } else if (payload.len <= 0xffff) {
            hdr[1] = 0x80 | 126;
            std.mem.writeInt(u16, hdr[2..4], @intCast(payload.len), .big);
            n = 4;
        } else {
            hdr[1] = 0x80 | 127;
            std.mem.writeInt(u64, hdr[2..10], payload.len, .big);
            n = 10;
        }
        var mask: [4]u8 = undefined;
        self.prng.random().bytes(&mask);
        @memcpy(hdr[n..][0..4], &mask);
        n += 4;
        try self.conn.writeAll(hdr[0..n]);
        var chunk: [4096]u8 = undefined;
        var off: usize = 0;
        while (off < payload.len) {
            const take = @min(chunk.len, payload.len - off);
            for (0..take) |i| chunk[i] = payload[off + i] ^ mask[(off + i) & 3];
            try self.conn.writeAll(chunk[0..take]);
            off += take;
        }
    }

    /// Sends a close frame (best effort). The reader sees the server's close.
    pub fn sendClose(self: *Ws, code: u16) void {
        var p: [2]u8 = undefined;
        std.mem.writeInt(u16, &p, code, .big);
        self.send(.close, &p) catch {};
        self.closed.store(true, .release);
    }

    /// Unblocks a thread stuck in readMessage.
    pub fn shutdown(self: *Ws) void {
        self.closed.store(true, .release);
        self.conn.shutdown();
    }

    pub fn destroy(self: *Ws, gpa: std.mem.Allocator) void {
        self.conn.close();
        gpa.destroy(self);
    }
};
