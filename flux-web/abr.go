package main

import (
	"encoding/binary"
	"log"
	"sync"
	"time"
)

// abrState adapts the upstream encoder bitrate for one machine to what its viewer's link actually
// sustains. It watches RTCP receiver reports from the browser: sustained loss
// means the WiFi link is saturated, so the encoder target is cut (multiplicative
// decrease); after a clean period it is raised back gradually (additive
// increase) toward the highest rate ever observed working. Targets are sent
// upstream as command 0x03 [4-byte BE kbps], which flux-server applies live
// via the encoder's set_bitrate.
type abrState struct {
	mu          sync.Mutex
	commandChan chan []byte

	bytesReceived uint64    // video bytes since last sample
	sampleStart   time.Time // start of current measurement window
	measuredKbps  uint32    // last measured incoming bitrate

	targetKbps       uint32 // 0 = never adjusted (encoder default)
	ceilingKbps      uint32 // sender-reported requested bitrate ceiling
	senderTargetKbps uint32

	lastDecrease time.Time
	lastIncrease time.Time
	cleanSince   time.Time

	baseRTT       time.Duration // lowest RTT in the trailing window: the path's uninflated floor
	smoothedRTT   time.Duration // EWMA of recent RTT samples
	windowMinRTT  time.Duration // lowest RTT in the window currently being collected
	windowStarted time.Time
}

const (
	abrMinKbps          = 1500
	abrLossThreshold    = 0.05 // fraction lost that triggers a decrease
	abrCleanThreshold   = 0.01 // fraction lost considered "clean"
	abrDecreaseFactor   = 0.7
	abrIncreaseFactor   = 1.15
	abrDecreaseCooldown = 2 * time.Second
	abrIncreaseCooldown = 5 * time.Second
	abrCleanBeforeRaise = 5 * time.Second

	abrRTTInflation  = 75 * time.Millisecond // queue delay above baseline that triggers a decrease
	abrRTTAlpha      = 0.125                 // EWMA weight for new RTT samples
	abrRTTBaseWindow = 30 * time.Second      // baseline is re-estimated over this trailing window
)

// countFrameBytes records incoming video bytes for bitrate measurement.
func (a *abrState) countFrameBytes(n int) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now()
	if a.sampleStart.IsZero() {
		a.sampleStart = now
	}
	a.bytesReceived += uint64(n)
	if elapsed := now.Sub(a.sampleStart); elapsed >= time.Second {
		a.measuredKbps = uint32(float64(a.bytesReceived*8) / elapsed.Seconds() / 1000)
		a.bytesReceived = 0
		a.sampleStart = now
	}
}

// setSenderTarget updates capacity from the sender's requested bitrate.
// Measured output is intentionally not used as capacity: an idle desktop can
// emit almost no bytes while still supporting the full requested bitrate.
func (a *abrState) setSenderTarget(kbps uint32) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.ceilingKbps = kbps
	if a.senderTargetKbps != kbps {
		a.senderTargetKbps = kbps
		a.targetKbps = kbps
	}
}

// onRTTSample feeds a round-trip-time measurement derived from RTCP
// receiver reports. Rising RTT means packets are queueing (bufferbloat) —
// the link is saturating even though nothing is being lost yet — so the
// bitrate is cut before loss appears. Inflation is measured against a
// baseline re-estimated over a trailing window.
func (a *abrState) onRTTSample(rtt time.Duration) {
	if rtt <= 0 {
		return
	}
	a.mu.Lock()
	defer a.mu.Unlock()

	// The baseline tracks the minimum over a trailing window rather than the
	// session minimum: a path that genuinely gets slower (moving from Ethernet
	// to WiFi) would otherwise look permanently inflated and pin the bitrate
	// at the floor forever.
	now := time.Now()
	if a.windowStarted.IsZero() {
		a.windowStarted = now
	}
	if a.windowMinRTT == 0 || rtt < a.windowMinRTT {
		a.windowMinRTT = rtt
	}
	if a.baseRTT == 0 || rtt < a.baseRTT {
		a.baseRTT = rtt
	} else if now.Sub(a.windowStarted) >= abrRTTBaseWindow {
		a.baseRTT = a.windowMinRTT
		a.windowMinRTT = 0
		a.windowStarted = now
	}

	if a.smoothedRTT == 0 {
		a.smoothedRTT = rtt
	} else {
		a.smoothedRTT = time.Duration((1-abrRTTAlpha)*float64(a.smoothedRTT) + abrRTTAlpha*float64(rtt))
	}
	if a.smoothedRTT-a.baseRTT <= abrRTTInflation {
		return
	}
	a.cleanSince = time.Time{}
	if now.Sub(a.lastDecrease) < abrDecreaseCooldown {
		return
	}
	base := a.targetKbps
	if base == 0 {
		return
	}
	target := uint32(float64(base) * abrDecreaseFactor)
	if target < abrMinKbps {
		target = abrMinKbps
	}
	if target == a.targetKbps {
		return
	}
	a.targetKbps = target
	a.lastDecrease = now
	log.Printf("[abr] RTT %.0fms (baseline %.0fms) → lowering encoder bitrate to %d kbps",
		float64(a.smoothedRTT)/float64(time.Millisecond),
		float64(a.baseRTT)/float64(time.Millisecond), target)
	a.sendBitrateCommand(target)
}

// onReceiverReport reacts to loss feedback from the viewer.
// fractionLost is the RFC 3550 fraction (0..255 scaled to 0..1).
func (a *abrState) onReceiverReport(fractionLost float64) {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now()

	if fractionLost > abrLossThreshold {
		a.cleanSince = time.Time{}
		if now.Sub(a.lastDecrease) < abrDecreaseCooldown {
			return
		}
		base := a.targetKbps
		if base == 0 {
			return // nothing measured yet
		}
		target := uint32(float64(base) * abrDecreaseFactor)
		if target < abrMinKbps {
			target = abrMinKbps
		}
		if target == a.targetKbps {
			return
		}
		a.targetKbps = target
		a.lastDecrease = now
		log.Printf("[abr] loss %.1f%% → lowering encoder bitrate to %d kbps", fractionLost*100, target)
		a.sendBitrateCommand(target)
		return
	}

	if fractionLost <= abrCleanThreshold {
		if a.cleanSince.IsZero() {
			a.cleanSince = now
		}
		// Only raise if we previously lowered, the link has been clean,
		// and RTT is not sitting above its baseline (queues still draining).
		if a.smoothedRTT-a.baseRTT > abrRTTInflation {
			return
		}
		if a.targetKbps == 0 ||
			a.targetKbps >= a.ceilingKbps ||
			now.Sub(a.cleanSince) < abrCleanBeforeRaise ||
			now.Sub(a.lastIncrease) < abrIncreaseCooldown {
			return
		}
		target := uint32(float64(a.targetKbps) * abrIncreaseFactor)
		if target > a.ceilingKbps {
			target = a.ceilingKbps
		}
		a.targetKbps = target
		a.lastIncrease = now
		log.Printf("[abr] link clean → raising encoder bitrate to %d kbps", target)
		a.sendBitrateCommand(target)
	}
}

func (a *abrState) sendBitrateCommand(kbps uint32) {
	cmd := make([]byte, 5)
	cmd[0] = 0x03
	binary.BigEndian.PutUint32(cmd[1:], kbps)
	select {
	case a.commandChan <- cmd:
	default:
	}
}
