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

func TestPacingScheduleLargeFrameFitsFrameInterval(t *testing.T) {
	packetSizes := []int{10_000, 10_000}
	frameDuration := 16 * time.Millisecond
	schedule := pacingSchedule(packetSizes, 100, frameDuration)
	frameBits := (packetSizes[0] + packetSizes[1]) * 8
	rate := float64(frameBits) / frameDuration.Seconds()
	lastPacketEnd := schedule[len(schedule)-1] +
		time.Duration(float64(packetSizes[len(packetSizes)-1]*8)/rate*float64(time.Second))
	if lastPacketEnd > frameDuration {
		t.Fatalf("large frame ends at %s, beyond frame interval %s", lastPacketEnd, frameDuration)
	}
}

func TestEmissionSequenceNumbersRemainContiguousAcrossAbandonment(t *testing.T) {
	next := uint16(65534)
	var emitted []uint16
	for range 2 {
		packet := &rtp.Packet{}
		assignSequenceNumber(packet, &next)
		emitted = append(emitted, packet.Header.SequenceNumber)
	}
	// The remainder of this frame is abandoned. The next frame starts with
	// the next emitted sequence number rather than the packetizer's counter.
	for range 3 {
		packet := &rtp.Packet{}
		assignSequenceNumber(packet, &next)
		emitted = append(emitted, packet.Header.SequenceNumber)
	}
	want := []uint16{65534, 65535, 0, 1, 2}
	for i := range want {
		if emitted[i] != want[i] {
			t.Fatalf("emitted sequence[%d] = %d, want %d", i, emitted[i], want[i])
		}
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
