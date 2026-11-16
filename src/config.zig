//! rexa's settings, kept in $XDG_CONFIG_HOME/rexa/config.json (~/.config/rexa/...):
//! the language and the saved machines (a nickname and an address each).
//!
//!   {"language":"zh","servers":[{"name":"MacBook","endpoint":"ssh://me@mac.local"}],
//!    "shown_hosts":[{"server":"ssh://me@mac.local","hosts":["studio"]}]}
//!
//! shown_hosts: which of a server's own `rex hosts` the machine list shows. A server
//! with no entry: all of them for this computer (""), none for a remote one.

const std = @import("std");
const i18n = @import("i18n.zig");

pub const Server = struct { name: []const u8, endpoint: []const u8 };
pub const Shown = struct { server: []const u8, hosts: []const []const u8 };

pub const Config = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    /// strings live here; edits leak a little until exit, which is fine for a few names
    arena: std.heap.ArenaAllocator,
    path: []const u8 = "",
    lang: i18n.Lang = .en,
    /// a plain drag selects text even when the program wants the mouse (Shift gives it the drag)
    drag_selects: bool = false,
    servers: std.ArrayList(Server) = .empty,
    shown: std.ArrayList(Shown) = .empty,
    /// the last save failed: why
    save_error: ?[]const u8 = null,

    const File = struct {
        language: ?[]const u8 = null,
        drag_selects: bool = false,
        servers: []const Server = &.{},
        shown_hosts: []const Shown = &.{},
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
        c.drag_selects = parsed.drag_selects;
        for (parsed.servers) |s| {
            if (s.endpoint.len == 0) continue;
            c.servers.append(gpa, s) catch {};
        }
        for (parsed.shown_hosts) |s| c.shown.append(gpa, s) catch {};
        return c;
    }

    pub fn deinit(self: *Config) void {
        self.servers.deinit(self.gpa);
        self.shown.deinit(self.gpa);
        self.arena.deinit();
    }

    pub fn save(self: *Config) bool {
        self.save_error = null;
        if (self.path.len == 0) {
            self.save_error = "no $HOME";
            return false;
        }
        const out: File = .{ .language = @tagName(self.lang), .drag_selects = self.drag_selects, .servers = self.servers.items, .shown_hosts = self.shown.items };
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

    /// The `rex hosts` labels chosen for `server`; null: never chosen (the default applies).
    pub fn shownHosts(self: *const Config, server: []const u8) ?[]const []const u8 {
        for (self.shown.items) |s| if (std.mem.eql(u8, s.server, server)) return s.hosts;
        return null;
    }

    /// Shows or hides one of `server`'s hosts. `visible`: the labels shown now (the
    /// default, when nothing was chosen yet).
    pub fn setHostShown(self: *Config, server: []const u8, label: []const u8, on: bool, visible: []const []const u8) void {
        const a = self.arena.allocator();
        var list: std.ArrayList([]const u8) = .empty;
        for (visible) |l| if (!std.mem.eql(u8, l, label)) list.append(a, a.dupe(u8, l) catch return) catch return;
        if (on) list.append(a, a.dupe(u8, label) catch return) catch return;
        for (self.shown.items) |*s| if (std.mem.eql(u8, s.server, server)) {
            s.hosts = list.items;
            _ = self.save();
            return;
        };
        self.shown.append(self.gpa, .{ .server = a.dupe(u8, server) catch return, .hosts = list.items }) catch return;
        _ = self.save();
    }

    pub fn setDragSelects(self: *Config, on: bool) void {
        self.drag_selects = on;
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
    // ssh://mac#unix:///x/p1.sock: that server, reached through mac
    if (std.mem.startsWith(u8, s, "ssh://")) if (std.mem.indexOfScalar(u8, s, '#')) |i| {
        if (i + 1 < s.len) return defaultName(s[i + 1 ..]);
    };
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
    try std.testing.expectEqualStrings("p1.sock", defaultName("ssh://me@mac#unix:///x/p1.sock"));
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
    try std.testing.expect(c.shownHosts("ssh://me@mac.local") == null);
    c.setHostShown("ssh://me@mac.local", "studio", true, &.{});
    c.setHostShown("ssh://me@mac.local", "P1", true, c.shownHosts("ssh://me@mac.local").?);
    c.setHostShown("ssh://me@mac.local", "studio", false, c.shownHosts("ssh://me@mac.local").?);
    var c2 = Config.load(std.testing.allocator, io, &env);
    defer c2.deinit();
    const shown = c2.shownHosts("ssh://me@mac.local").?;
    try std.testing.expectEqual(@as(usize, 1), shown.len);
    try std.testing.expectEqualStrings("P1", shown[0]);
}
