//! vaxis key events -> Rex `client.key` key objects. The server runs Lua bindings and
//! then encodes the key for the pane's current keyboard mode (legacy, kitty, ...).

const std = @import("std");
const vaxis = @import("vaxis");
const Key = vaxis.Key;
const control = @import("control.zig");

/// W3C KeyboardEvent code in snake_case, from the key's base (US layout) codepoint.
pub fn physicalName(key: Key) []const u8 {
    const cp = key.base_layout_codepoint orelse key.codepoint;
    return switch (cp) {
        'a'...'z' => letters[cp - 'a'],
        'A'...'Z' => letters[cp - 'A'],
        '0'...'9' => digits[cp - '0'],
        '`', '~' => "backquote",
        '\\', '|' => "backslash",
        '[', '{' => "bracket_left",
        ']', '}' => "bracket_right",
        ',', '<' => "comma",
        '=', '+' => "equal",
        '-', '_' => "minus",
        '.', '>' => "period",
        '\'', '"' => "quote",
        ';', ':' => "semicolon",
        '/', '?' => "slash",
        '!' => "digit_1",
        '@' => "digit_2",
        '#' => "digit_3",
        '$' => "digit_4",
        '%' => "digit_5",
        '^' => "digit_6",
        '&' => "digit_7",
        '*' => "digit_8",
        '(' => "digit_9",
        ')' => "digit_0",
        Key.space => "space",
        Key.enter, '\n' => "enter",
        Key.tab => "tab",
        Key.backspace, 0x08 => "backspace",
        Key.escape => "escape",
        Key.insert => "insert",
        Key.delete => "delete",
        Key.left => "arrow_left",
        Key.right => "arrow_right",
        Key.up => "arrow_up",
        Key.down => "arrow_down",
        Key.page_up => "page_up",
        Key.page_down => "page_down",
        Key.home => "home",
        Key.end => "end",
        Key.caps_lock => "caps_lock",
        Key.scroll_lock => "scroll_lock",
        Key.num_lock => "num_lock",
        Key.print_screen => "print_screen",
        Key.pause => "pause",
        Key.menu => "context_menu",
        Key.f1...Key.f25 => fkeys[cp - Key.f1],
        Key.kp_0...Key.kp_9 => numpad_digits[cp - Key.kp_0],
        Key.kp_decimal => "numpad_decimal",
        Key.kp_divide => "numpad_divide",
        Key.kp_multiply => "numpad_multiply",
        Key.kp_subtract => "numpad_subtract",
        Key.kp_add => "numpad_add",
        Key.kp_enter => "numpad_enter",
        Key.kp_equal => "numpad_equal",
        Key.kp_separator => "numpad_separator",
        Key.kp_left => "numpad_left",
        Key.kp_right => "numpad_right",
        Key.kp_up => "numpad_up",
        Key.kp_down => "numpad_down",
        Key.kp_page_up => "numpad_page_up",
        Key.kp_page_down => "numpad_page_down",
        Key.kp_home => "numpad_home",
        Key.kp_end => "numpad_end",
        Key.kp_insert => "numpad_insert",
        Key.kp_delete => "numpad_delete",
        else => "unidentified",
    };
}

const letters = blk: {
    var out: [26][]const u8 = undefined;
    for (0..26) |i| out[i] = "key_" ++ [_]u8{'a' + i};
    break :blk out;
};
const digits = blk: {
    var out: [10][]const u8 = undefined;
    for (0..10) |i| out[i] = "digit_" ++ [_]u8{'0' + i};
    break :blk out;
};
const numpad_digits = blk: {
    var out: [10][]const u8 = undefined;
    for (0..10) |i| out[i] = "numpad_" ++ [_]u8{'0' + i};
    break :blk out;
};
const fkeys = blk: {
    var out: [25][]const u8 = undefined;
    for (0..25) |i| out[i] = std.fmt.comptimePrint("f{d}", .{i + 1});
    break :blk out;
};

pub fn modsString(buf: []u8, m: Key.Modifiers) []const u8 {
    var w: std.Io.Writer = .fixed(buf);
    var first = true;
    inline for (.{ .{ m.shift, "shift" }, .{ m.ctrl, "ctrl" }, .{ m.alt, "alt" }, .{ m.super, "super" }, .{ m.caps_lock, "caps_lock" }, .{ m.num_lock, "num_lock" } }) |pair| {
        if (pair[0]) {
            if (!first) w.writeByte('+') catch {};
            w.writeAll(pair[1]) catch {};
            first = false;
        }
    }
    return w.buffered();
}

fn isText(cp: u21) bool {
    return cp >= 0x20 and cp != 0x7f and cp < Key.insert;
}

/// The text the key typed, if any (into buf).
pub fn keyText(buf: []u8, key: Key) []const u8 {
    if (key.mods.ctrl or key.mods.alt or key.mods.super) {
        // Terminals still report text for e.g. ctrl+shift? Ignore: bindings use codepoints.
        if (key.text) |t| if (!key.mods.ctrl and !key.mods.super) return t;
        return "";
    }
    if (key.text) |t| return t;
    const cp = if (key.mods.shift) key.shifted_codepoint orelse key.codepoint else key.codepoint;
    if (!isText(cp)) return "";
    const n = std.unicode.utf8Encode(cp, buf) catch return "";
    return buf[0..n];
}

pub fn unshiftedCodepoint(key: Key) u21 {
    const cp = key.base_layout_codepoint orelse key.codepoint;
    if (cp >= 'A' and cp <= 'Z') return cp + 32;
    if (cp == Key.enter) return '\r';
    if (cp == Key.tab) return '\t';
    if (cp == Key.backspace) return 0x7f;
    if (cp == Key.escape) return 0x1b;
    if (cp == Key.space) return ' ';
    return if (isText(cp)) cp else 0;
}

/// Writes the `key` object of a client.key request.
pub fn writeKeyJson(w: *std.Io.Writer, key: Key, action: []const u8) !void {
    var mb: [96]u8 = undefined;
    var tb: [16]u8 = undefined;
    const mods = modsString(&mb, key.mods);
    const text = keyText(&tb, key);
    var cb: [96]u8 = undefined;
    const consumed = if (key.mods.shift and text.len > 0) "shift" else "";
    _ = &cb;
    try w.print(
        \\{{"key":"{s}","mods":"{s}","consumed_mods":"{s}","text":{f},"unshifted_codepoint":{d},"composing":false,"action":"{s}"}}
    , .{ physicalName(key), mods, consumed, control.q(text), unshiftedCodepoint(key), action });
}

test "physical names" {
    try std.testing.expectEqualStrings("key_b", physicalName(.{ .codepoint = 'b', .mods = .{ .ctrl = true } }));
    try std.testing.expectEqualStrings("digit_1", physicalName(.{ .codepoint = '!', .mods = .{ .shift = true } }));
    try std.testing.expectEqualStrings("arrow_up", physicalName(.{ .codepoint = Key.up }));
    try std.testing.expectEqualStrings("f12", physicalName(.{ .codepoint = Key.f12 }));
    var b: [32]u8 = undefined;
    try std.testing.expectEqualStrings("shift+ctrl", modsString(&b, .{ .shift = true, .ctrl = true }));
}

test "key json" {
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeKeyJson(&w, .{ .codepoint = 'a', .text = "A", .mods = .{ .shift = true } }, "press");
    try std.testing.expectEqualStrings(
        \\{"key":"key_a","mods":"shift","consumed_mods":"shift","text":"A","unshifted_codepoint":97,"composing":false,"action":"press"}
    , w.buffered());
}
