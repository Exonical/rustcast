package main

import (
	"testing"
	"time"
)

func TestDeliveryEstimatorReducesTargetForQueueDelay(t *testing.T) {
	base := time.Unix(1_700_000_000, 0)
	estimator := newDeliveryEstimator(10_000, base)
	for seq, delayMs := range []float64{100, 180, 180, 180} {
		sentAt := base.Add(time.Duration(seq) * 10 * time.Millisecond)
		estimator.onFrameSent(uint32(seq), 1000, sentAt)
		sentAtMs := float64(sentAt.Sub(base)) / float64(time.Millisecond)
		estimator.onFrameAck(uint32(seq), sentAtMs+delayMs, sentAt.Add(time.Duration(delayMs)*time.Millisecond))
	}
	if kbps, changed := estimator.update(base.Add(600 * time.Millisecond)); !changed || kbps != 8500 {
		t.Fatalf("queue-delay update = (%d, %t), want (8500, true)", kbps, changed)
	}
}

func TestDeliveryEstimatorReducesTargetForRTTInflation(t *testing.T) {
	base := time.Unix(1_700_000_000, 0)
	estimator := newDeliveryEstimator(10_000, base)
	estimator.onTransportStats(120*time.Millisecond, 20*time.Millisecond, 100, 0)
	if kbps, changed := estimator.update(base.Add(500 * time.Millisecond)); !changed || kbps != 8500 {
		t.Fatalf("RTT-inflation update = (%d, %t), want (8500, true)", kbps, changed)
	}
}

func TestDeliveryEstimatorReducesTargetForPacketLoss(t *testing.T) {
	base := time.Unix(1_700_000_000, 0)
	estimator := newDeliveryEstimator(10_000, base)
	estimator.onTransportStats(20*time.Millisecond, 20*time.Millisecond, 100, 0)
	estimator.onTransportStats(20*time.Millisecond, 20*time.Millisecond, 200, 10)
	if kbps, changed := estimator.update(base.Add(500 * time.Millisecond)); !changed || kbps != 8500 {
		t.Fatalf("loss update = (%d, %t), want (8500, true)", kbps, changed)
	}
}

func TestDeliveryEstimatorIncreasesAfterCleanInterval(t *testing.T) {
	base := time.Unix(1_700_000_000, 0)
	estimator := newDeliveryEstimator(10_000, base)
	if kbps, changed := estimator.update(base.Add(1500 * time.Millisecond)); changed || kbps != 10_000 {
		t.Fatalf("early clean update = (%d, %t), want (10000, false)", kbps, changed)
	}
	if kbps, changed := estimator.update(base.Add(2 * time.Second)); !changed || kbps != 11_200 {
		t.Fatalf("clean increase = (%d, %t), want (11200, true)", kbps, changed)
	}
}

func TestDeliveryEstimatorClampsTargets(t *testing.T) {
	if got := clampDeliveryTarget(1000, 10_000); got != abrMinKbps {
		t.Fatalf("floor clamp = %d, want %d", got, abrMinKbps)
	}
	if got := clampDeliveryTarget(5000, 3000); got != 3000 {
		t.Fatalf("ceiling clamp = %d, want 3000", got)
	}
	estimator := newDeliveryEstimator(10_000, time.Unix(1_700_000_000, 0))
	estimator.setCeilingKbps(8_000)
	if got, _ := estimator.update(estimator.start.Add(2 * time.Second)); got != 8_000 {
		t.Fatalf("target above sender ceiling = %d, want 8000", got)
	}
}

func TestDeliveryEstimatorDoesNotDecreaseTwiceWithin500ms(t *testing.T) {
	base := time.Unix(1_700_000_000, 0)
	estimator := newDeliveryEstimator(10_000, base)
	estimator.onTransportStats(120*time.Millisecond, 20*time.Millisecond, 100, 0)
	if kbps, changed := estimator.update(base.Add(500 * time.Millisecond)); !changed || kbps != 8500 {
		t.Fatalf("first decrease = (%d, %t), want (8500, true)", kbps, changed)
	}
	if kbps, changed := estimator.update(base.Add(900 * time.Millisecond)); changed || kbps != 8500 {
		t.Fatalf("second decrease = (%d, %t), want (8500, false)", kbps, changed)
	}
}

func TestDeliveryEstimatorIgnoresUnknownAndExpiredAcknowledgements(t *testing.T) {
	base := time.Unix(1_700_000_000, 0)
	estimator := newDeliveryEstimator(10_000, base)
	estimator.onFrameAck(7, 100, base.Add(100*time.Millisecond))
	estimator.onFrameSent(1, 1000, base)
	estimator.onFrameAck(1, 11_000, base.Add(11*time.Second))
	_, queue, _, _ := estimator.diagnostics()
	if queue != 0 || estimator.haveQueue {
		t.Fatalf("invalid acknowledgements changed queue estimate: queue=%v have=%t", queue, estimator.haveQueue)
	}
}
