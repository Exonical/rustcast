// Wire-protocol constants shared with the Rust host. Each value mirrors a named
// Rust definition; lib/protocol.test.ts parses those Rust sources and fails if
// the two sides drift.

// `ModifierFlags` in flux/crates/flux-input/src/keyboard.rs.
export const ModifierFlags = {
  SHIFT: 0x0001,
  CTRL: 0x0002,
  ALT: 0x0004,
  META: 0x0008,
  CAPS_LOCK: 0x0010,
  NUM_LOCK: 0x0020,
} as const;

export type ModifierState = Pick<KeyboardEvent, "shiftKey" | "ctrlKey" | "altKey" | "metaKey" | "getModifierState">;

export function modifierFlags(e: ModifierState): number {
  let modifiers = 0;
  if (e.shiftKey) modifiers |= ModifierFlags.SHIFT;
  if (e.ctrlKey) modifiers |= ModifierFlags.CTRL;
  if (e.altKey) modifiers |= ModifierFlags.ALT;
  if (e.metaKey) modifiers |= ModifierFlags.META;
  if (e.getModifierState("CapsLock")) modifiers |= ModifierFlags.CAPS_LOCK;
  if (e.getModifierState("NumLock")) modifiers |= ModifierFlags.NUM_LOCK;
  return modifiers;
}

export const MIN_QUALITY_LEVEL = 1;
export const MAX_QUALITY_LEVEL = 10;

// `quality_bpp()` in flux/crates/flux-server/src/main.rs, indexed by level - 1.
export const QUALITY_BPP: readonly number[] = [0.025, 0.035, 0.05, 0.065, 0.08, 0.1, 0.12, 0.14, 0.16, 0.2];

// Clamp bounds applied by `bitrate_kbps_for()` in flux/crates/flux-server/src/main.rs.
export const MIN_TARGET_BITRATE_KBPS = 3_000;
export const MAX_TARGET_BITRATE_KBPS = 50_000;

export function qualityBpp(level: number): number {
  const clamped = Math.min(Math.max(level, MIN_QUALITY_LEVEL), MAX_QUALITY_LEVEL);
  return QUALITY_BPP[clamped - MIN_QUALITY_LEVEL];
}

// Mirror of `bitrate_kbps_for()`: the encoder target the host derives for a quality level.
export function targetBitrateKbps(level: number, width: number, height: number, fps: number): number {
  const kbps = Math.round((width * height * fps * qualityBpp(level)) / 1000);
  return Math.min(Math.max(kbps, MIN_TARGET_BITRATE_KBPS), MAX_TARGET_BITRATE_KBPS);
}
