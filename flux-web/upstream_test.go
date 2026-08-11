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

func TestLatestFrameReportsDiscardedFrames(t *testing.T) {
	ch := make(chan frameMsg, 2)
	ch <- frameMsg{tsMicros: 2}
	ch <- frameMsg{tsMicros: 3}
	latest, dropped := latestFrame(ch, frameMsg{tsMicros: 1})
	if !dropped || latest.tsMicros != 3 {
		t.Fatalf("latestFrame() = (%+v, %t), want newest frame and dropped=true", latest, dropped)
	}
}

func TestIDRRequestGateBoundsBurstAcrossDropPaths(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })

	for range 6 {
		u.requestIDR()
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("burst queued %d IDR requests, want exactly 1", got)
	}
	if got := <-u.commandChan; len(got) != 1 || got[0] != 0x01 {
		t.Fatalf("burst queued command %#v, want IDR command", got)
	}

	now = now.Add(idrRequestInterval)
	if !u.requestIDR() {
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

	if !u.requestIDR() {
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
