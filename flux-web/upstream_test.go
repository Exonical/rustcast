package main

import (
	"testing"
	"time"

	"github.com/pion/rtp"
)

func TestCaptureFrameDuration(t *testing.T) {
	tests := []struct {
		name       string
		lastTs     uint64
		currentTs  uint64
		haveLastTs bool
		want       time.Duration
	}{
		{
			name:       "normal frame interval",
			lastTs:     1_000_000,
			currentTs:  1_016_000,
			haveLastTs: true,
			want:       16 * time.Millisecond,
		},
		{
			name:       "multi-second idle gap is preserved",
			lastTs:     1_000_000,
			currentTs:  4_000_000,
			haveLastTs: true,
			want:       3 * time.Second,
		},
		{
			name:       "backwards timestamp uses nominal duration",
			lastTs:     4_000_000,
			currentTs:  3_000_000,
			haveLastTs: true,
			want:       defaultFrameDuration,
		},
		{
			name:       "timestamp reset uses nominal duration",
			lastTs:     9_000_000,
			currentTs:  0,
			haveLastTs: true,
			want:       defaultFrameDuration,
		},
		{
			name:       "first sample uses nominal duration",
			lastTs:     0,
			currentTs:  2_000_000,
			haveLastTs: false,
			want:       defaultFrameDuration,
		},
		{
			name:       "absurd timestamp gap is capped",
			lastTs:     1,
			currentTs:  31_000_000,
			haveLastTs: true,
			want:       maxSaneFrameDuration,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := captureFrameDuration(test.lastTs, test.currentTs, test.haveLastTs); got != test.want {
				t.Fatalf("captureFrameDuration(%d, %d, %t) = %s, want %s",
					test.lastTs, test.currentTs, test.haveLastTs, got, test.want)
			}
		})
	}
}

func TestPacingScheduleUsesTwiceTargetBitrate(t *testing.T) {
	schedule := pacingSchedule([]int{1000, 1000, 500}, 1000, 16*time.Millisecond)
	want := []time.Duration{0, 4 * time.Millisecond, 8 * time.Millisecond}
	for i := range want {
		if schedule[i] != want[i] {
			t.Fatalf("schedule[%d] = %s, want %s", i, schedule[i], want[i])
		}
	}
}

func TestPacingScheduleUsesFrameRateFloor(t *testing.T) {
	schedule := pacingSchedule([]int{1200, 800}, 0, 16*time.Millisecond)
	if schedule[0] != 0 || schedule[1] != 9600*time.Microsecond {
		t.Fatalf("frame-rate floor schedule = %v, want [0 9.6ms]", schedule)
	}
}

func TestPacingScheduleLargeFrameIsBoundedByTargetMultiple(t *testing.T) {
	packetSizes := []int{10_000, 10_000}
	schedule := pacingSchedule(packetSizes, 100, 16*time.Millisecond)
	if got, want := schedule[len(schedule)-1], 200*time.Millisecond; got != want {
		t.Fatalf("large frame's final packet starts at %s, want %s", got, want)
	}
}

func TestPacingScheduleIDRUsesEmissionTargetAfterLongGap(t *testing.T) {
	schedule := pacingScheduleForFrame([]int{50_000, 30_000}, 2_100, time.Second, true)
	if got, want := schedule[1], 40*time.Millisecond; got != want {
		t.Fatalf("IDR final packet starts at %s, want %s", got, want)
	}
	if got, want := schedule[1]+30_000*8*time.Second/time.Duration(pacingIDRMaxRate), 64*time.Millisecond; got != want {
		t.Fatalf("IDR emission time = %s, want %s", got, want)
	}
}

func TestPacingScheduleIDRRespectsAbsoluteRateCeiling(t *testing.T) {
	schedule := pacingScheduleForFrame([]int{1_000_000, 1}, 1_000, time.Second, true)
	if got, want := schedule[1], 800*time.Millisecond; got != want {
		t.Fatalf("large IDR final packet starts at %s, want %s", got, want)
	}
}

func TestLatestFrameReportsDiscardedFrames(t *testing.T) {
	tests := []struct {
		name     string
		current  frameMsg
		queued   []frameMsg
		wantTs   uint64
		wantIDR  bool
		wantDrop bool
	}{
		{
			name:     "keyframe outranks newer P-frame",
			current:  pFrame(1),
			queued:   []frameMsg{idrFrame(2), pFrame(3)},
			wantTs:   2,
			wantIDR:  true,
			wantDrop: true,
		},
		{
			name:     "newest keyframe wins among multiple keyframes",
			current:  pFrame(1),
			queued:   []frameMsg{idrFrame(2), pFrame(3), idrFrame(4), pFrame(5)},
			wantTs:   4,
			wantIDR:  true,
			wantDrop: true,
		},
		{
			name:     "newest P-frame wins without a keyframe",
			current:  pFrame(1),
			queued:   []frameMsg{pFrame(2), pFrame(3)},
			wantTs:   3,
			wantIDR:  false,
			wantDrop: true,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			ch := make(chan frameMsg, len(test.queued))
			for _, frame := range test.queued {
				ch <- frame
			}
			latest, dropped := latestFrame(ch, test.current)
			if latest.tsMicros != test.wantTs || isIDRFrame(latest.data) != test.wantIDR || dropped != test.wantDrop {
				t.Fatalf("latestFrame() = (%+v, %t), want ts=%d idr=%t dropped=%t",
					latest, dropped, test.wantTs, test.wantIDR, test.wantDrop)
			}
		})
	}
}

func TestGatedSkipRequestsRecoveryIDRThroughGate(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })
	session := &Session{needsIDR: true}

	if !u.skipFrameUntilIDR(session, false) {
		t.Fatal("gated P-frame was not skipped")
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("first gated skip queued %d IDR requests, want 1", got)
	}
	if !u.skipFrameUntilIDR(session, false) {
		t.Fatal("second gated P-frame was not skipped")
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("same-window gated skip queued %d IDR requests, want 1", got)
	}

	now = now.Add(idrRequestInterval)
	if !u.skipFrameUntilIDR(session, false) {
		t.Fatal("next-window gated P-frame was not skipped")
	}
	if got := len(u.commandChan); got != 2 {
		t.Fatalf("next-window gated skip queued %d IDR requests, want 2 total", got)
	}
}

func TestGatedSkipDoesNotSkipIDR(t *testing.T) {
	u := newMachineUpstream("", "", nil)
	session := &Session{needsIDR: true}
	if u.skipFrameUntilIDR(session, true) {
		t.Fatal("IDR was incorrectly skipped")
	}
	if got := len(u.commandChan); got != 0 {
		t.Fatalf("IDR path queued %d recovery requests, want 0", got)
	}
}

func pFrame(ts uint64) frameMsg {
	return frameMsg{tsMicros: ts, data: []byte{0, 0, 0, 1, 0x41}}
}

func idrFrame(ts uint64) frameMsg {
	return frameMsg{tsMicros: ts, data: []byte{0, 0, 0, 1, 0x65}}
}

func TestIDRRequestGateBoundsBurstAcrossDropPaths(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })

	for range 6 {
		u.requestIDR(idrReasonStaleQueue)
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("burst queued %d IDR requests, want exactly 1", got)
	}
	if got := <-u.commandChan; len(got) != 1 || got[0] != 0x01 {
		t.Fatalf("burst queued command %#v, want IDR command", got)
	}

	now = now.Add(idrRequestInterval)
	if !u.requestIDR(idrReasonStaleQueue) {
		t.Fatal("request at the next gate window was rejected")
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("second gate window queued %d IDR requests, want 1", got)
	}
	if got := <-u.commandChan; len(got) != 1 || got[0] != 0x01 {
		t.Fatalf("next-window queued command %#v, want IDR command", got)
	}
}

func TestInitialIDRRequestBypassesGateOncePerSession(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })
	session := &Session{}

	if !u.requestIDR(idrReasonViewerPLI) {
		t.Fatal("regular IDR request was rejected")
	}
	if !u.requestInitialIDR(session) {
		t.Fatal("initial session IDR request was delayed by the shared gate")
	}
	if u.requestInitialIDR(session) {
		t.Fatal("initial-session exemption was reused")
	}
	if got := len(u.commandChan); got != 2 {
		t.Fatalf("queued %d IDR requests, want regular plus one initial request", got)
	}
}

func TestEmissionSequenceNumbersRemainContiguousAcrossAbandonment(t *testing.T) {
	next := uint16(65534)
	var emitted []uint16
	for range 2 {
		packet := &rtp.Packet{}
		assignSequenceNumber(packet, &next)
		emitted = append(emitted, packet.Header.SequenceNumber)
		commitSequenceNumber(&next, true)
	}
	// The remainder of this frame is abandoned. The next frame starts with
	// the next emitted sequence number rather than the packetizer's counter.
	for range 3 {
		packet := &rtp.Packet{}
		assignSequenceNumber(packet, &next)
		emitted = append(emitted, packet.Header.SequenceNumber)
		commitSequenceNumber(&next, true)
	}
	want := []uint16{65534, 65535, 0, 1, 2}
	for i := range want {
		if emitted[i] != want[i] {
			t.Fatalf("emitted sequence[%d] = %d, want %d", i, emitted[i], want[i])
		}
	}
}

func TestSequenceNumberDoesNotAdvanceOnWriteError(t *testing.T) {
	next := uint16(41)
	packet := &rtp.Packet{}
	assignSequenceNumber(packet, &next)
	if packet.Header.SequenceNumber != next {
		t.Fatalf("assigned sequence = %d, want %d", packet.Header.SequenceNumber, next)
	}
	if next != 41 {
		t.Fatalf("sequence advanced before write: %d", next)
	}

	commitSequenceNumber(&next, false)
	if next != 41 {
		t.Fatalf("failed write sequence = %d, want 41", next)
	}
	commitSequenceNumber(&next, true)
	if next != 42 {
		t.Fatalf("successful write sequence = %d, want 42", next)
	}
}

func TestAbandonedFrameDoesNotOveradvanceRTPClock(t *testing.T) {
	firstTicks, remainder := consumeRTPDuration(16*time.Millisecond, 0)
	secondTicks, remainder := consumeRTPDuration(16*time.Millisecond, remainder)
	combinedTicks, combinedRemainder := consumeRTPDuration(32*time.Millisecond, 0)
	if firstTicks+secondTicks != combinedTicks || remainder != combinedRemainder {
		t.Fatalf(
			"abandoned-frame timeline = (%d, %v), direct timeline = (%d, %v)",
			firstTicks+secondTicks,
			remainder,
			combinedTicks,
			combinedRemainder,
		)
	}
}

func TestIDRStatsSeparatesGrantedFromSuppressedByReason(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })

	u.requestIDR(idrReasonViewerPLI)  // granted
	u.requestIDR(idrReasonAbandoned)  // gated
	u.requestIDR(idrReasonStaleQueue) // gated
	u.requestIDR(idrReasonStaleQueue) // gated

	summary := u.idrStats.drain()
	want := "viewer-pli=1(+0 suppressed) stale-queue-discard=0(+2 suppressed) abandoned-packets=0(+1 suppressed)"
	if summary != want {
		t.Fatalf("summary = %q, want %q", summary, want)
	}
	if again := u.idrStats.drain(); again != "" {
		t.Fatalf("counts survived a drain: %q", again)
	}
}

func TestStageStatsReportsWorstCaseSeparatelyFromAverage(t *testing.T) {
	var stats stageStats
	stats.observe(1, 2*time.Millisecond, 4*time.Millisecond, false, 10_000)
	stats.observe(9, 20*time.Millisecond, 40*time.Millisecond, true, 700_000)

	summary := stats.summary()
	want := "frames=2 queue avg=5.0 max=9 | relay wait avg=11.0ms max=20.0ms | " +
		"pacing avg=22.0ms max=40.0ms | idr n=1 max=700000 bytes max pacing=40.0ms"
	if summary != want {
		t.Fatalf("summary = %q, want %q", summary, want)
	}
	if empty := (&stageStats{}).summary(); empty != "" {
		t.Fatalf("idle window summary = %q, want empty", empty)
	}
}
