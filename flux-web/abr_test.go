package main

import (
	"encoding/binary"
	"testing"
	"time"

	"github.com/pion/rtcp"
)

func TestABRGCCEstimateClampsToFloorAndCeiling(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{commandChan: commands, ceilingKbps: 10_000, lastGCCUpdate: time.Now().Add(-abrGCCUpdateInterval)}

	state.onEstimate(500)
	if state.targetKbps != abrMinKbps {
		t.Fatalf("floor target = %d, want %d", state.targetKbps, abrMinKbps)
	}
	if got := binary.BigEndian.Uint32((<-commands)[1:]); got != abrMinKbps {
		t.Fatalf("floor command = %d, want %d", got, abrMinKbps)
	}

	state.lastGCCUpdate = time.Now().Add(-abrGCCUpdateInterval)
	state.onEstimate(20_000)
	if state.targetKbps != 10_000 {
		t.Fatalf("ceiling target = %d, want 10000", state.targetKbps)
	}
	if got := binary.BigEndian.Uint32((<-commands)[1:]); got != 10_000 {
		t.Fatalf("ceiling command = %d, want 10000", got)
	}
}

func TestABRGCCEstimateSuppressesSmallAndRapidUpdates(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{
		commandChan:   commands,
		targetKbps:    5_000,
		ceilingKbps:   20_000,
		lastGCCUpdate: time.Now().Add(-abrGCCUpdateInterval),
	}

	state.onEstimate(5_400)
	if state.targetKbps != 5_000 {
		t.Fatalf("small estimate changed target to %d", state.targetKbps)
	}
	if len(commands) != 0 {
		t.Fatal("small estimate emitted a command")
	}

	state.onEstimate(6_000)
	if state.targetKbps != 6_000 {
		t.Fatalf("material estimate target = %d, want 6000", state.targetKbps)
	}
	if got := binary.BigEndian.Uint32((<-commands)[1:]); got != 6_000 {
		t.Fatalf("material command = %d, want 6000", got)
	}

	state.onEstimate(8_000)
	if state.targetKbps != 6_000 {
		t.Fatalf("rapid estimate changed target to %d", state.targetKbps)
	}
	if len(commands) != 0 {
		t.Fatal("rapid estimate emitted a command")
	}
}

func TestABRReportsDoNotSteerGCCTarget(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{
		commandChan: commands,
		targetKbps:  8_000,
	}

	state.onReceiverReport(0.50)
	state.onRTTSample(10 * time.Millisecond)
	for i := 0; i < 50; i++ {
		state.onRTTSample(200 * time.Millisecond)
	}
	if state.targetKbps != 8_000 {
		t.Fatalf("measurement changed target to %d", state.targetKbps)
	}
	if len(commands) != 0 {
		t.Fatal("measurement emitted a bitrate command")
	}
}

func TestABRSenderCeilingOnlyPullsTargetDown(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{commandChan: commands, targetKbps: 8_000}

	state.setSenderTarget(6_000)
	if state.targetKbps != 6_000 {
		t.Fatalf("lowered ceiling target = %d, want 6000", state.targetKbps)
	}
	if got := binary.BigEndian.Uint32((<-commands)[1:]); got != 6_000 {
		t.Fatalf("lowered ceiling command = %d, want 6000", got)
	}

	state.setSenderTarget(12_000)
	if state.targetKbps != 6_000 {
		t.Fatalf("raised ceiling pushed target to %d", state.targetKbps)
	}
	if len(commands) != 0 {
		t.Fatal("raised ceiling emitted a command")
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
