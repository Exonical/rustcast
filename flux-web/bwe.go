package main

import (
	"math"
	"sync"
	"time"
)

const (
	bweDecreaseFactor = 0.85
	bweIncreaseFactor = 1.12
	bweQueueAlpha     = 0.25
	bweQueueThreshold = 30 * time.Millisecond
	bweRTTThreshold   = 50 * time.Millisecond
	bweLossThreshold  = 0.05
	bweUpdateInterval = 500 * time.Millisecond
	bweDelayWindow    = 10 * time.Second
	bweMaxOutstanding = 256
)

type sentFrame struct {
	at    time.Time
	bytes int
}

type sentOrderEntry struct {
	seq uint32
	at  time.Time
}

type delaySample struct {
	at      time.Time
	delayMs float64
}

type deliveryEstimator struct {
	mu sync.Mutex

	start            time.Time
	targetKbps       uint32
	ceilingKbps      uint32
	lastUpdate       time.Time
	lastChange       time.Time
	suppressIncrease time.Time

	outstanding  map[uint32]sentFrame
	sendOrder    []sentOrderEntry
	delaySamples []delaySample
	queueEWMA    time.Duration
	haveQueue    bool
	lastSampleAt time.Time

	smoothedRTT    time.Duration
	minRTT         time.Duration
	packetsSent    uint64
	packetsLost    uint64
	haveStats      bool
	windowSent     uint64
	windowLost     uint64
	lossRatioValue float64
}

func newDeliveryEstimator(initialKbps uint32, now time.Time) *deliveryEstimator {
	if initialKbps == 0 {
		initialKbps = abrInitialBitrateKbps
	}
	return &deliveryEstimator{
		start:       now,
		targetKbps:  initialKbps,
		ceilingKbps: abrMaxBitrateKbps,
		lastUpdate:  now,
		lastChange:  now,
		outstanding: make(map[uint32]sentFrame),
	}
}

func (e *deliveryEstimator) onFrameSent(seq uint32, bytes int, at time.Time) {
	e.mu.Lock()
	defer e.mu.Unlock()
	kept := e.sendOrder[:0]
	for _, item := range e.sendOrder {
		sent, ok := e.outstanding[item.seq]
		if ok && sent.at.Equal(item.at) && at.Sub(item.at) <= bweDelayWindow {
			kept = append(kept, item)
		} else if ok && sent.at.Equal(item.at) {
			delete(e.outstanding, item.seq)
		}
	}
	e.sendOrder = kept
	delete(e.outstanding, seq)
	e.outstanding[seq] = sentFrame{
		at:    at,
		bytes: bytes,
	}
	e.sendOrder = append(e.sendOrder, sentOrderEntry{seq: seq, at: at})
	for len(e.outstanding) > bweMaxOutstanding && len(e.sendOrder) > 0 {
		oldest := e.sendOrder[0]
		e.sendOrder = e.sendOrder[1:]
		if current, ok := e.outstanding[oldest.seq]; ok && current.at.Equal(oldest.at) {
			delete(e.outstanding, oldest.seq)
		}
	}
}

func (e *deliveryEstimator) onFrameAck(seq uint32, clientDoneMs float64, at time.Time) {
	e.mu.Lock()
	defer e.mu.Unlock()
	sent, ok := e.outstanding[seq]
	if !ok {
		return
	}
	delete(e.outstanding, seq)
	if at.Before(sent.at) || at.Sub(sent.at) > bweDelayWindow ||
		math.IsNaN(clientDoneMs) || math.IsInf(clientDoneMs, 0) || clientDoneMs < 0 {
		return
	}
	delayMs := float64(at.Sub(sent.at)) / float64(time.Millisecond)
	e.delaySamples = append(e.delaySamples, delaySample{
		at:      at,
		delayMs: delayMs,
	})
	e.lastSampleAt = at
	first := 0
	baseline := math.Inf(1)
	for _, sample := range e.delaySamples {
		if at.Sub(sample.at) <= bweDelayWindow {
			if sample.delayMs < baseline {
				baseline = sample.delayMs
			}
		} else {
			first++
		}
	}
	if first > 0 {
		e.delaySamples = append([]delaySample(nil), e.delaySamples[first:]...)
	}
	queueMs := math.Max(0, delayMs-baseline)
	queueDelay := time.Duration(queueMs * float64(time.Millisecond))
	if !e.haveQueue {
		e.queueEWMA = queueDelay
		e.haveQueue = true
	} else {
		e.queueEWMA = time.Duration((1-bweQueueAlpha)*float64(e.queueEWMA) + bweQueueAlpha*float64(queueDelay))
	}
}

func (e *deliveryEstimator) forget(seq uint32) {
	e.mu.Lock()
	defer e.mu.Unlock()
	delete(e.outstanding, seq)
	kept := e.sendOrder[:0]
	for _, item := range e.sendOrder {
		if item.seq != seq {
			kept = append(kept, item)
		}
	}
	e.sendOrder = kept
}

func (e *deliveryEstimator) onTransportStats(smoothedRTT, minRTT time.Duration, packetsSent, packetsLost uint64) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.smoothedRTT = smoothedRTT
	e.minRTT = minRTT
	if e.haveStats {
		if packetsSent >= e.packetsSent {
			e.windowSent += packetsSent - e.packetsSent
		}
		if packetsLost >= e.packetsLost {
			e.windowLost += packetsLost - e.packetsLost
		}
	}
	e.packetsSent = packetsSent
	e.packetsLost = packetsLost
	e.haveStats = true
}

func (e *deliveryEstimator) setCeilingKbps(ceiling uint32) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if ceiling == 0 {
		ceiling = abrMaxBitrateKbps
	}
	if ceiling < abrMinKbps {
		ceiling = abrMinKbps
	}
	e.ceilingKbps = ceiling
	e.targetKbps = clampDeliveryTarget(e.targetKbps, ceiling)
}

func (e *deliveryEstimator) update(now time.Time) (uint32, bool) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if now.Sub(e.lastUpdate) < bweUpdateInterval {
		return e.targetKbps, false
	}
	e.lastUpdate = now
	if e.windowSent > 0 {
		e.lossRatioValue = float64(e.windowLost) / float64(e.windowSent)
	} else {
		e.lossRatioValue = 0
	}
	e.windowSent, e.windowLost = 0, 0
	if e.haveQueue && now.Sub(e.lastSampleAt) > time.Second {
		e.haveQueue = false
		e.queueEWMA = 0
	}
	rttInflation := e.smoothedRTT - e.minRTT
	congested := e.haveQueue && e.queueEWMA > bweQueueThreshold ||
		rttInflation > bweRTTThreshold ||
		e.lossRatioValue > bweLossThreshold
	if congested {
		e.suppressIncrease = now.Add(time.Second)
		if now.Sub(e.lastChange) < bweUpdateInterval {
			return e.targetKbps, false
		}
		next := uint32(math.Round(float64(e.targetKbps) * bweDecreaseFactor))
		next = clampDeliveryTarget(next, e.ceilingKbps)
		if next == e.targetKbps {
			return e.targetKbps, false
		}
		e.targetKbps = next
		e.lastChange = now
		return e.targetKbps, true
	}
	if now.Sub(e.lastChange) < 2*time.Second || now.Before(e.suppressIncrease) {
		return e.targetKbps, false
	}
	next := uint32(math.Round(float64(e.targetKbps) * bweIncreaseFactor))
	next = clampDeliveryTarget(next, e.ceilingKbps)
	if next == e.targetKbps {
		return e.targetKbps, false
	}
	e.targetKbps = next
	e.lastChange = now
	return e.targetKbps, true
}

func (e *deliveryEstimator) lossRatio() float64 {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.lossRatioValue
}

func (e *deliveryEstimator) diagnostics() (uint32, time.Duration, time.Duration, float64) {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.targetKbps, e.queueEWMA, e.smoothedRTT - e.minRTT, e.lossRatioValue
}

func clampDeliveryTarget(kbps, ceiling uint32) uint32 {
	if kbps < abrMinKbps {
		kbps = abrMinKbps
	}
	if ceiling == 0 || ceiling > abrMaxBitrateKbps {
		ceiling = abrMaxBitrateKbps
	}
	if ceiling < abrMinKbps {
		ceiling = abrMinKbps
	}
	if kbps > ceiling {
		kbps = ceiling
	}
	return kbps
}
