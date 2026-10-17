export type ResolutionStatusState = "transitioning" | "recovering" | "succeeded" | "failed";

export class ResolutionPaintTracker {
  private receivedFrames = 0;
  private successAfterFrame: number | null = null;

  reset(): void {
    this.receivedFrames = 0;
    this.successAfterFrame = null;
  }

  onResolutionStatus(state: ResolutionStatusState): void {
    this.successAfterFrame = state === "succeeded" ? this.receivedFrames : null;
  }

  onFrameReceived(): number {
    this.receivedFrames++;
    return this.receivedFrames;
  }

  onKeyframePainted(frameOrdinal: number, keyframe: boolean): boolean {
    if (!keyframe || this.successAfterFrame === null || frameOrdinal <= this.successAfterFrame) {
      return false;
    }
    this.successAfterFrame = null;
    return true;
  }
}
