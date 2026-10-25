package main

import (
	"encoding/json"
	"errors"
	"log"
	"math"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/collectors"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

const (
	defaultMetricsAddr = ":9101"
	maxClientFPS       = 1000
	maxClientDelayMs   = 60_000
	maxClientBitrate   = 10_000_000
	maxClientDrops     = 1_000_000_000
)

var (
	clientDecoderValues = map[string]struct{}{
		"hardware": {},
		"software": {},
		"unknown":  {},
	}
	clientBrowserValues = map[string]struct{}{
		"chrome":  {},
		"firefox": {},
		"safari":  {},
		"edge":    {},
		"other":   {},
	}
	clientDropReasonValues = map[string]struct{}{
		"not_painted":       {},
		"awaiting_keyframe": {},
		"decode_backlog":    {},
		"lost":              {},
		"decoder_error":     {},
	}
)

type relayMetrics struct {
	registry *prometheus.Registry

	viewers              *prometheus.GaugeVec
	upstreamConnected    *prometheus.GaugeVec
	rtt                  *prometheus.GaugeVec
	packetLoss           *prometheus.GaugeVec
	queueDelay           *prometheus.GaugeVec
	bweTarget            *prometheus.GaugeVec
	encoderTarget        *prometheus.GaugeVec
	upstreamFrames       *prometheus.CounterVec
	upstreamBytes        *prometheus.CounterVec
	upstreamAudioPackets *prometheus.CounterVec
	upstreamAudioBytes   *prometheus.CounterVec
	upstreamReconnects   *prometheus.CounterVec
	framesSent           *prometheus.CounterVec
	bytesSent            *prometheus.CounterVec
	framesReset          *prometheus.CounterVec
	framesDropped        *prometheus.CounterVec
	idrRequests          *prometheus.CounterVec
	sessionsStarted      *prometheus.CounterVec
	sessionRejections    *prometheus.CounterVec
	clipboardEvents      *prometheus.CounterVec
	frameSend            *prometheus.HistogramVec
	queueWait            *prometheus.HistogramVec
	timeToFirstFrame     *prometheus.HistogramVec
	clientFPS            *prometheus.GaugeVec
	clientDecode         *prometheus.GaugeVec
	clientReceivePaint   *prometheus.GaugeVec
	clientBitrate        *prometheus.GaugeVec
	clientFramesDropped  *prometheus.CounterVec
	clientInfo           *prometheus.GaugeVec
}

type machineMetrics struct {
	owner *relayMetrics
	id    string

	viewers              prometheus.Gauge
	upstreamConnected    prometheus.Gauge
	rttSmoothed          prometheus.Gauge
	rttMin               prometheus.Gauge
	packetLoss           prometheus.Gauge
	queueDelay           prometheus.Gauge
	bweTarget            prometheus.Gauge
	encoderTarget        prometheus.Gauge
	upstreamFrames       prometheus.Counter
	upstreamBytes        prometheus.Counter
	upstreamAudioPackets prometheus.Counter
	upstreamAudioBytes   prometheus.Counter
	upstreamReconnects   prometheus.Counter
	framesSentKey        prometheus.Counter
	framesSentDelta      prometheus.Counter
	bytesSent            prometheus.Counter
	framesReset          prometheus.Counter
	framesDroppedStale   prometheus.Counter
	framesDroppedIDR     prometheus.Counter
	idrGranted           map[idrReason]prometheus.Counter
	idrSuppressed        map[idrReason]prometheus.Counter
	sessionsStarted      prometheus.Counter
	rejections           map[string]prometheus.Counter
	frameSendKey         prometheus.Observer
	frameSendDelta       prometheus.Observer
	queueWait            prometheus.Observer
	timeToFirstFrame     prometheus.Observer
	clientFPS            prometheus.Gauge
	clientDecode         prometheus.Gauge
	clientReceivePaint   prometheus.Gauge
	clientBitrate        prometheus.Gauge

	clientMu      sync.Mutex
	clientBrowser string
	clientDecoder string
	clientInfo    prometheus.Gauge
}

type clientStats struct {
	FPS              float64            `json:"fps"`
	DecodeMs         float64            `json:"decodeMs"`
	ReceiveToPaintMs float64            `json:"receiveToPaintMs"`
	BitrateKbps      float64            `json:"bitrateKbps"`
	FramesDropped    float64            `json:"framesDropped"`
	DroppedByReason  map[string]float64 `json:"droppedByReason"`
	Decoder          string             `json:"decoder"`
	Browser          string             `json:"browser"`
}

func newRelayMetrics() *relayMetrics {
	m := &relayMetrics{
		registry:             prometheus.NewRegistry(),
		viewers:              prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_relay_viewers", Help: "Number of viewers attached to the machine upstream."}, []string{"machine"}),
		upstreamConnected:    prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_relay_upstream_connected", Help: "Whether the relay is connected to the host upstream."}, []string{"machine"}),
		rtt:                  prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_relay_rtt_seconds", Help: "Upstream WebTransport round-trip time."}, []string{"machine", "kind"}),
		packetLoss:           prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_relay_packet_loss_ratio", Help: "Observed packet loss ratio."}, []string{"machine"}),
		queueDelay:           prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_relay_queue_delay_seconds", Help: "Delivery estimator queue delay EWMA."}, []string{"machine"}),
		bweTarget:            prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_relay_bwe_target_kbps", Help: "Delivery-based bandwidth estimate target."}, []string{"machine"}),
		encoderTarget:        prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_relay_encoder_target_kbps", Help: "Encoder bitrate target sent to the host."}, []string{"machine"}),
		upstreamFrames:       prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_upstream_frames_total", Help: "Video frames received from the host."}, []string{"machine"}),
		upstreamBytes:        prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_upstream_bytes_total", Help: "Video bytes received from the host."}, []string{"machine"}),
		upstreamAudioPackets: prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_upstream_audio_packets_total", Help: "Audio packets received from the host."}, []string{"machine"}),
		upstreamAudioBytes:   prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_upstream_audio_bytes_total", Help: "Audio bytes received from the host."}, []string{"machine"}),
		upstreamReconnects:   prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_upstream_reconnects_total", Help: "Successful upstream connections after the first connection."}, []string{"machine"}),
		framesSent:           prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_frames_sent_total", Help: "Frames successfully sent to viewers."}, []string{"machine", "type"}),
		bytesSent:            prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_bytes_sent_total", Help: "Video bytes successfully sent to viewers."}, []string{"machine"}),
		framesReset:          prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_frames_reset_total", Help: "Video frame streams reset while writing."}, []string{"machine"}),
		framesDropped:        prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_frames_dropped_total", Help: "Frames dropped by the relay."}, []string{"machine", "reason"}),
		idrRequests:          prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_idr_requests_total", Help: "IDR requests by reason and gate result."}, []string{"machine", "reason", "result"}),
		sessionsStarted:      prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_sessions_started_total", Help: "Viewer sessions accepted for a machine."}, []string{"machine"}),
		sessionRejections:    prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_session_rejections_total", Help: "WebTransport sessions rejected by the relay."}, []string{"machine", "reason"}),
		clipboardEvents:      prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_relay_clipboard_events_total", Help: "Clipboard events by direction and enforcement result."}, []string{"machine", "direction", "result"}),
		frameSend:            prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "flux_relay_frame_send_seconds", Help: "Time spent sending a video frame stream.", Buckets: prometheus.ExponentialBuckets(0.001, 2, 12)}, []string{"machine", "type"}),
		queueWait:            prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "flux_relay_queue_wait_seconds", Help: "Time a video frame waited in the relay queue.", Buckets: prometheus.ExponentialBuckets(0.001, 2, 12)}, []string{"machine"}),
		timeToFirstFrame:     prometheus.NewHistogramVec(prometheus.HistogramOpts{Name: "flux_relay_time_to_first_frame_seconds", Help: "Time from session acceptance until the first frame was written.", Buckets: prometheus.ExponentialBuckets(0.05, 2, 10)}, []string{"machine"}),
		clientFPS:            prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_client_fps", Help: "Recent browser-rendered frames per second."}, []string{"machine"}),
		clientDecode:         prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_client_decode_seconds", Help: "Recent browser video decode time."}, []string{"machine"}),
		clientReceivePaint:   prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_client_receive_to_paint_seconds", Help: "Recent browser receive-to-paint latency."}, []string{"machine"}),
		clientBitrate:        prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_client_bitrate_kbps", Help: "Recent browser-received video bitrate."}, []string{"machine"}),
		clientFramesDropped:  prometheus.NewCounterVec(prometheus.CounterOpts{Name: "flux_client_frames_dropped_total", Help: "Frames dropped by the browser, by reason."}, []string{"machine", "reason"}),
		clientInfo:           prometheus.NewGaugeVec(prometheus.GaugeOpts{Name: "flux_client_info", Help: "Current browser and decoder information."}, []string{"machine", "browser", "decoder"}),
	}
	m.registry.MustRegister(
		collectors.NewGoCollector(),
		collectors.NewProcessCollector(collectors.ProcessCollectorOpts{}),
		m.viewers, m.upstreamConnected, m.rtt, m.packetLoss, m.queueDelay, m.bweTarget, m.encoderTarget,
		m.upstreamFrames, m.upstreamBytes, m.upstreamAudioPackets, m.upstreamAudioBytes,
		m.upstreamReconnects, m.framesSent, m.bytesSent, m.framesReset,
		m.framesDropped, m.idrRequests, m.sessionsStarted, m.sessionRejections, m.clipboardEvents, m.frameSend, m.queueWait,
		m.timeToFirstFrame, m.clientFPS, m.clientDecode, m.clientReceivePaint, m.clientBitrate,
		m.clientFramesDropped, m.clientInfo,
	)
	return m
}

func (m *relayMetrics) handler() http.Handler {
	return promhttp.HandlerFor(m.registry, promhttp.HandlerOpts{})
}

func (m *relayMetrics) machine(id string) *machineMetrics {
	metrics := &machineMetrics{
		owner:                m,
		id:                   id,
		viewers:              m.viewers.WithLabelValues(id),
		upstreamConnected:    m.upstreamConnected.WithLabelValues(id),
		rttSmoothed:          m.rtt.WithLabelValues(id, "smoothed"),
		rttMin:               m.rtt.WithLabelValues(id, "min"),
		packetLoss:           m.packetLoss.WithLabelValues(id),
		queueDelay:           m.queueDelay.WithLabelValues(id),
		bweTarget:            m.bweTarget.WithLabelValues(id),
		encoderTarget:        m.encoderTarget.WithLabelValues(id),
		upstreamFrames:       m.upstreamFrames.WithLabelValues(id),
		upstreamBytes:        m.upstreamBytes.WithLabelValues(id),
		upstreamAudioPackets: m.upstreamAudioPackets.WithLabelValues(id),
		upstreamAudioBytes:   m.upstreamAudioBytes.WithLabelValues(id),
		upstreamReconnects:   m.upstreamReconnects.WithLabelValues(id),
		framesSentKey:        m.framesSent.WithLabelValues(id, "key"),
		framesSentDelta:      m.framesSent.WithLabelValues(id, "delta"),
		bytesSent:            m.bytesSent.WithLabelValues(id),
		framesReset:          m.framesReset.WithLabelValues(id),
		framesDroppedStale:   m.framesDropped.WithLabelValues(id, "stale_queue"),
		framesDroppedIDR:     m.framesDropped.WithLabelValues(id, "awaiting_idr"),
		sessionsStarted:      m.sessionsStarted.WithLabelValues(id),
		idrGranted:           make(map[idrReason]prometheus.Counter),
		idrSuppressed:        make(map[idrReason]prometheus.Counter),
		rejections:           make(map[string]prometheus.Counter),
		frameSendKey:         m.frameSend.WithLabelValues(id, "key"),
		frameSendDelta:       m.frameSend.WithLabelValues(id, "delta"),
		queueWait:            m.queueWait.WithLabelValues(id),
		timeToFirstFrame:     m.timeToFirstFrame.WithLabelValues(id),
		clientFPS:            m.clientFPS.WithLabelValues(id),
		clientDecode:         m.clientDecode.WithLabelValues(id),
		clientReceivePaint:   m.clientReceivePaint.WithLabelValues(id),
		clientBitrate:        m.clientBitrate.WithLabelValues(id),
	}
	for _, reason := range []idrReason{
		idrReasonViewerRequest, idrReasonUpstreamDrop, idrReasonStaleQueue, idrReasonAwaitingIDR,
	} {
		metrics.idrGranted[reason] = m.idrRequests.WithLabelValues(id, string(reason), "granted")
		metrics.idrSuppressed[reason] = m.idrRequests.WithLabelValues(id, string(reason), "suppressed")
	}
	for _, reason := range []string{"machine_in_use", "not_found", "offline", "bad_request", "no_control_stream"} {
		metrics.rejections[reason] = m.sessionRejections.WithLabelValues(id, reason)
	}
	return metrics
}

func (m *relayMetrics) reject(machine, reason string) {
	m.machine(machine).reject(reason)
}

func (m *relayMetrics) deleteMachine(id string) {
	m.deleteMachineVectors(id, true)
}

// deleteUpstreamMachine clears per-machine vectors when an upstream connection
// drops. Clipboard event counters are kept: they record audited history, not
// live connection state, and must survive upstream reconnects.
func (m *relayMetrics) deleteUpstreamMachine(id string) {
	m.deleteMachineVectors(id, false)
}

func (m *relayMetrics) deleteMachineVectors(id string, includeClipboard bool) {
	labels := prometheus.Labels{"machine": id}
	vectors := []interface{ DeletePartialMatch(prometheus.Labels) int }{
		m.viewers, m.upstreamConnected, m.rtt, m.packetLoss, m.queueDelay, m.bweTarget, m.encoderTarget,
		m.upstreamFrames, m.upstreamBytes, m.upstreamAudioPackets, m.upstreamAudioBytes,
		m.upstreamReconnects, m.framesSent, m.bytesSent, m.framesReset,
		m.framesDropped, m.idrRequests, m.sessionsStarted, m.sessionRejections, m.frameSend, m.queueWait,
		m.timeToFirstFrame, m.clientFPS, m.clientDecode, m.clientReceivePaint, m.clientBitrate,
		m.clientFramesDropped, m.clientInfo,
	}
	if includeClipboard {
		vectors = append(vectors, m.clipboardEvents)
	}
	for _, vector := range vectors {
		vector.DeletePartialMatch(labels)
	}
}

func serveMetrics(addr string, metrics *relayMetrics) {
	if addr == "" {
		addr = defaultMetricsAddr
	}
	addr = strings.TrimSpace(addr)
	if addr == "" {
		addr = defaultMetricsAddr
	}
	if strings.EqualFold(addr, "off") {
		return
	}
	go func() {
		listener, err := net.Listen("tcp", addr)
		if err != nil {
			logMetricsError(addr, err)
			return
		}
		if err := http.Serve(listener, metrics.handler()); err != nil && !errors.Is(err, http.ErrServerClosed) {
			logMetricsError(addr, err)
		}
	}()
}

func logMetricsError(addr string, err error) {
	log.Printf("[metrics] listener %s stopped: %v", addr, err)
}

func validateClientStats(stats clientStats) (clientStats, bool) {
	if !validMetricValue(stats.FPS, maxClientFPS) ||
		!validMetricValue(stats.DecodeMs, maxClientDelayMs) ||
		!validMetricValue(stats.ReceiveToPaintMs, maxClientDelayMs) ||
		!validMetricValue(stats.BitrateKbps, maxClientBitrate) ||
		!validMetricValue(stats.FramesDropped, maxClientDrops) ||
		math.Trunc(stats.FramesDropped) != stats.FramesDropped {
		return clientStats{}, false
	}
	var reasonTotal float64
	for reason, value := range stats.DroppedByReason {
		if _, known := clientDropReasonValues[reason]; !known {
			continue
		}
		if !validMetricValue(value, maxClientDrops) || math.Trunc(value) != value {
			return clientStats{}, false
		}
		reasonTotal += value
	}
	if reasonTotal > stats.FramesDropped {
		return clientStats{}, false
	}
	if _, ok := clientDecoderValues[stats.Decoder]; !ok {
		stats.Decoder = "unknown"
	}
	if _, ok := clientBrowserValues[stats.Browser]; !ok {
		stats.Browser = "other"
	}
	return stats, true
}

func validMetricValue(value float64, max float64) bool {
	return !math.IsNaN(value) && !math.IsInf(value, 0) && value >= 0 && value <= max
}

func (m *machineMetrics) recordClientStats(stats clientStats) bool {
	stats, ok := validateClientStats(stats)
	if !ok {
		return false
	}
	m.clientFPS.Set(stats.FPS)
	m.clientDecode.Set(stats.DecodeMs / 1000)
	m.clientReceivePaint.Set(stats.ReceiveToPaintMs / 1000)
	m.clientBitrate.Set(stats.BitrateKbps)
	var reasonTotal float64
	for reason := range clientDropReasonValues {
		count := stats.DroppedByReason[reason]
		m.owner.clientFramesDropped.WithLabelValues(m.id, reason).Add(count)
		reasonTotal += count
	}
	if residual := stats.FramesDropped - reasonTotal; residual > 0 {
		m.owner.clientFramesDropped.WithLabelValues(m.id, "unknown").Add(residual)
	}

	m.clientMu.Lock()
	defer m.clientMu.Unlock()
	if m.clientInfo != nil && (m.clientBrowser != stats.Browser || m.clientDecoder != stats.Decoder) {
		m.owner.clientInfo.DeleteLabelValues(m.id, m.clientBrowser, m.clientDecoder)
		m.clientInfo = nil
	}
	if m.clientInfo == nil {
		m.clientInfo = m.owner.clientInfo.WithLabelValues(m.id, stats.Browser, stats.Decoder)
		m.clientBrowser, m.clientDecoder = stats.Browser, stats.Decoder
	}
	m.clientInfo.Set(1)
	return true
}

func (m *machineMetrics) recordIDR(reason idrReason, granted bool) {
	if granted {
		if counter := m.idrGranted[reason]; counter != nil {
			counter.Inc()
		}
		return
	}
	if counter := m.idrSuppressed[reason]; counter != nil {
		counter.Inc()
	}
}

func (m *machineMetrics) reject(reason string) {
	if counter := m.rejections[reason]; counter != nil {
		counter.Inc()
	}
}

func (m *machineMetrics) recordFrameWrite(keyframe bool, bytes int, elapsed, queueWait time.Duration, sent, reset bool) {
	frameSend := m.frameSendDelta
	framesSent := m.framesSentDelta
	if keyframe {
		frameSend = m.frameSendKey
		framesSent = m.framesSentKey
	}
	frameSend.Observe(elapsed.Seconds())
	if queueWait >= 0 {
		m.queueWait.Observe(queueWait.Seconds())
	}
	if reset {
		m.framesReset.Inc()
	}
	if sent {
		framesSent.Inc()
		m.bytesSent.Add(float64(bytes))
	}
}

func (m *machineMetrics) resetClientStats() {
	m.clientFPS.Set(0)
	m.clientDecode.Set(0)
	m.clientReceivePaint.Set(0)
	m.clientBitrate.Set(0)
	m.clientMu.Lock()
	if m.clientInfo != nil {
		m.owner.clientInfo.DeleteLabelValues(m.id, m.clientBrowser, m.clientDecoder)
		m.clientInfo = nil
		m.clientBrowser = ""
		m.clientDecoder = ""
	}
	m.clientMu.Unlock()
}

func decodeClientStats(payload json.RawMessage) (clientStats, bool) {
	var stats clientStats
	if err := json.Unmarshal(payload, &stats); err != nil {
		return clientStats{}, false
	}
	return validateClientStats(stats)
}
