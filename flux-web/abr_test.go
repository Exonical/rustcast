package main

import (
	"encoding/binary"
	"testing"
	"time"
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
