//! Data connections (`GET /api/data`): binary HELLO/WELCOME/RESUME handshake, then a
//! one-way stream (terminal output) or, for uploads, client-to-server bytes.

const std = @import("std");
const ws = @import("ws.zig");
const transport = @import("transport.zig");

pub const terminal_v1 = "rex.data.terminal.v1";
pub const bytes_v1 = "rex.data.bytes.v1";

pub const Item = union(enum) {
    bytes: []u8,
    closed: struct { code: u16 },
};

pub const DataConn = struct {
    sock: *ws.Ws,
    nonce: u64 = 0,
    token: [16]u8 = undefined,
    resume_ms: u32 = 0,
    received: u64 = 0,

    pub fn open(gpa: std.mem.Allocator, io: std.Io, ep: transport.Endpoint, ticket_b64: []const u8, subprotocol: []const u8) !DataConn {
        var ticket: [16]u8 = undefined;
        const n = std.base64.url_safe_no_pad.Decoder.calcSizeForSlice(ticket_b64) catch return error.BadTicket;
        if (n != 16) return error.BadTicket;
        std.base64.url_safe_no_pad.Decoder.decode(&ticket, ticket_b64) catch return error.BadTicket;
        var hello: [17]u8 = undefined;
        hello[0] = 0x01;
        @memcpy(hello[1..], &ticket);
        return handshake(gpa, io, ep, subprotocol, &hello);
    }

    /// Continues a dropped stream: RESUME with the latest token and byte count.
    pub fn resumeStream(gpa: std.mem.Allocator, io: std.Io, ep: transport.Endpoint, subprotocol: []const u8, token: [16]u8, received: u64) !DataConn {
        var msg: [25]u8 = undefined;
        msg[0] = 0x02;
        @memcpy(msg[1..17], &token);
        std.mem.writeInt(u64, msg[17..25], received, .little);
        var dc = try handshake(gpa, io, ep, subprotocol, &msg);
        dc.received = received;
        return dc;
    }

    fn handshake(gpa: std.mem.Allocator, io: std.Io, ep: transport.Endpoint, subprotocol: []const u8, first: []const u8) !DataConn {
        const sock = try ws.Ws.connect(gpa, io, ep, "/api/data", subprotocol);
        errdefer sock.destroy(gpa);
        try sock.send(.binary, first);
        const msg = try sock.readMessage(gpa);
        defer msg.deinit(gpa);
        if (msg.op == .close) return error.DataRejected;
        if (msg.data.len == 2 and msg.data[0] == 0x82) {
            return switch (msg.data[1]) {
                0x01 => error.TicketInvalid,
                0x02 => error.AlreadyConnected,
                0x03 => error.StreamUnavailable,
                0x07 => error.WrongSubprotocol,
                else => error.DataRejected,
            };
        }
        if (msg.data.len != 32 or msg.data[0] != 0x81) return error.BadWelcome;
        var dc: DataConn = .{ .sock = sock };
        @memcpy(&dc.token, msg.data[1..17]);
        dc.resume_ms = std.mem.readInt(u32, msg.data[20..24], .little);
        dc.nonce = std.mem.readInt(u64, msg.data[24..32], .little);
        return dc;
    }

    /// Next chunk of the stream (caller frees bytes) or the close.
    pub fn next(self: *DataConn, gpa: std.mem.Allocator) !Item {
        while (true) {
            const msg = self.sock.readMessage(gpa) catch |e| switch (e) {
                error.ConnectionClosed, error.ReadFailed => return .{ .closed = .{ .code = 1006 } },
                else => return e,
            };
            switch (msg.op) {
                .binary => {
                    self.received += msg.data.len;
                    return .{ .bytes = msg.data };
                },
                .close => {
                    msg.deinit(gpa);
                    return .{ .closed = .{ .code = msg.close_code } };
                },
                else => msg.deinit(gpa),
            }
        }
    }

    pub fn send(self: *DataConn, bytes: []const u8) !void {
        try self.sock.send(.binary, bytes);
    }

    /// Closes the stream for good (status 1000 ends it on the server).
    pub fn close(self: *DataConn, gpa: std.mem.Allocator) void {
        self.sock.sendClose(1000);
        self.sock.shutdown();
        self.sock.destroy(gpa);
    }

    /// Unblocks a reader in next() from another thread.
    pub fn shutdown(self: *DataConn) void {
        self.sock.shutdown();
    }
};
