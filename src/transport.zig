//! Byte transport to a Rex server: a unix socket, or a command whose stdin/stdout is
//! bridged to the server (`ssh host rex __internal dial-stdio`). Every WebSocket gets
//! its own Conn. Blocking I/O; callers use one thread per reading direction.

const std = @import("std");
const linux = std.os.linux;

pub const Endpoint = union(enum) {
    unix: []const u8,
    /// argv of a stdio bridge command
    command: []const []const u8,

    /// unix:///path, /path, ssh://[user@]host[:port][#address], cmd:<argv words>
    /// (#address: a Rex server as that machine reaches it, e.g. one of its `rex hosts`)
    pub fn parse(arena: std.mem.Allocator, s: []const u8) !Endpoint {
        if (std.mem.startsWith(u8, s, "unix://")) return .{ .unix = s["unix://".len..] };
        if (std.mem.startsWith(u8, s, "/")) return .{ .unix = s };
        if (std.mem.startsWith(u8, s, "cmd:")) {
            var argv: std.ArrayList([]const u8) = .empty;
            var it = std.mem.tokenizeScalar(u8, s["cmd:".len..], ' ');
            while (it.next()) |w| try argv.append(arena, w);
            if (argv.items.len == 0) return error.BadEndpoint;
            return .{ .command = argv.items };
        }
        if (std.mem.startsWith(u8, s, "ssh://")) {
            var rest = std.mem.trimEnd(u8, s["ssh://".len..], "/");
            var hop: []const u8 = "";
            if (std.mem.indexOfScalar(u8, rest, '#')) |i| {
                hop = rest[i + 1 ..];
                rest = std.mem.trimEnd(u8, rest[0..i], "/");
            }
            if (rest.len == 0) return error.BadEndpoint;
            var argv: std.ArrayList([]const u8) = .empty;
            try argv.appendSlice(arena, &.{ "ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15" });
            if (std.mem.lastIndexOfScalar(u8, rest, ':')) |i| {
                const port = rest[i + 1 ..];
                if (port.len > 0 and std.mem.indexOfNone(u8, port, "0123456789") == null) {
                    try argv.appendSlice(arena, &.{ "-p", port });
                    rest = rest[0..i];
                }
            }
            try argv.append(arena, rest);
            // The remote shell runs this. A non-interactive ssh shell often lacks the PATH
            // set up in .zprofile/.bashrc (Homebrew on a Mac), so add the usual places.
            try argv.append(arena, if (hop.len == 0) remote_cmd else try std.fmt.allocPrint(arena, remote_path ++ "exec rex --server {s} __internal dial-stdio", .{try shellQuote(arena, hop)}));
            return .{ .command = argv.items };
        }
        if (s.len == 0) return error.BadEndpoint;
        // Anything else (a host label from `rex hosts`, a Tailscale name, host:port):
        // let the rex CLI reach it and bridge its stdio to us.
        const argv = try arena.dupe([]const u8, &.{ "rex", "--server", s, "__internal", "dial-stdio" });
        return .{ .command = argv };
    }

    /// Put in front of a remote command so a non-interactive ssh shell finds rex.
    pub const remote_path = "PATH=\"$PATH:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/Applications/Rex.app/Contents/MacOS:/Applications/Rex.app/Contents/Resources\"; export PATH; ";
    pub const remote_cmd = remote_path ++ "exec rex __internal dial-stdio";

    /// it's -> 'it'\''s', for the remote shell
    fn shellQuote(arena: std.mem.Allocator, s: []const u8) ![]const u8 {
        var out: std.ArrayList(u8) = .empty;
        try out.append(arena, '\'');
        for (s) |c| if (c == '\'') try out.appendSlice(arena, "'\\''") else try out.append(arena, c);
        try out.append(arena, '\'');
        return out.items;
    }

    pub fn defaultLocal(arena: std.mem.Allocator, env: *const std.process.Environ.Map) !Endpoint {
        if (env.get("REX_SERVER")) |s| {
            if (Endpoint.parse(arena, s)) |ep| return ep else |_| {}
        }
        const uid = linux.getuid();
        return .{ .unix = try std.fmt.allocPrint(arena, "/run/user/{d}/rex/server.sock", .{uid}) };
    }

    pub fn dial(self: Endpoint, io: std.Io) !Conn {
        switch (self) {
            .unix => |path| return dialUnix(path),
            .command => |argv| {
                const child = try std.process.spawn(io, .{
                    .argv = argv,
                    .stdin = .pipe,
                    .stdout = .pipe,
                    .stderr = .ignore,
                });
                return .{
                    .rfd = child.stdout.?.handle,
                    .wfd = child.stdin.?.handle,
                    .pid = child.id,
                };
            },
        }
    }
};

fn sysErr(rc: usize) ?linux.E {
    const e = linux.errno(rc);
    return if (e == .SUCCESS) null else e;
}

fn dialUnix(path: []const u8) !Conn {
    const rc = linux.socket(linux.AF.UNIX, linux.SOCK.STREAM | linux.SOCK.CLOEXEC, 0);
    if (sysErr(rc) != null) return error.SocketFailed;
    const fd: linux.fd_t = @intCast(rc);
    errdefer _ = linux.close(fd);
    var addr: linux.sockaddr.un = .{ .path = @splat(0) };
    if (path.len >= addr.path.len) return error.PathTooLong;
    @memcpy(addr.path[0..path.len], path);
    const crc = linux.connect(fd, &addr, @sizeOf(linux.sockaddr.un));
    if (sysErr(crc)) |e| {
        return switch (e) {
            .NOENT, .CONNREFUSED => error.ServerNotRunning,
            .ACCES, .PERM => error.AccessDenied,
            else => error.ConnectFailed,
        };
    }
    return .{ .rfd = fd, .wfd = fd, .pid = null };
}

pub const Conn = struct {
    rfd: linux.fd_t,
    wfd: linux.fd_t,
    pid: ?linux.pid_t,

    pub fn read(self: Conn, buf: []u8) !usize {
        while (true) {
            const rc = linux.read(self.rfd, buf.ptr, buf.len);
            if (sysErr(rc)) |e| {
                if (e == .INTR) continue;
                return error.ReadFailed;
            }
            return rc;
        }
    }

    pub fn writeAll(self: Conn, data: []const u8) !void {
        var off: usize = 0;
        while (off < data.len) {
            const rc = linux.write(self.wfd, data[off..].ptr, data.len - off);
            if (sysErr(rc)) |e| {
                if (e == .INTR) continue;
                return error.WriteFailed;
            }
            off += rc;
        }
    }

    /// Unblocks a reader thread without freeing the fds yet. A bridge command is
    /// killed outright: some ignore SIGTERM and would keep the pipe open.
    pub fn shutdown(self: Conn) void {
        if (self.pid) |pid| {
            _ = linux.kill(pid, .KILL);
        } else {
            _ = linux.shutdown(self.rfd, linux.SHUT.RDWR);
        }
    }

    /// Collects a dead bridge process so it doesn't linger as a zombie.
    pub fn reap(self: Conn) void {
        if (self.pid) |pid| {
            var status: i32 = 0;
            _ = linux.waitpid(pid, &status, linux.W.NOHANG);
        }
    }

    pub fn close(self: Conn) void {
        if (self.pid) |pid| {
            _ = linux.kill(pid, .KILL);
            var status: i32 = 0;
            _ = linux.waitpid(pid, &status, 0);
            _ = linux.close(self.rfd);
            _ = linux.close(self.wfd);
        } else {
            _ = linux.close(self.rfd);
        }
    }
};

test "parse endpoints" {
    var arena: std.heap.ArenaAllocator = .init(std.testing.allocator);
    defer arena.deinit();
    const a = arena.allocator();
    try std.testing.expectEqualStrings("/x.sock", (try Endpoint.parse(a, "unix:///x.sock")).unix);
    const ssh = (try Endpoint.parse(a, "ssh://me@box:2222")).command;
    try std.testing.expectEqualStrings("-p", ssh[8]);
    try std.testing.expectEqualStrings("2222", ssh[9]);
    try std.testing.expectEqualStrings("me@box", ssh[10]);
    const cmd = (try Endpoint.parse(a, "cmd:rex __internal dial-stdio")).command;
    try std.testing.expectEqual(@as(usize, 3), cmd.len);
    const label = (try Endpoint.parse(a, "studio")).command;
    try std.testing.expectEqualStrings("studio", label[2]);
    const hop = (try Endpoint.parse(a, "ssh://me@mac:22/#it's P1")).command;
    try std.testing.expectEqualStrings("me@mac", hop[10]);
    try std.testing.expect(std.mem.endsWith(u8, hop[11], "exec rex --server 'it'\\''s P1' __internal dial-stdio"));
}
