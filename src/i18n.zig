//! English / Chinese. Every string the user sees goes through `t(en, zh)`; the language
//! is chosen in Settings (and saved), or from $LANG the first time.

const std = @import("std");

pub const Lang = enum { en, zh };

/// The UI language. Read by the UI thread (and background jobs writing a notice).
pub var lang: Lang = .en;

pub fn t(en: []const u8, zh: []const u8) []const u8 {
    return if (lang == .zh) zh else en;
}

pub fn isZh() bool {
    return lang == .zh;
}

/// The language $LC_ALL / $LC_MESSAGES / $LANG asks for.
pub fn fromEnv(env: *const std.process.Environ.Map) Lang {
    for ([_][]const u8{ "LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE" }) |name| {
        const v = env.get(name) orelse continue;
        if (v.len == 0) continue;
        return if (std.ascii.startsWithIgnoreCase(v, "zh")) .zh else .en;
    }
    return .en;
}

pub fn parse(s: []const u8) ?Lang {
    if (std.mem.eql(u8, s, "zh")) return .zh;
    if (std.mem.eql(u8, s, "en")) return .en;
    return null;
}

/// allocPrint in the current language ("" when out of memory).
pub fn fmt(a: std.mem.Allocator, comptime en: []const u8, comptime zh_fmt: []const u8, args: anytype) []const u8 {
    const r = if (lang == .zh) std.fmt.allocPrint(a, zh_fmt, args) else std.fmt.allocPrint(a, en, args);
    return r catch "";
}

/// bufPrint in the current language (empty when it doesn't fit).
pub fn bufFmt(buf: []u8, comptime en: []const u8, comptime zh_fmt: []const u8, args: anytype) []const u8 {
    const r = if (lang == .zh) std.fmt.bufPrint(buf, zh_fmt, args) else std.fmt.bufPrint(buf, en, args);
    return r catch buf[0..0];
}
