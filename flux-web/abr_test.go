package main

import (
	"encoding/binary"
	"testing"
	"time"
)

func TestABRInitialBitrateKbps(t *testing.T) {
	tests := []struct {
		name        string
		ceilingKbps uint32
		want        uint32
	}{
		{name: "default", ceilingKbps: 0, want: abrInitialBitrateKbps},
		{name: "sender target", ceilingKbps: 12_000, want: 12_000},
		{name: "maximum", ceilingKbps: 100_000, want: 50_000},
		{name: "minimum", ceilingKbps: 1_000, want: 1_500},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			state := &abrState{ceilingKbps: tt.ceilingKbps}
			if got := state.initialBitrateKbps(); got != tt.want {
				t.Fatalf("initial bitrate = %d, want %d", got, tt.want)
			}
		})
	}
}

func TestABREstimateClampsToFloorAndCeiling(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{commandChan: commands, ceilingKbps: 10_000, lastEstimateUpdate: time.Now().Add(-abrEstimateInterval)}

	state.onEstimate(500)
	if state.targetKbps != abrMinKbps {
		t.Fatalf("floor target = %d, want %d", state.targetKbps, abrMinKbps)
	}
	if got := binary.BigEndian.Uint32((<-commands)[1:]); got != abrMinKbps {
		t.Fatalf("floor command = %d, want %d", got, abrMinKbps)
	}

	state.lastEstimateUpdate = time.Now().Add(-abrEstimateInterval)
	state.onEstimate(20_000)
	if state.targetKbps != 10_000 {
		t.Fatalf("ceiling target = %d, want 10000", state.targetKbps)
	}
	if got := binary.BigEndian.Uint32((<-commands)[1:]); got != 10_000 {
		t.Fatalf("ceiling command = %d, want 10000", got)
	}
}

func TestABREstimateSuppressesSmallAndRapidUpdates(t *testing.T) {
	commands := make(chan []byte, 4)
	state := &abrState{
		commandChan:        commands,
		targetKbps:         5_000,
		ceilingKbps:        20_000,
		lastEstimateUpdate: time.Now().Add(-abrEstimateInterval),
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

func TestABREstimateDoesNotRepeatClampCommands(t *testing.T) {
	floorCommands := make(chan []byte, 4)
	floor := &abrState{
		commandChan:        floorCommands,
		targetKbps:         abrMinKbps,
		lastEstimateUpdate: time.Now().Add(-abrEstimateInterval),
	}
	floor.onEstimate(500)
	if len(floorCommands) != 0 {
		t.Fatal("floor estimate repeated the current target")
	}

	ceilingCommands := make(chan []byte, 4)
	ceiling := &abrState{
		commandChan:        ceilingCommands,
		targetKbps:         10_000,
		ceilingKbps:        10_000,
		lastEstimateUpdate: time.Now().Add(-abrEstimateInterval),
	}
	ceiling.onEstimate(20_000)
	if len(ceilingCommands) != 0 {
		t.Fatal("ceiling estimate repeated the current target")
	}
}

func TestABRReportsDoNotChangeTarget(t *testing.T) {
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
