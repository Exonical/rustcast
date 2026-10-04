import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
  MAX_QUALITY_LEVEL,
  MAX_TARGET_BITRATE_KBPS,
  MIN_QUALITY_LEVEL,
  MIN_TARGET_BITRATE_KBPS,
  ModifierFlags,
  QUALITY_BPP,
  modifierFlags,
  qualityBpp,
  targetBitrateKbps,
  type ModifierState,
} from "./protocol";

const repoRoot = resolve(process.cwd(), "../..");

function rustSource(path: string): string {
  return readFileSync(resolve(repoRoot, path), "utf8");
}

function rustItem(source: string, header: string): string {
  const start = source.indexOf(header);
  assert.notEqual(start, -1, `missing Rust item: ${header}`);
  const end = source.indexOf("\n}", start);
  assert.notEqual(end, -1, `unterminated Rust item: ${header}`);
  return source.slice(start, end);
}

function rustNumber(literal: string): number {
  return Number(literal.replace(/_/g, ""));
}

const serverMain = rustSource("flux/crates/flux-server/src/main.rs");

test("QUALITY_BPP matches quality_bpp() in flux-server", () => {
  const body = rustItem(serverMain, "fn quality_bpp(");
  const levelClamp = body.match(/level\.clamp\((\d+),\s*(\d+)\)/);
  assert.ok(levelClamp, "quality_bpp level clamp not found");
  assert.equal(Number(levelClamp[1]), MIN_QUALITY_LEVEL);
  assert.equal(Number(levelClamp[2]), MAX_QUALITY_LEVEL);

  const arms = [...body.matchAll(/^\s*(\d+|_)\s*=>\s*([\d.]+),/gm)];
  const table = arms.map(([, level, bpp]) => [level === "_" ? MAX_QUALITY_LEVEL : Number(level), Number(bpp)]);
  assert.deepEqual(
    table,
    QUALITY_BPP.map((bpp, i) => [MIN_QUALITY_LEVEL + i, bpp]),
  );
});

test("target bitrate clamp matches bitrate_kbps_for() in flux-server", () => {
  const body = rustItem(serverMain, "fn bitrate_kbps_for(");
  const clamp = body.match(/\.clamp\(([\d_.]+),\s*([\d_.]+)\)/);
  assert.ok(clamp, "bitrate_kbps_for clamp not found");
  assert.equal(rustNumber(clamp[1]), MIN_TARGET_BITRATE_KBPS);
  assert.equal(rustNumber(clamp[2]), MAX_TARGET_BITRATE_KBPS);
});

test("ModifierFlags matches flux-input keyboard ModifierFlags", () => {
  const body = rustItem(rustSource("flux/crates/flux-input/src/keyboard.rs"), "pub struct ModifierFlags");
  const flags = Object.fromEntries(
    [...body.matchAll(/const\s+(\w+)\s*=\s*(0x[0-9a-fA-F]+);/g)].map(([, name, value]) => [name, Number(value)]),
  );
  assert.deepEqual(flags, { ...ModifierFlags });
});

test("targetBitrateKbps follows the host formula and clamp", () => {
  assert.equal(targetBitrateKbps(6, 1920, 1080, 60), 12_442);
  assert.equal(targetBitrateKbps(1, 1280, 720, 30), MIN_TARGET_BITRATE_KBPS);
  assert.equal(targetBitrateKbps(10, 3840, 2160, 60), MAX_TARGET_BITRATE_KBPS);
  assert.equal(qualityBpp(0), QUALITY_BPP[0]);
  assert.equal(qualityBpp(11), QUALITY_BPP[QUALITY_BPP.length - 1]);
});

test("modifierFlags maps keyboard state to the wire mask", () => {
  const state = (overrides: Partial<ModifierState>, locks: string[] = []): ModifierState => ({
    shiftKey: false,
    ctrlKey: false,
    altKey: false,
    metaKey: false,
    getModifierState: (key: string) => locks.includes(key),
    ...overrides,
  });
  assert.equal(modifierFlags(state({})), 0);
  assert.equal(modifierFlags(state({ shiftKey: true, metaKey: true })), ModifierFlags.SHIFT | ModifierFlags.META);
  assert.equal(
    modifierFlags(state({ ctrlKey: true, altKey: true }, ["CapsLock", "NumLock"])),
    ModifierFlags.CTRL | ModifierFlags.ALT | ModifierFlags.CAPS_LOCK | ModifierFlags.NUM_LOCK,
  );
});
