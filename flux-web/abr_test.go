package main

import (
	"encoding/binary"
	"testing"
	"time"

	"github.com/pion/rtcp"
)

func TestABRIdleLossRecoversToSenderTarget(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{commandChan: commands}
	state.setSenderTarget(10_000)
	state.measuredKbps = 20 // Idle heartbeat traffic must not define capacity.
	state.lastDecrease = time.Now().Add(-abrDecreaseCooldown)

	state.onReceiverReport(0.10)
	if state.targetKbps != 7_000 {
		t.Fatalf("loss target = %d, want 7000", state.targetKbps)
	}
	if state.ceilingKbps != 10_000 {
		t.Fatalf("loss ceiling = %d, want sender target 10000", state.ceilingKbps)
	}
	command := <-commands
	if got := binary.BigEndian.Uint32(command[1:]); got != 7_000 {
		t.Fatalf("loss command = %d, want 7000", got)
	}

	state.cleanSince = time.Now().Add(-abrCleanBeforeRaise)
	state.lastIncrease = time.Time{}
	state.onReceiverReport(0)
	if state.targetKbps != 8_049 {
		t.Fatalf("recovery target = %d, want 8049", state.targetKbps)
	}
	if state.ceilingKbps != 10_000 {
		t.Fatalf("recovery ceiling = %d, want sender target 10000", state.ceilingKbps)
	}

	state.setSenderTarget(10_000) // Repeated heartbeat must not undo adaptation.
	if state.targetKbps != 8_049 {
		t.Fatalf("repeated sender report reset target to %d", state.targetKbps)
	}
}

func TestABRRTTInflationLowersBitrateAndGatesRecovery(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{commandChan: commands}
	state.setSenderTarget(10_000)
	state.lastDecrease = time.Now().Add(-abrDecreaseCooldown)

	state.onRTTSample(10 * time.Millisecond) // establishes baseline
	if state.targetKbps != 10_000 {
		t.Fatalf("baseline sample changed target to %d", state.targetKbps)
	}

	// Sustained inflated RTT converges the EWMA above baseline+threshold.
	for i := 0; i < 50; i++ {
		state.onRTTSample(200 * time.Millisecond)
	}
	if state.targetKbps != 7_000 {
		t.Fatalf("inflated RTT target = %d, want 7000", state.targetKbps)
	}
	command := <-commands
	if got := binary.BigEndian.Uint32(command[1:]); got != 7_000 {
		t.Fatalf("RTT command = %d, want 7000", got)
	}

	// Clean loss reports must not raise the bitrate while RTT is still inflated.
	state.cleanSince = time.Now().Add(-abrCleanBeforeRaise)
	state.lastIncrease = time.Time{}
	state.onReceiverReport(0)
	if state.targetKbps != 7_000 {
		t.Fatalf("recovery ran during RTT inflation: target = %d", state.targetKbps)
	}

	// Once RTT returns to baseline, recovery proceeds.
	for i := 0; i < 100; i++ {
		state.onRTTSample(10 * time.Millisecond)
	}
	state.cleanSince = time.Now().Add(-abrCleanBeforeRaise)
	state.onReceiverReport(0)
	if state.targetKbps != 8_049 {
		t.Fatalf("post-drain recovery target = %d, want 8049", state.targetKbps)
	}
}

func TestRTTFromReport(t *testing.T) {
	arrival := time.Unix(1_700_000_000, 500_000_000)
	const ntpEpochOffset = 2208988800
	secs := uint64(arrival.Unix()) + ntpEpochOffset
	frac := uint64(arrival.Nanosecond()) << 32 / uint64(time.Second)
	nowNTP32 := uint32(secs<<16 | frac>>16)

	report := rtcp.ReceptionReport{
		LastSenderReport: nowNTP32 - 65536/10, // SR sent 100ms before arrival
		Delay:            65536 / 20,          // receiver held it 50ms
	}
	rtt := rttFromReport(report, arrival)
	if rtt < 45*time.Millisecond || rtt > 55*time.Millisecond {
		t.Fatalf("rtt = %v, want ~50ms", rtt)
	}

	if got := rttFromReport(rtcp.ReceptionReport{}, arrival); got != 0 {
		t.Fatalf("zero LSR should yield 0, got %v", got)
	}
}

func TestABRRTTBaselineFollowsASlowerPath(t *testing.T) {
	state := &abrState{commandChan: make(chan []byte, 4)}
	state.setSenderTarget(10_000)

	state.onRTTSample(10 * time.Millisecond) // fast path baseline
	// The path genuinely becomes slower (e.g. Ethernet → WiFi). Once the
	// trailing window rolls over, the baseline must follow rather than
	// treating the new floor as permanent congestion.
	state.windowStarted = time.Now().Add(-abrRTTBaseWindow)
	state.windowMinRTT = 0
	for i := 0; i < 50; i++ {
		state.onRTTSample(120 * time.Millisecond)
	}
	if state.baseRTT < 100*time.Millisecond {
		t.Fatalf("baseline = %v, want it to follow the slower path", state.baseRTT)
	}
	if state.smoothedRTT-state.baseRTT > abrRTTInflation {
		t.Fatalf("stable slower path still reads as inflated: smoothed=%v base=%v",
			state.smoothedRTT, state.baseRTT)
	}
}
