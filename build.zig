const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const ghostty_dep = b.dependency("ghostty", .{ .target = target, .optimize = optimize });
    const vaxis_dep = b.dependency("vaxis", .{ .target = target, .optimize = optimize });

    // libghostty-vt's stable C API, translated for Zig.
    const ghostty_c = b.addTranslateC(.{
        .root_source_file = b.path("src/c/ghostty.h"),
        .target = target,
        .optimize = optimize,
    });
    ghostty_c.addIncludePath(ghostty_dep.path("include"));

    const mod = b.createModule(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    mod.addImport("vaxis", vaxis_dep.module("vaxis"));
    mod.addImport("gt", ghostty_c.createModule());
    mod.linkLibrary(ghostty_dep.artifact("ghostty-vt-static"));

    const exe = b.addExecutable(.{ .name = "rexa", .root_module = mod });
    b.installArtifact(exe);

    const run = b.addRunArtifact(exe);
    run.step.dependOn(b.getInstallStep());
    run.addPassthruArgs();
    b.step("run", "Run rexa").dependOn(&run.step);

    const tests = b.addTest(.{ .root_module = mod });
    b.step("test", "Run unit tests").dependOn(&b.addRunArtifact(tests).step);
}
