//! rexa's settings, kept in $XDG_CONFIG_HOME/rexa/config.json (~/.config/rexa/...):
//! the language and the saved machines (a nickname and an address each).
//!
//!   {"language":"zh","servers":[{"name":"MacBook","endpoint":"ssh://me@mac.local"}]}

const std = @import("std");
const i18n = @import("i18n.zig");

pub const Server = struct { name: []const u8, endpoint: []const u8 };

pub const Config = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    /// strings live here; edits leak a little until exit, which is fine for a few names
    arena: std.heap.ArenaAllocator,
    path: []const u8 = "",
    lang: i18n.Lang = .en,
    servers: std.ArrayList(Server) = .empty,
    /// the last save failed: why
    save_error: ?[]const u8 = null,

    const File = struct {
        language: ?[]const u8 = null,
        servers: []const Server = &.{},
    };

    pub fn load(gpa: std.mem.Allocator, io: std.Io, env: *const std.process.Environ.Map) Config {
        var c: Config = .{ .gpa = gpa, .io = io, .arena = .init(gpa) };
        const a = c.arena.allocator();
        c.lang = i18n.fromEnv(env);
        if (env.get("XDG_CONFIG_HOME")) |x| {
            if (x.len > 0) c.path = std.fmt.allocPrint(a, "{s}/rexa/config.json", .{x}) catch "";
        }
        if (c.path.len == 0) if (env.get("HOME")) |h| {
            c.path = std.fmt.allocPrint(a, "{s}/.config/rexa/config.json", .{h}) catch "";
        };
        if (c.path.len == 0) return c;
        const bytes = std.Io.Dir.cwd().readFileAlloc(io, c.path, a, .limited(1024 * 1024)) catch return c;
        const parsed = std.json.parseFromSliceLeaky(File, a, bytes, .{ .ignore_unknown_fields = true, .allocate = .alloc_always }) catch return c;
        if (parsed.language) |l| c.lang = i18n.parse(l) orelse c.lang;
        for (parsed.servers) |s| {
            if (s.endpoint.len == 0) continue;
            c.servers.append(gpa, s) catch {};
        }
        return c;
    }

    pub fn deinit(self: *Config) void {
        self.servers.deinit(self.gpa);
        self.arena.deinit();
    }

    pub fn save(self: *Config) bool {
        self.save_error = null;
        if (self.path.len == 0) {
            self.save_error = "no $HOME";
            return false;
        }
        const out: File = .{ .language = @tagName(self.lang), .servers = self.servers.items };
        const bytes = std.json.Stringify.valueAlloc(self.gpa, out, .{ .whitespace = .indent_2 }) catch return false;
        defer self.gpa.free(bytes);
        const dir = std.fs.path.dirname(self.path) orelse ".";
        const cwd = std.Io.Dir.cwd();
        cwd.createDirPath(self.io, dir) catch |e| {
            self.save_error = @errorName(e);
            return false;
        };
        var tmp_buf: [4096]u8 = undefined;
        const tmp = std.fmt.bufPrint(&tmp_buf, "{s}.tmp", .{self.path}) catch return false;
        cwd.writeFile(self.io, .{ .sub_path = tmp, .data = bytes }) catch |e| {
            self.save_error = @errorName(e);
            return false;
        };
        cwd.rename(tmp, cwd, self.path, self.io) catch |e| {
            self.save_error = @errorName(e);
            return false;
        };
        return true;
    }

    pub fn find(self: *const Config, endpoint: []const u8) ?usize {
        for (self.servers.items, 0..) |s, i| if (std.mem.eql(u8, s.endpoint, endpoint)) return i;
        return null;
    }

    pub fn add(self: *Config, name: []const u8, endpoint: []const u8) void {
        const a = self.arena.allocator();
        if (self.find(endpoint)) |i| {
            self.servers.items[i].name = a.dupe(u8, name) catch return;
        } else {
            self.servers.append(self.gpa, .{ .name = a.dupe(u8, name) catch return, .endpoint = a.dupe(u8, endpoint) catch return }) catch return;
        }
        _ = self.save();
    }

    pub fn rename(self: *Config, i: usize, name: []const u8) void {
        if (i >= self.servers.items.len) return;
        self.servers.items[i].name = self.arena.allocator().dupe(u8, name) catch return;
        _ = self.save();
    }

    pub fn setEndpoint(self: *Config, i: usize, endpoint: []const u8) void {
        if (i >= self.servers.items.len) return;
        self.servers.items[i].endpoint = self.arena.allocator().dupe(u8, endpoint) catch return;
        _ = self.save();
    }

    pub fn remove(self: *Config, i: usize) void {
        if (i >= self.servers.items.len) return;
        _ = self.servers.orderedRemove(i);
        _ = self.save();
    }

    pub fn setLang(self: *Config, l: i18n.Lang) void {
        self.lang = l;
        i18n.lang = l;
        _ = self.save();
    }
};

/// A short default nickname for an address: the host part of ssh://user@host:port,
/// or the address itself.
pub fn defaultName(endpoint: []const u8) []const u8 {
    var s = endpoint;
    if (std.mem.startsWith(u8, s, "unix://")) return std.fs.path.basename(s);
    if (std.mem.indexOf(u8, s, "://")) |i| s = s[i + 3 ..];
    if (std.mem.lastIndexOfScalar(u8, s, '@')) |i| s = s[i + 1 ..];
    if (std.mem.indexOfScalar(u8, s, ':')) |i| s = s[0..i];
    if (std.mem.indexOfScalar(u8, s, '/')) |i| {
        if (i > 0) s = s[0..i];
    }
    if (std.mem.endsWith(u8, s, ".local")) s = s[0 .. s.len - ".local".len];
    return if (s.len > 0) s else endpoint;
}

test "default names" {
    try std.testing.expectEqualStrings("mac", defaultName("ssh://me@mac.local"));
    try std.testing.expectEqualStrings("box", defaultName("ssh://box:2222"));
    try std.testing.expectEqualStrings("studio", defaultName("studio"));
}

test "save and load" {
    const io = std.testing.io;
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    var buf: [512]u8 = undefined;
    const base = try tmp.dir.realPath(io, &buf);
    var env: std.process.Environ.Map = .init(std.testing.allocator);
    defer env.deinit();
    try env.put("XDG_CONFIG_HOME", buf[0..base]);
    try env.put("LANG", "zh_CN.UTF-8");
    {
        var c = Config.load(std.testing.allocator, io, &env);
        defer c.deinit();
        try std.testing.expectEqual(i18n.Lang.zh, c.lang);
        c.add("Mac", "ssh://me@mac.local");
        c.lang = .en;
        try std.testing.expect(c.save());
    }
    var c = Config.load(std.testing.allocator, io, &env);
    defer c.deinit();
    try std.testing.expectEqual(i18n.Lang.en, c.lang);
    try std.testing.expectEqual(@as(usize, 1), c.servers.items.len);
    try std.testing.expectEqualStrings("Mac", c.servers.items[0].name);
}
