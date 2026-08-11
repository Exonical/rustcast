package main

import (
	"encoding/binary"
	"log"
	"sync"
	"time"
)

// abrState tracks the upstream encoder bitrate target for one machine. GCC's
// transport-wide congestion estimate drives the target; RTCP receiver reports
// and RTT samples remain here for diagnostics. Targets are sent upstream as
// command 0x03 [4-byte BE kbps], which flux-server applies live via the
// encoder's set_bitrate.
type abrState struct {
	mu          sync.Mutex
	commandChan chan []byte

	bytesReceived uint64    // video bytes since last sample
	sampleStart   time.Time // start of current measurement window
	measuredKbps  uint32    // last measured incoming bitrate

	targetKbps  uint32 // 0 = never adjusted (encoder default)
	ceilingKbps uint32 // sender-reported requested bitrate ceiling

	baseRTT       time.Duration // lowest RTT in the trailing window: the path's uninflated floor
	smoothedRTT   time.Duration // EWMA of recent RTT samples
	windowMinRTT  time.Duration // lowest RTT in the window currently being collected
	windowStarted time.Time
	lastGCCUpdate time.Time
	lossNotable   bool
	rttInflated   bool
}

const (
	abrMinKbps       = 1500
	abrLossThreshold = 0.05 // fraction lost worth recording in the relay log

	abrRTTInflation  = 75 * time.Millisecond // queue delay above baseline worth recording
	abrRTTAlpha      = 0.125                 // EWMA weight for new RTT samples
	abrRTTBaseWindow = 30 * time.Second      // baseline is re-estimated over this trailing window

	abrGCCInitialBitrateBps = 5_000_000
	abrGCCMaxBitrateBps     = 50_000_000
	abrGCCMinChange         = 0.10
	abrGCCUpdateInterval    = 500 * time.Millisecond
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
	if kbps == 0 || a.targetKbps <= kbps {
		return
	}
	a.targetKbps = kbps
	log.Printf("[abr] sender ceiling lowered → clamping encoder bitrate to %d kbps", kbps)
	a.sendBitrateCommand(kbps)
}

func (a *abrState) targetBitrateKbps() uint32 {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.targetKbps
}

// onRTTSample records a round-trip-time measurement derived from RTCP receiver
// reports. GCC owns bitrate changes; these samples remain for diagnostics.
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
	inflated := a.smoothedRTT-a.baseRTT > abrRTTInflation
	if inflated == a.rttInflated {
		return
	}
	a.rttInflated = inflated
	if inflated {
		log.Printf("[abr] RTT %.0fms (baseline %.0fms) → queue inflation observed; GCC owns bitrate",
			float64(a.smoothedRTT)/float64(time.Millisecond),
			float64(a.baseRTT)/float64(time.Millisecond))
	} else {
		log.Printf("[abr] RTT queue inflation cleared; GCC owns bitrate")
	}
}

// onReceiverReport records loss feedback from the viewer. GCC owns bitrate
// changes; this remains so relay logs can distinguish loss from queueing.
func (a *abrState) onReceiverReport(fractionLost float64) {
	a.mu.Lock()
	defer a.mu.Unlock()
	notable := fractionLost > abrLossThreshold
	if notable == a.lossNotable {
		return
	}
	a.lossNotable = notable
	if notable {
		log.Printf("[abr] loss %.1f%% observed; GCC owns bitrate", fractionLost*100)
	} else {
		log.Printf("[abr] loss cleared; GCC owns bitrate")
	}
}

// onEstimate applies a material GCC estimate to the encoder target.
func (a *abrState) onEstimate(kbps uint32) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if kbps == 0 {
		return
	}
	if kbps < abrMinKbps {
		kbps = abrMinKbps
	}
	if a.ceilingKbps != 0 && kbps > a.ceilingKbps {
		kbps = a.ceilingKbps
	}
	now := time.Now()
	if !a.lastGCCUpdate.IsZero() && now.Sub(a.lastGCCUpdate) < abrGCCUpdateInterval {
		return
	}
	if a.targetKbps != 0 && !gccEstimateIsMaterial(a.targetKbps, kbps, a.ceilingKbps) {
		return
	}
	a.targetKbps = kbps
	a.lastGCCUpdate = now
	log.Printf("[abr] GCC estimate → encoder bitrate %d kbps", kbps)
	a.sendBitrateCommand(kbps)
}

func gccEstimateIsMaterial(current, next, ceiling uint32) bool {
	if next == abrMinKbps || ceiling != 0 && next == ceiling {
		return true
	}
	if current == 0 {
		return true
	}
	delta := current - next
	if next > current {
		delta = next - current
	}
	return float64(delta)/float64(current) >= abrGCCMinChange
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
