//! A Pane mirrors one Rex terminal block: a libghostty terminal fed by the block's data
//! stream (GHOSTSNP snapshot, then live VT), drawn into a vaxis window on demand.
//! A reader thread owns the stream; the UI thread draws under `lock`.

const std = @import("std");
const vaxis = @import("vaxis");
const gt = @import("gt");
const control = @import("control.zig");
const data = @import("data.zig");
const transport = @import("transport.zig");
const rexscan = @import("rexscan.zig");

pub const Status = enum(u8) { connecting, live, closed, failed };

pub const Notify = struct {
    ctx: *anyopaque,
    /// Called from the reader thread when the pane's picture changed.
    redraw: *const fn (ctx: *anyopaque) void,
};

pub const Pane = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    ctl: *control.Control,
    ep: transport.Endpoint,
    sid: []u8,
    bid: []u8,
    notify: Notify,

    lock: std.Io.Mutex = .init,
    term: gt.GhosttyTerminal = null,
    rstate: gt.GhosttyRenderState = null,
    row_iter: gt.GhosttyRenderStateRowIterator = null,
    row_cells: gt.GhosttyRenderStateRowCells = null,
    cols: u16 = 0,
    rows: u16 = 0,
    status: std.atomic.Value(Status) = .init(.connecting),
    exit_code: ?i64 = null,
    err_buf: [160]u8 = undefined,
    err_len: usize = 0,
    title_buf: [128]u8 = undefined,
    title_len: usize = 0,

    dc: ?data.DataConn = null,
    thread: ?std.Thread = null,
    stopping: std.atomic.Value(bool) = .init(false),
    search: gt.GhosttySearch = null,
    /// how many times the stream was resumed after a drop (diagnostics)
    resumes: u32 = 0,
    scanner: ?rexscan.Scanner = null,

    pub fn create(gpa: std.mem.Allocator, io: std.Io, ctl: *control.Control, ep: transport.Endpoint, sid: []const u8, bid: []const u8, notify: Notify) !*Pane {
        const p = try gpa.create(Pane);
        p.* = .{
            .gpa = gpa,
            .io = io,
            .ctl = ctl,
            .ep = ep,
            .sid = try gpa.dupe(u8, sid),
            .bid = try gpa.dupe(u8, bid),
            .notify = notify,
        };
        if (gt.ghostty_render_state_new(null, &p.rstate) != gt.GHOSTTY_SUCCESS) return error.GhosttyInit;
        if (gt.ghostty_render_state_row_iterator_new(null, &p.row_iter) != gt.GHOSTTY_SUCCESS) return error.GhosttyInit;
        if (gt.ghostty_render_state_row_cells_new(null, &p.row_cells) != gt.GHOSTTY_SUCCESS) return error.GhosttyInit;
        p.thread = try std.Thread.spawn(.{}, run, .{p});
        return p;
    }

    pub fn destroy(self: *Pane) void {
        self.stopping.store(true, .release);
        self.lock.lockUncancelable(self.io);
        if (self.dc) |*dc| dc.shutdown();
        self.lock.unlock(self.io);
        if (self.thread) |t| t.join();
        if (self.dc) |*dc| dc.close(self.gpa);
        if (self.scanner) |*s| s.deinit();
        if (self.search != null) gt.ghostty_search_free(self.search);
        if (self.row_cells != null) gt.ghostty_render_state_row_cells_free(self.row_cells);
        if (self.row_iter != null) gt.ghostty_render_state_row_iterator_free(self.row_iter);
        if (self.rstate != null) gt.ghostty_render_state_free(self.rstate);
        if (self.term != null) gt.ghostty_terminal_free(self.term);
        self.gpa.free(self.sid);
        self.gpa.free(self.bid);
        self.gpa.destroy(self);
    }

    // ---- scrollback (UI thread) -------------------------------------------------------

    pub const Scroll = struct { offset: u64, total: u64, len: u64 };

    pub fn scrollbar(self: *Pane) ?Scroll {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return null;
        var sb: gt.GhosttyTerminalScrollbar = undefined;
        if (gt.ghostty_terminal_get(self.term, gt.GHOSTTY_TERMINAL_DATA_SCROLLBAR, &sb) != gt.GHOSTTY_SUCCESS) return null;
        return .{ .offset = sb.offset, .total = sb.total, .len = sb.len };
    }

    /// True when the viewport isn't at the bottom (the user scrolled back).
    pub fn scrolledBack(self: *Pane) bool {
        const sb = self.scrollbar() orelse return false;
        return sb.offset + sb.len < sb.total;
    }

    pub fn scrollBy(self: *Pane, delta: isize) void {
        self.scroll(.{ .tag = gt.GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA, .value = .{ .delta = delta } });
    }

    pub fn scrollTo(self: *Pane, top: bool) void {
        self.scroll(.{ .tag = if (top) gt.GHOSTTY_TERMINAL_SCROLL_VIEWPORT_TOP else gt.GHOSTTY_TERMINAL_SCROLL_VIEWPORT_BOTTOM, .value = .{ .delta = 0 } });
    }

    pub fn scrollPrompt(self: *Pane, delta: isize) void {
        self.scroll(.{ .tag = gt.GHOSTTY_TERMINAL_SCROLL_VIEWPORT_DELTA_PROMPT, .value = .{ .delta_prompt = delta } });
    }

    fn scroll(self: *Pane, how: gt.GhosttyTerminalScrollViewport) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return;
        _ = gt.ghostty_terminal_scroll_viewport(self.term, how);
    }

    fn modeOn(self: *Pane, mode: u16) bool {
        var mc: gt.GhosttyTerminalModeConfig = .{ .mode = gt.ghostty_mode_new(mode, false), .value = false };
        if (gt.ghostty_terminal_get(self.term, gt.GHOSTTY_TERMINAL_DATA_MODE, &mc) != gt.GHOSTTY_SUCCESS) return false;
        return mc.value;
    }

    /// Whether wheel and clicks belong to the program (mouse tracking or alt screen)
    /// rather than to local scrollback.
    pub fn programWantsMouse(self: *Pane) bool {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return true;
        if (self.modeOn(1000) or self.modeOn(1002) or self.modeOn(1003) or self.modeOn(9)) return true;
        var screen: c_int = gt.GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        _ = gt.ghostty_terminal_get(self.term, gt.GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen);
        return screen != gt.GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    }

    // ---- search (UI thread) -----------------------------------------------------------

    pub const Match = struct { index: usize, total: usize };

    /// Starts (or changes) a search and selects the newest match.
    pub fn searchFor(self: *Pane, needle: []const u8) ?Match {
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            if (self.term == null) return null;
            if (self.search == null) {
                if (gt.ghostty_search_new(null, &self.search, self.term) != gt.GHOSTTY_SUCCESS) {
                    self.search = null;
                    return null;
                }
            }
            const s2: gt.GhosttyString = .{ .ptr = needle.ptr, .len = needle.len };
            if (gt.ghostty_search_set(self.search, gt.GHOSTTY_SEARCH_OPT_NEEDLE, &s2) != gt.GHOSTTY_SUCCESS) return null;
        }
        return self.searchStep(true);
    }

    /// Moves to the next match toward older (true) or newer output, scrolling to it
    /// and highlighting it.
    pub fn searchStep(self: *Pane, older: bool) ?Match {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.search == null or self.term == null) return null;
        const rc = if (older) gt.ghostty_search_select_next(self.search) else gt.ghostty_search_select_prev(self.search);
        if (rc != gt.GHOSTTY_SUCCESS) return .{ .index = 0, .total = 0 };
        var m: Match = .{ .index = 0, .total = 0 };
        var sel: gt.GhosttySelection = std.mem.zeroes(gt.GhosttySelection);
        _ = gt.ghostty_search_get(self.search, gt.GHOSTTY_SEARCH_DATA_SELECTED_INDEX, &m.index);
        _ = gt.ghostty_search_get(self.search, gt.GHOSTTY_SEARCH_DATA_TOTAL_MATCHES, &m.total);
        if (gt.ghostty_search_get(self.search, gt.GHOSTTY_SEARCH_DATA_SELECTED_MATCH, &sel) == gt.GHOSTTY_SUCCESS)
            _ = gt.ghostty_terminal_set(self.term, gt.GHOSTTY_TERMINAL_OPT_SELECTION, &sel);
        return m;
    }

    pub fn searchClear(self: *Pane) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.search != null) gt.ghostty_search_free(self.search);
        self.search = null;
        if (self.term != null) _ = gt.ghostty_terminal_set(self.term, gt.GHOSTTY_TERMINAL_OPT_SELECTION, null);
    }

    // ---- selection (UI thread) --------------------------------------------------------

    fn gridRef(self: *Pane, x: u16, y: u16) ?gt.GhosttyGridRef {
        var ref: gt.GhosttyGridRef = std.mem.zeroes(gt.GhosttyGridRef);
        var pt: gt.GhosttyPoint = std.mem.zeroes(gt.GhosttyPoint);
        pt.tag = gt.GHOSTTY_POINT_TAG_VIEWPORT;
        pt.value.coordinate = .{ .x = @min(x, self.cols -| 1), .y = @min(y, self.rows -| 1) };
        if (gt.ghostty_terminal_grid_ref(self.term, pt, &ref) != gt.GHOSTTY_SUCCESS) return null;
        return ref;
    }

    /// Selects from (x0,y0) to (x1,y1), viewport cells, in reading order.
    pub fn select(self: *Pane, x0: u16, y0: u16, x1: u16, y1: u16) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return;
        const a_first = y0 < y1 or (y0 == y1 and x0 <= x1);
        const sx = if (a_first) x0 else x1;
        const sy = if (a_first) y0 else y1;
        const ex = if (a_first) x1 else x0;
        const ey = if (a_first) y1 else y0;
        var sel: gt.GhosttySelection = std.mem.zeroes(gt.GhosttySelection);
        sel.start = self.gridRef(sx, sy) orelse return;
        sel.end = self.gridRef(ex, ey) orelse return;
        _ = gt.ghostty_terminal_set(self.term, gt.GHOSTTY_TERMINAL_OPT_SELECTION, &sel);
    }

    pub fn clearSelection(self: *Pane) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return;
        _ = gt.ghostty_terminal_set(self.term, gt.GHOSTTY_TERMINAL_OPT_SELECTION, null);
    }

    /// The selected text the way Ghostty copies it (unwrapped, trimmed). Caller frees.
    pub fn selectionText(self: *Pane, gpa: std.mem.Allocator) ?[]u8 {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return null;
        var sel: gt.GhosttySelection = undefined;
        if (gt.ghostty_terminal_get(self.term, gt.GHOSTTY_TERMINAL_DATA_SELECTION, &sel) != gt.GHOSTTY_SUCCESS) return null;
        var fmt: gt.GhosttyFormatter = null;
        if (gt.ghostty_formatter_terminal_new(null, &fmt, self.term) != gt.GHOSTTY_SUCCESS) return null;
        defer gt.ghostty_formatter_free(fmt);
        var on = true;
        _ = gt.ghostty_formatter_set(fmt, gt.GHOSTTY_FORMATTER_OPT_UNWRAP, &on);
        _ = gt.ghostty_formatter_set(fmt, gt.GHOSTTY_FORMATTER_OPT_TRIM, &on);
        _ = gt.ghostty_formatter_set(fmt, gt.GHOSTTY_FORMATTER_OPT_SELECTION, &sel);
        var out: [*c]u8 = null;
        var len: usize = 0;
        if (gt.ghostty_formatter_format_alloc(fmt, null, &out, &len) != gt.GHOSTTY_SUCCESS) return null;
        defer gt.ghostty_free(null, out, len);
        if (len == 0) return null;
        return gpa.dupe(u8, out[0..len]) catch null;
    }

    /// The terminal's title (OSC 0/2), copied; empty if none.
    pub fn title(self: *Pane) []const u8 {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return "";
        var t: gt.GhosttyString = undefined;
        if (gt.ghostty_terminal_get(self.term, gt.GHOSTTY_TERMINAL_DATA_TITLE, &t) != gt.GHOSTTY_SUCCESS) return "";
        const n = @min(t.len, self.title_buf.len);
        if (n > 0) @memcpy(self.title_buf[0..n], t.ptr[0..n]);
        self.title_len = n;
        return self.title_buf[0..n];
    }

    pub fn errorText(self: *Pane) []const u8 {
        return self.err_buf[0..self.err_len];
    }

    fn fail(self: *Pane, comptime fmt: []const u8, args: anytype) void {
        const m = std.fmt.bufPrint(&self.err_buf, fmt, args) catch self.err_buf[0..0];
        self.err_len = m.len;
        self.status.store(.failed, .release);
        self.notify.redraw(self.notify.ctx);
    }

    // ---- reader thread --------------------------------------------------------------

    fn run(self: *Pane) void {
        var attempts: usize = 0;
        while (!self.stopping.load(.acquire)) {
            const outcome = self.runInner() catch |e| blk: {
                if (self.stopping.load(.acquire)) return;
                self.fail("{s}", .{@errorName(e)});
                break :blk Outcome.restart;
            };
            if (outcome == .done) return;
            // Start over: new ticket, new snapshot. Back off if it keeps failing.
            attempts += 1;
            if (attempts > 30) return;
            self.io.sleep(.fromMilliseconds(@min(200 * @as(i64, @intCast(attempts)), 3000)), .awake) catch {};
            self.resetForRestart();
        }
    }

    const Outcome = enum { done, restart };

    fn resetForRestart(self: *Pane) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.dc) |*dc| dc.close(self.gpa);
        self.dc = null;
        if (self.scanner) |*sc| sc.deinit();
        self.scanner = null;
        // Keep the old picture until the new snapshot replaces it.
        self.status.store(.connecting, .release);
    }

    fn runInner(self: *Pane) !Outcome {
        var pbuf: [512]u8 = undefined;
        const payload = try std.fmt.bufPrint(&pbuf,
            \\{{"session_id":{f},"block_id":{f},"data_ref_id":"terminal","subprotocol":"rex.data.terminal.v1"}}
        , .{ control.q(self.sid), control.q(self.bid) });
        const t = self.ctl.call("data.connect", payload) catch {
            self.fail("{s}", .{self.ctl.lastError()});
            return .restart;
        };
        defer t.deinit();
        const ticket = control.getStr(t.value, "ticket") orelse return error.NoTicket;
        const dc = try data.DataConn.open(self.gpa, self.io, self.ep, ticket, data.terminal_v1);
        self.lock.lockUncancelable(self.io);
        self.dc = dc;
        self.lock.unlock(self.io);
        if (self.stopping.load(.acquire)) return .done;
        self.scanner = .init(self.gpa, dc.nonce);

        // Phase 1: the snapshot. The decoder pulls from `snap` which refills from the
        // stream; whatever it didn't consume is live output.
        var snap: SnapSource = .{ .pane = self };
        defer snap.buf.deinit(self.gpa);
        var dec: gt.GhosttySnapshotDecoder = null;
        const reader: gt.GhosttyReader = .{ .read = SnapSource.read, .userdata = &snap };
        if (gt.ghostty_snapshot_decoder_new(null, &dec, reader) != gt.GHOSTTY_SUCCESS) return error.GhosttyInit;
        var term: gt.GhosttyTerminal = null;
        const rc = gt.ghostty_snapshot_decoder_decode(dec, &term);
        var consumed: usize = 0;
        if (rc == gt.GHOSTTY_SUCCESS) {
            _ = gt.ghostty_snapshot_decoder_get(dec, gt.GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET, &consumed);
        }
        gt.ghostty_snapshot_decoder_free(dec);
        if (snap.closed) return if (snap.close_code == 4000) self.streamEnded(4000) else .restart;
        if (rc != gt.GHOSTTY_SUCCESS or term == null) {
            // Unknown snapshot version: rebuild the screen from the server's VT dump.
            try self.fallbackSnapshot(&snap);
            return self.liveLoop();
        }
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            if (self.search != null) gt.ghostty_search_free(self.search);
            self.search = null;
            if (self.term != null) gt.ghostty_terminal_free(self.term);
            self.term = term;
            self.refreshSize();
        }
        self.status.store(.live, .release);
        if (consumed < snap.buf.items.len) self.feed(snap.buf.items[consumed..]);
        self.notify.redraw(self.notify.ctx);
        return self.liveLoop();
    }

    fn fallbackSnapshot(self: *Pane, snap: *SnapSource) !void {
        // Find the first Rex message: it follows the snapshot.
        var marker_buf: [48]u8 = undefined;
        const marker = try std.fmt.bufPrint(&marker_buf, "\x1b_rex1;{d};", .{self.dc.?.nonce});
        while (std.mem.indexOf(u8, snap.buf.items, marker) == null) {
            if (!try snap.pull()) return error.StreamClosed;
        }
        const at = std.mem.indexOf(u8, snap.buf.items, marker).?;
        var pbuf: [512]u8 = undefined;
        const size = self.ctl.call("com.superlogical.terminal.size", try std.fmt.bufPrint(&pbuf,
            \\{{"session_id":{f},"block_id":{f},"args":{{}}}}
        , .{ control.q(self.sid), control.q(self.bid) })) catch return error.SizeFailed;
        defer size.deinit();
        const cols: u16 = @intCast(control.getInt(size.value, "columns") orelse 80);
        const rows: u16 = @intCast(control.getInt(size.value, "rows") orelse 24);
        var term: gt.GhosttyTerminal = null;
        if (gt.ghostty_terminal_new(null, &term, cols, rows) != gt.GHOSTTY_SUCCESS) return error.GhosttyInit;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            if (self.search != null) gt.ghostty_search_free(self.search);
            self.search = null;
            if (self.term != null) gt.ghostty_terminal_free(self.term);
            self.term = null;
        }
        const vt = self.ctl.call("com.superlogical.terminal.format", try std.fmt.bufPrint(&pbuf,
            \\{{"session_id":{f},"block_id":{f},"args":{{"format":"vt","extra_cursor":true,"extra_modes":true,"extra_style":true,"extra_palette":true,"extra_scrolling_region":true,"extra_keyboard":true,"extra_kitty_keyboard":true}}}}
        , .{ control.q(self.sid), control.q(self.bid) })) catch null;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            self.term = term;
            if (vt) |v| {
                const s = control.getStr(v.value, "content") orelse "";
                gt.ghostty_terminal_vt_write(term, s.ptr, s.len);
            }
            self.refreshSize();
        }
        if (vt) |v| v.deinit();
        self.status.store(.live, .release);
        self.feed(snap.buf.items[at..]);
        self.notify.redraw(self.notify.ctx);
    }

    fn liveLoop(self: *Pane) !Outcome {
        while (!self.stopping.load(.acquire)) {
            const item = try self.dc.?.next(self.gpa);
            switch (item) {
                .bytes => |b| {
                    defer self.gpa.free(b);
                    self.feed(b);
                    self.notify.redraw(self.notify.ctx);
                },
                .closed => |c| {
                    if (self.stopping.load(.acquire)) return .done;
                    switch (c.code) {
                        4000 => return self.streamEnded(c.code),
                        // ended for good, or the server wants a fresh start
                        1000, 1008, 1011, 1013 => return .restart,
                        else => if (!self.resumeStream()) return .restart,
                    }
                },
            }
        }
        return .done;
    }

    /// The connection dropped (network, keepalive): continue the same stream for up
    /// to a minute. The server keeps it for 60 s and replays what we missed.
    fn resumeStream(self: *Pane) bool {
        const old = self.dc.?;
        var delay: i64 = 100;
        var waited: i64 = 0;
        while (waited < 55_000 and !self.stopping.load(.acquire)) {
            self.io.sleep(.fromMilliseconds(delay), .awake) catch {};
            waited += delay;
            const dc = data.DataConn.resumeStream(self.gpa, self.io, self.ep, data.terminal_v1, old.token, old.received) catch |e| switch (e) {
                error.TicketInvalid, error.StreamUnavailable, error.DataRejected, error.WrongSubprotocol, error.BadWelcome => return false,
                else => {
                    delay = @min(delay * 2, 2000);
                    continue;
                },
            };
            self.lock.lockUncancelable(self.io);
            var prev = self.dc.?;
            self.dc = dc;
            self.lock.unlock(self.io);
            prev.sock.destroy(self.gpa);
            self.resumes += 1;
            return true;
        }
        return false;
    }

    fn streamEnded(self: *Pane, code: u16) Outcome {
        if (self.stopping.load(.acquire)) return .done;
        if (code == 4000) {
            self.status.store(.closed, .release);
            self.notify.redraw(self.notify.ctx);
        } else if (@import("i18n.zig").isZh()) self.fail("连接已关闭({d})", .{code}) else self.fail("stream closed ({d})", .{code});
        return .done;
    }

    /// Splits Rex messages out and applies everything in order.
    fn feed(self: *Pane, bytes: []const u8) void {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        self.scanner.?.feed(bytes, .{ .ctx = self, .bytes = sinkBytes, .message = sinkMessage });
    }

    fn sinkBytes(ctx: *anyopaque, b: []const u8) void {
        const self: *Pane = @ptrCast(@alignCast(ctx));
        if (self.term != null) gt.ghostty_terminal_vt_write(self.term, b.ptr, b.len);
    }

    fn sinkMessage(ctx: *anyopaque, msg: []const u8) void {
        const self: *Pane = @ptrCast(@alignCast(ctx));
        const parsed = std.json.parseFromSlice(std.json.Value, self.gpa, msg, .{}) catch return;
        defer parsed.deinit();
        const v = parsed.value;
        const typ = control.getStr(v, "type") orelse return;
        if (std.mem.eql(u8, typ, "resize")) {
            const cols: u16 = @intCast(@max(1, control.getInt(v, "cols") orelse return));
            const rows: u16 = @intCast(@max(1, control.getInt(v, "rows") orelse return));
            const cw: u32 = @intCast(@max(0, control.getInt(v, "cell_width_px") orelse 0));
            const ch: u32 = @intCast(@max(0, control.getInt(v, "cell_height_px") orelse 0));
            if (self.term != null) _ = gt.ghostty_terminal_resize(self.term, cols, rows, cw, ch);
            self.cols = cols;
            self.rows = rows;
        } else if (std.mem.eql(u8, typ, "child_exited")) {
            self.exit_code = control.getInt(v, "exit_code") orelse 0;
        }
    }

    /// Caller holds lock.
    fn refreshSize(self: *Pane) void {
        if (self.term == null) return;
        var c: u16 = 0;
        var r: u16 = 0;
        _ = gt.ghostty_terminal_get(self.term, gt.GHOSTTY_TERMINAL_DATA_COLS, &c);
        _ = gt.ghostty_terminal_get(self.term, gt.GHOSTTY_TERMINAL_DATA_ROWS, &r);
        self.cols = c;
        self.rows = r;
    }

    // ---- drawing (UI thread) --------------------------------------------------------

    pub const DrawResult = struct { cursor: ?struct { x: u16, y: u16, shape: vaxis.Cell.CursorShape } = null };

    /// Draws the terminal into `win`. Grapheme strings live in `arena` until render.
    pub fn draw(self: *Pane, win: vaxis.Window, arena: std.mem.Allocator) DrawResult {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        if (self.term == null) return .{};
        if (gt.ghostty_render_state_update(self.rstate, self.term) != gt.GHOSTTY_SUCCESS) return .{};
        if (gt.ghostty_render_state_get(self.rstate, gt.GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, @ptrCast(&self.row_iter)) != gt.GHOSTTY_SUCCESS) return .{};

        var y: u16 = 0;
        var cps: [16]u32 = undefined;
        while (gt.ghostty_render_state_row_iterator_next(self.row_iter)) : (y += 1) {
            if (y >= win.height) continue;
            if (gt.ghostty_render_state_row_get(self.row_iter, gt.GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, @ptrCast(&self.row_cells)) != gt.GHOSTTY_SUCCESS) continue;
            var x: u16 = 0;
            while (gt.ghostty_render_state_row_cells_next(self.row_cells)) : (x += 1) {
                if (x >= win.width) continue;
                var raw: gt.GhosttyCell = 0;
                _ = gt.ghostty_render_state_row_cells_get(self.row_cells, gt.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw);
                var wide: c_int = gt.GHOSTTY_CELL_WIDE_NARROW;
                _ = gt.ghostty_cell_get(raw, gt.GHOSTTY_CELL_DATA_WIDE, &wide);
                if (wide == gt.GHOSTTY_CELL_WIDE_SPACER_TAIL) continue;

                var style: gt.GhosttyStyle = std.mem.zeroes(gt.GhosttyStyle);
                _ = gt.ghostty_render_state_row_cells_get(self.row_cells, gt.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &style);
                var vstyle = mapStyle(style);
                var selected = false;
                if (gt.ghostty_render_state_row_cells_get(self.row_cells, gt.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED, &selected) == gt.GHOSTTY_SUCCESS and selected)
                    vstyle.reverse = !vstyle.reverse;
                if (vstyle.bg == .default) {
                    var rgb: gt.GhosttyColorRgb = undefined;
                    if (gt.ghostty_render_state_row_cells_get(self.row_cells, gt.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &rgb) == gt.GHOSTTY_SUCCESS)
                        vstyle.bg = .{ .rgb = .{ rgb.r, rgb.g, rgb.b } };
                }

                var buf: gt.GhosttyCodepointBuffer = .{ .ptr = &cps, .cap = cps.len, .len = 0 };
                _ = gt.ghostty_render_state_row_cells_get(self.row_cells, gt.GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, &buf);
                const n = @min(buf.len, cps.len);
                var grapheme: []const u8 = " ";
                if (n > 0) {
                    var tmp: [64]u8 = undefined;
                    var len: usize = 0;
                    for (cps[0..n]) |cp| {
                        const c: u21 = std.math.cast(u21, cp) orelse 0xfffd;
                        len += std.unicode.utf8Encode(c, tmp[len..]) catch break;
                        if (len + 4 > tmp.len) break;
                    }
                    grapheme = arena.dupe(u8, tmp[0..len]) catch " ";
                }
                const width: u8 = if (wide == gt.GHOSTTY_CELL_WIDE_WIDE) 2 else 1;
                if (width == 2 and x + 1 >= win.width) continue;
                win.writeCell(x, y, .{ .char = .{ .grapheme = grapheme, .width = width }, .style = vstyle });
            }
        }

        var res: DrawResult = .{};
        var visible = false;
        var has = false;
        _ = gt.ghostty_render_state_get(self.rstate, gt.GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE, &visible);
        _ = gt.ghostty_render_state_get(self.rstate, gt.GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE, &has);
        if (visible and has) {
            var cx: u16 = 0;
            var cy: u16 = 0;
            var shape: c_int = gt.GHOSTTY_CURSOR_STYLE_BLOCK;
            var blink = false;
            _ = gt.ghostty_render_state_get(self.rstate, gt.GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X, &cx);
            _ = gt.ghostty_render_state_get(self.rstate, gt.GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y, &cy);
            _ = gt.ghostty_render_state_get(self.rstate, gt.GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE, &shape);
            _ = gt.ghostty_render_state_get(self.rstate, gt.GHOSTTY_RENDER_STATE_DATA_CURSOR_BLINKING, &blink);
            if (cx < win.width and cy < win.height) {
                res.cursor = .{ .x = cx, .y = cy, .shape = switch (shape) {
                    gt.GHOSTTY_CURSOR_STYLE_BAR => if (blink) .beam_blink else .beam,
                    gt.GHOSTTY_CURSOR_STYLE_UNDERLINE => if (blink) .underline_blink else .underline,
                    else => if (blink) .block_blink else .block,
                } };
            }
        }
        return res;
    }
};

fn mapColor(c: gt.GhosttyStyleColor) vaxis.Color {
    return switch (c.tag) {
        gt.GHOSTTY_STYLE_COLOR_PALETTE => .{ .index = c.value.palette },
        gt.GHOSTTY_STYLE_COLOR_RGB => .{ .rgb = .{ c.value.rgb.r, c.value.rgb.g, c.value.rgb.b } },
        else => .default,
    };
}

fn mapStyle(s: gt.GhosttyStyle) vaxis.Style {
    const f = s.flags;
    const ul: u16 = (f >> 8) & 0x7;
    return .{
        .fg = mapColor(s.fg_color),
        .bg = mapColor(s.bg_color),
        .ul = mapColor(s.underline_color),
        .ul_style = switch (ul) {
            1 => .single,
            2 => .double,
            3 => .curly,
            4 => .dotted,
            5 => .dashed,
            else => .off,
        },
        .bold = f & (1 << 0) != 0,
        .italic = f & (1 << 1) != 0,
        .dim = f & (1 << 2) != 0,
        .blink = f & (1 << 3) != 0,
        .reverse = f & (1 << 4) != 0,
        .invisible = f & (1 << 5) != 0,
        .strikethrough = f & (1 << 6) != 0,
    };
}

/// Feeds the snapshot decoder from the data stream, keeping every byte so the part
/// after the snapshot can be replayed as live output.
const SnapSource = struct {
    pane: *Pane,
    buf: std.ArrayList(u8) = .empty,
    pos: usize = 0,
    closed: bool = false,
    close_code: u16 = 0,

    fn pull(self: *SnapSource) !bool {
        if (self.closed) return false;
        const item = try self.pane.dc.?.next(self.pane.gpa);
        switch (item) {
            .bytes => |b| {
                defer self.pane.gpa.free(b);
                try self.buf.appendSlice(self.pane.gpa, b);
                return true;
            },
            .closed => |c| {
                self.closed = true;
                self.close_code = c.code;
                return false;
            },
        }
    }

    fn read(userdata: ?*anyopaque, out: [*c]u8, cap: usize, out_read: [*c]usize) callconv(.c) bool {
        const self: *SnapSource = @ptrCast(@alignCast(userdata.?));
        if (self.pos == self.buf.items.len) {
            const more = self.pull() catch return false;
            if (!more) {
                out_read.* = 0; // EOF
                return true;
            }
        }
        const n = @min(cap, self.buf.items.len - self.pos);
        @memcpy(out[0..n], self.buf.items[self.pos..][0..n]);
        self.pos += n;
        out_read.* = n;
        return true;
    }
};
