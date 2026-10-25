export const DROP_REASONS = [
  "not_painted",
  "awaiting_keyframe",
  "decode_backlog",
  "lost",
  "decoder_error",
] as const;

export type DropReason = typeof DROP_REASONS[number];
export type DropCounts = Record<DropReason, number>;

function emptyCounts(): DropCounts {
  return {
    not_painted: 0,
    awaiting_keyframe: 0,
    decode_backlog: 0,
    lost: 0,
    decoder_error: 0,
  };
}

export class DropCounter {
  private cumulative = emptyCounts();
  private window = emptyCounts();

  add(reason: DropReason, count = 1): void {
    if (!(count > 0)) return;
    this.cumulative[reason] += count;
    this.window[reason] += count;
  }

  get total(): number {
    return DROP_REASONS.reduce((total, reason) => total + this.cumulative[reason], 0);
  }

  takeWindow(): DropCounts {
    const counts = { ...this.window };
    this.window = emptyCounts();
    return counts;
  }

  reset(): void {
    this.cumulative = emptyCounts();
    this.window = emptyCounts();
  }
}

export class DropVisibilityPolicy {
  private hidden = false;
  private awaitingResumeKeyframe = false;

  get isHidden(): boolean {
    return this.hidden;
  }

  setHidden(hidden: boolean): boolean {
    const becameVisible = this.hidden && !hidden;
    if (hidden || becameVisible) this.awaitingResumeKeyframe = true;
    this.hidden = hidden;
    return becameVisible;
  }

  suppresses(reason: DropReason): boolean {
    return this.hidden || (this.awaitingResumeKeyframe && reason === "awaiting_keyframe");
  }

  acceptKeyframe(): void {
    if (!this.hidden) this.awaitingResumeKeyframe = false;
  }

  reset(): void {
    this.hidden = false;
    this.awaitingResumeKeyframe = false;
  }
}
