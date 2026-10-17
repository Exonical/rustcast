package main

import (
	"math"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func scrapeMetrics(metrics *relayMetrics) string {
	recorder := httptest.NewRecorder()
	metrics.handler().ServeHTTP(recorder, httptest.NewRequest("GET", "/metrics", nil))
	return recorder.Body.String()
}

func TestRelayMetricsExposeSessionAndFrameMetrics(t *testing.T) {
	metrics := newRelayMetrics()
	machine := metrics.machine("host-1")
	machine.viewers.Set(1)
	machine.upstreamConnected.Set(1)
	machine.rttSmoothed.Set(0.01)
	machine.rttMin.Set(0.005)
	machine.packetLoss.Set(0.01)
	machine.queueDelay.Set(0.002)
	machine.bweTarget.Set(5000)
	machine.encoderTarget.Set(4500)
	machine.sessionsStarted.Inc()
	machine.upstreamFrames.Inc()
	machine.upstreamBytes.Add(42)
	machine.upstreamReconnects.Inc()
	machine.recordFrameWrite(true, 42, 20*time.Millisecond, 10*time.Millisecond, true, false)
	machine.framesReset.Inc()
	machine.framesDroppedStale.Inc()
	machine.recordIDR(idrReasonViewerRequest, true)
	machine.reject("bad_request")
	machine.timeToFirstFrame.Observe(0.1)
	if !machine.recordClientStats(clientStats{
		FPS: 60, DecodeMs: 4, ReceiveToPaintMs: 12, BitrateKbps: 5000,
		FramesDropped: 2, Decoder: "hardware", Browser: "chrome",
	}) {
		t.Fatal("valid client stats were rejected")
	}

	body := scrapeMetrics(metrics)
	for _, name := range []string{
		"flux_relay_viewers",
		"flux_relay_upstream_connected",
		"flux_relay_rtt_seconds",
		"flux_relay_packet_loss_ratio",
		"flux_relay_queue_delay_seconds",
		"flux_relay_bwe_target_kbps",
		"flux_relay_encoder_target_kbps",
		"flux_relay_sessions_started_total",
		"flux_relay_upstream_frames_total",
		"flux_relay_upstream_bytes_total",
		"flux_relay_upstream_reconnects_total",
		"flux_relay_frames_sent_total",
		"flux_relay_bytes_sent_total",
		"flux_relay_frames_reset_total",
		"flux_relay_frames_dropped_total",
		"flux_relay_idr_requests_total",
		"flux_relay_session_rejections_total",
		"flux_relay_frame_send_seconds",
		"flux_relay_queue_wait_seconds",
		"flux_relay_time_to_first_frame_seconds",
		"flux_client_fps",
		"flux_client_decode_seconds",
		"flux_client_receive_to_paint_seconds",
		"flux_client_bitrate_kbps",
		"flux_client_frames_dropped_total",
		"flux_client_info",
	} {
		if !strings.Contains(body, name) {
			t.Errorf("scrape missing %s", name)
		}
	}
	if !strings.Contains(body, `machine="host-1"`) {
		t.Fatal("scrape missing machine label")
	}
}

func TestClientStatsValidationAndEnumNormalization(t *testing.T) {
	for _, stats := range []clientStats{
		{FPS: math.NaN()},
		{DecodeMs: -1},
		{ReceiveToPaintMs: maxClientDelayMs + 1},
		{BitrateKbps: math.Inf(1)},
		{FramesDropped: maxClientDrops + 1},
		{FramesDropped: 1.5},
	} {
		if _, ok := validateClientStats(stats); ok {
			t.Errorf("accepted invalid stats: %+v", stats)
		}
	}

	stats, ok := validateClientStats(clientStats{
		FPS: 60, Decoder: "custom-decoder", Browser: "custom-browser",
	})
	if !ok || stats.Decoder != "unknown" || stats.Browser != "other" {
		t.Fatalf("unknown enums were not normalized: %+v, %t", stats, ok)
	}
	if _, ok := decodeClientStats([]byte(`{"fps":-1}`)); ok {
		t.Fatal("decoded negative client stats")
	}
}

func TestRelayMetricsCleanupRemovesClientInfoAndMachineSeries(t *testing.T) {
	metrics := newRelayMetrics()
	machine := metrics.machine("host-2")
	machine.recordClientStats(clientStats{
		FPS: 30, DecodeMs: 8, ReceiveToPaintMs: 15, BitrateKbps: 1000,
		Browser: "firefox", Decoder: "software",
	})
	session := &Session{metrics: machine, sessionDone: make(chan struct{})}
	session.releaseNow()
	body := scrapeMetrics(metrics)
	if strings.Contains(body, "flux_client_info{") {
		t.Fatalf("client info remained after release: %s", body)
	}
	if !strings.Contains(body, `flux_client_fps{machine="host-2"} 0`) {
		t.Fatalf("client gauges were not reset after release: %s", body)
	}
	metrics.deleteMachine("host-2")
	body = scrapeMetrics(metrics)
	if strings.Contains(body, `machine="host-2"`) {
		t.Fatalf("machine series remained after cleanup: %s", body)
	}
}
