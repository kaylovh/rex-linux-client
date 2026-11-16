//! Client-side bookkeeping that outlives a single session view: the session list and
//! what programs report about themselves (OSC 7501 program status), per block.

const std = @import("std");

pub const State = enum(u8) {
    idle = 0,
    done = 1,
    working = 2,
    err = 3,
    blocked = 4,

    pub fn parse(s: []const u8) State {
        if (std.mem.eql(u8, s, "working")) return .working;
        if (std.mem.eql(u8, s, "blocked")) return .blocked;
        if (std.mem.eql(u8, s, "done")) return .done;
        if (std.mem.eql(u8, s, "error")) return .err;
        return .idle;
    }

    /// Higher wins when several records or blocks are summarized.
    pub fn max(a: State, b: State) State {
        return if (@intFromEnum(b) > @intFromEnum(a)) b else a;
    }
};

pub const Session = struct {
    id: []u8,
    label: []u8,
};

const BlockStatus = struct {
    session: []u8,
    /// record id ("" for the root record) -> state
    records: std.StringHashMapUnmanaged(State) = .empty,

    fn summary(self: *const BlockStatus) State {
        var s: State = .idle;
        var it = self.records.valueIterator();
        while (it.next()) |v| s = s.max(v.*);
        return s;
    }
};

pub const Model = struct {
    gpa: std.mem.Allocator,
    sessions: std.ArrayList(Session) = .empty,
    blocks: std.StringHashMapUnmanaged(BlockStatus) = .empty,

    pub fn deinit(self: *Model) void {
        for (self.sessions.items) |s| {
            self.gpa.free(s.id);
            self.gpa.free(s.label);
        }
        self.sessions.deinit(self.gpa);
        var it = self.blocks.iterator();
        while (it.next()) |e| self.freeBlock(e.key_ptr.*, e.value_ptr);
        self.blocks.deinit(self.gpa);
    }

    fn freeBlock(self: *Model, key: []const u8, b: *BlockStatus) void {
        var rit = b.records.keyIterator();
        while (rit.next()) |k| self.gpa.free(k.*);
        b.records.deinit(self.gpa);
        self.gpa.free(b.session);
        self.gpa.free(key);
    }

    /// Forgets every session and status (before reloading from a new server).
    pub fn clear(self: *Model) void {
        const gpa = self.gpa;
        self.deinit();
        self.* = .{ .gpa = gpa };
    }

    pub fn findSession(self: *Model, id: []const u8) ?usize {
        for (self.sessions.items, 0..) |s, i| if (std.mem.eql(u8, s.id, id)) return i;
        return null;
    }

    pub fn putSession(self: *Model, id: []const u8, label: []const u8) !void {
        if (self.findSession(id)) |i| {
            const s = &self.sessions.items[i];
            if (std.mem.eql(u8, s.label, label)) return;
            self.gpa.free(s.label);
            s.label = try self.gpa.dupe(u8, label);
            return;
        }
        try self.sessions.append(self.gpa, .{ .id = try self.gpa.dupe(u8, id), .label = try self.gpa.dupe(u8, label) });
    }

    pub fn removeSession(self: *Model, id: []const u8) void {
        if (self.findSession(id)) |i| {
            const s = self.sessions.orderedRemove(i);
            self.gpa.free(s.id);
            self.gpa.free(s.label);
        }
        // drop its blocks' statuses
        var dead: std.ArrayList([]const u8) = .empty;
        defer dead.deinit(self.gpa);
        var it = self.blocks.iterator();
        while (it.next()) |e| if (std.mem.eql(u8, e.value_ptr.session, id)) dead.append(self.gpa, e.key_ptr.*) catch {};
        for (dead.items) |k| self.removeBlock(k);
    }

    pub fn setStatus(self: *Model, session: []const u8, block: []const u8, record: []const u8, state: State) !void {
        const gop = try self.blocks.getOrPut(self.gpa, block);
        if (!gop.found_existing) {
            gop.key_ptr.* = try self.gpa.dupe(u8, block);
            gop.value_ptr.* = .{ .session = try self.gpa.dupe(u8, session) };
        }
        const r = try gop.value_ptr.records.getOrPut(self.gpa, record);
        if (!r.found_existing) r.key_ptr.* = try self.gpa.dupe(u8, record);
        r.value_ptr.* = state;
    }

    pub fn removeRecord(self: *Model, block: []const u8, record: []const u8) void {
        const b = self.blocks.getPtr(block) orelse return;
        if (b.records.fetchRemove(record)) |kv| self.gpa.free(kv.key);
    }

    pub fn removeBlock(self: *Model, block: []const u8) void {
        if (self.blocks.fetchRemove(block)) |kv| {
            var v = kv.value;
            self.freeBlock(kv.key, &v);
        }
    }

    pub fn blockState(self: *Model, block: []const u8) State {
        const b = self.blocks.getPtr(block) orelse return .idle;
        return b.summary();
    }

    pub fn sessionState(self: *Model, session: []const u8) State {
        var s: State = .idle;
        var it = self.blocks.valueIterator();
        while (it.next()) |b| if (std.mem.eql(u8, b.session, session)) {
            s = s.max(b.summary());
        };
        return s;
    }

    pub fn anyWorking(self: *Model) bool {
        var it = self.blocks.valueIterator();
        while (it.next()) |b| if (b.summary() == .working) return true;
        return false;
    }
};

test "status summary" {
    var m: Model = .{ .gpa = std.testing.allocator };
    defer m.deinit();
    try m.putSession("s1", "one");
    try m.putSession("s1", "uno");
    try std.testing.expectEqualStrings("uno", m.sessions.items[0].label);
    try m.setStatus("s1", "b1", "", .working);
    try m.setStatus("s1", "b1", "build/test", .blocked);
    try m.setStatus("s1", "b2", "", .done);
    try std.testing.expectEqual(State.blocked, m.blockState("b1"));
    try std.testing.expectEqual(State.blocked, m.sessionState("s1"));
    m.removeRecord("b1", "build/test");
    try std.testing.expectEqual(State.working, m.sessionState("s1"));
    m.removeSession("s1");
    try std.testing.expectEqual(State.idle, m.sessionState("s1"));
}
