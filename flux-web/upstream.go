package main

import (
	"context"
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
	"github.com/pion/rtp"
	"github.com/quic-go/quic-go"
)

// ---------------------------------------------------------------------------
// TCP/QUIC frame reader — connects to flux-server's frame server
// ---------------------------------------------------------------------------

type machineUpstream struct {
	id          string
	addr        string
	frameChan   chan frameMsg
	cursorChan  chan cursorMsg
	commandChan chan []byte
	abr         *abrState
	stopChan    chan struct{}
	stopOnce    sync.Once
	viewers     int // Protected by machineRegistry.mu.
	viewerCount atomic.Uint32
	mu          sync.Mutex
	session     *Session
	lastCursor  *cursorMsg
	conn        net.Conn
	cancel      context.CancelFunc
	status      func(string)
	idrGate     idrRequestGate
	idrStats    idrStats
}

const (
	defaultFrameDuration    = 16 * time.Millisecond // ~60fps for the first sample
	minFrameDuration        = 4 * time.Millisecond  // clamp absurdly fast bursts
	maxSaneFrameDuration    = 30 * time.Second      // cap corrupted/absurd timestamps
	pacingMultiplier        = 2                     // smooth bursts at 2x target bitrate
	maxPacingMultiplier     = 4                     // cap frame-size floor at 4x target
	pacingIDRTargetEmission = 40 * time.Millisecond // emit a keyframe within ~2 frame intervals
	pacingIDRMaxRate        = 10_000_000            // bits/s ceiling so that bound can't become a burst
	idrRequestInterval      = 2 * time.Second
	stageStatsInterval      = 5 * time.Second // how often per-stage timings are logged
)

// Why a keyframe was asked for. Every request path is tagged so the logs show
// whether keyframes are driven by the viewer's decoder (real loss on the path)
// or by the relay's own drop paths (the stream not keeping up locally) — the two
// call for opposite fixes.
type idrReason string

const (
	idrReasonViewerPLI    idrReason = "viewer-pli"
	idrReasonUpstreamDrop idrReason = "upstream-queue-full"
	idrReasonStaleQueue   idrReason = "stale-queue-discard"
	idrReasonAbandoned    idrReason = "abandoned-packets"
	idrReasonRequeueDrop  idrReason = "requeue-drop"
)

// idrStats counts keyframe requests by origin, including the ones the gate
// suppressed: a high suppressed count means the stream is still asking for
// keyframes far faster than it can send them.
type idrStats struct {
	mu         sync.Mutex
	granted    map[idrReason]int
	suppressed map[idrReason]int
}

func (s *idrStats) record(reason idrReason, granted bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	counts := &s.suppressed
	if granted {
		counts = &s.granted
	}
	if *counts == nil {
		*counts = map[idrReason]int{}
	}
	(*counts)[reason]++
}

// drain returns a summary of the counts since the last call and resets them.
func (s *idrStats) drain() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.granted) == 0 && len(s.suppressed) == 0 {
		return ""
	}
	var summary string
	for _, reason := range []idrReason{
		idrReasonViewerPLI,
		idrReasonUpstreamDrop,
		idrReasonStaleQueue,
		idrReasonAbandoned,
		idrReasonRequeueDrop,
	} {
		granted, suppressed := s.granted[reason], s.suppressed[reason]
		if granted == 0 && suppressed == 0 {
			continue
		}
		if summary != "" {
			summary += " "
		}
		summary += fmt.Sprintf("%s=%d(+%d suppressed)", reason, granted, suppressed)
	}
	s.granted, s.suppressed = nil, nil
	return summary
}

// stageStats accumulates the per-frame timings of the relay's own stages over a
// logging window. The point is to attribute a stall: relay wait is how long a
// frame sat between arriving from the sender and its first packet going out,
// pacing is how long the rest of its packets then took.
type stageStats struct {
	frames       int
	maxQueueLen  int
	queueLenSum  int
	maxRelayMs   float64
	relayMsSum   float64
	maxPacingMs  float64
	pacingMsSum  float64
	idrFrames    int
	maxIDRBytes  int
	maxIDRPaceMs float64
}

func (s *stageStats) observe(queueLen int, relay, pacing time.Duration, idr bool, bytes int) {
	s.frames++
	s.queueLenSum += queueLen
	if queueLen > s.maxQueueLen {
		s.maxQueueLen = queueLen
	}
	relayMs := relay.Seconds() * 1000
	s.relayMsSum += relayMs
	if relayMs > s.maxRelayMs {
		s.maxRelayMs = relayMs
	}
	pacingMs := pacing.Seconds() * 1000
	s.pacingMsSum += pacingMs
	if pacingMs > s.maxPacingMs {
		s.maxPacingMs = pacingMs
	}
	if !idr {
		return
	}
	s.idrFrames++
	if bytes > s.maxIDRBytes {
		s.maxIDRBytes = bytes
	}
	if pacingMs > s.maxIDRPaceMs {
		s.maxIDRPaceMs = pacingMs
	}
}

func (s *stageStats) summary() string {
	if s.frames == 0 {
		return ""
	}
	frames := float64(s.frames)
	summary := fmt.Sprintf(
		"frames=%d queue avg=%.1f max=%d | relay wait avg=%.1fms max=%.1fms | pacing avg=%.1fms max=%.1fms",
		s.frames,
		float64(s.queueLenSum)/frames,
		s.maxQueueLen,
		s.relayMsSum/frames,
		s.maxRelayMs,
		s.pacingMsSum/frames,
		s.maxPacingMs,
	)
	if s.idrFrames > 0 {
		summary += fmt.Sprintf(
			" | idr n=%d max=%d bytes max pacing=%.1fms",
			s.idrFrames, s.maxIDRBytes, s.maxIDRPaceMs,
		)
	}
	return summary
}

// idrRequestGate bounds the feedback-loop gain: regardless of how many
// independent drop/PLI paths observe corruption, they can produce at most one
// upstream keyframe request during the interval. The two-second window is
// deliberately longer than the cost of a large keyframe at the target rate;
// shortening it makes recovery feedback generate another keyframe before the
// previous one has drained.
type idrRequestGate struct {
	interval time.Duration
	now      func() time.Time
	last     time.Time
}

func newIDRRequestGate(now func() time.Time) idrRequestGate {
	return idrRequestGate{interval: idrRequestInterval, now: now}
}

func (g *idrRequestGate) allow() bool {
	now := g.now()
	if !g.last.IsZero() && now.Sub(g.last) < g.interval {
		return false
	}
	g.last = now
	return true
}

func pacingSchedule(
	packetSizes []int,
	targetKbps uint32,
	frameDuration time.Duration,
) []time.Duration {
	return pacingScheduleForFrame(packetSizes, targetKbps, frameDuration, false)
}

func pacingScheduleForFrame(
	packetSizes []int,
	targetKbps uint32,
	frameDuration time.Duration,
	idr bool,
) []time.Duration {
	schedule := make([]time.Duration, len(packetSizes))
	if len(packetSizes) == 0 || frameDuration <= 0 {
		return schedule
	}
	frameBits := 0
	for _, size := range packetSizes {
		frameBits += size * 8
	}
	targetBitsPerSecond := float64(targetKbps) * 1000 * pacingMultiplier
	frameBitsPerSecond := float64(frameBits) / frameDuration.Seconds()
	bitsPerSecond := frameBitsPerSecond
	if targetKbps > 0 {
		bitsPerSecond = targetBitsPerSecond
		if bitsPerSecond < frameBitsPerSecond {
			bitsPerSecond = frameBitsPerSecond
		}
		maxBitsPerSecond := float64(targetKbps) * 1000 * maxPacingMultiplier
		if bitsPerSecond > maxBitsPerSecond {
			bitsPerSecond = maxBitsPerSecond
		}
	}
	if idr {
		idrBitsPerSecond := float64(frameBits) / pacingIDRTargetEmission.Seconds()
		if idrBitsPerSecond > bitsPerSecond {
			bitsPerSecond = idrBitsPerSecond
		}
		if bitsPerSecond > pacingIDRMaxRate {
			bitsPerSecond = pacingIDRMaxRate
		}
	}
	var elapsedSeconds float64
	for i, size := range packetSizes {
		schedule[i] = time.Duration(elapsedSeconds * float64(time.Second))
		elapsedSeconds += float64(size*8) / bitsPerSecond
	}
	return schedule
}

func assignSequenceNumber(packet *rtp.Packet, next *uint16) {
	packet.Header.SequenceNumber = *next
}

func commitSequenceNumber(next *uint16, writeSucceeded bool) {
	if writeSucceeded {
		*next = *next + 1
	}
}

func latestFrame(ch <-chan frameMsg, current frameMsg) (frameMsg, bool) {
	dropped := false
	for {
		select {
		case newer := <-ch:
			current = newer
			dropped = true
		default:
			return current, dropped
		}
	}
}

func consumeRTPDuration(duration time.Duration, remainder float64) (uint32, float64) {
	total := duration.Seconds()*90000 + remainder
	ticks := uint32(total)
	return ticks, total - float64(ticks)
}

func captureFrameDuration(lastTs, currentTs uint64, haveLastTs bool) time.Duration {
	if !haveLastTs || currentTs <= lastTs {
		return defaultFrameDuration
	}

	deltaMicros := currentTs - lastTs
	maxMicros := uint64(maxSaneFrameDuration / time.Microsecond)
	if deltaMicros > maxMicros {
		return maxSaneFrameDuration
	}

	frameDuration := time.Duration(deltaMicros) * time.Microsecond
	if frameDuration < minFrameDuration {
		return minFrameDuration
	}
	return frameDuration
}

func newMachineUpstream(addr, id string, status func(string)) *machineUpstream {
	u := &machineUpstream{
		id: id, addr: addr,
		frameChan:   make(chan frameMsg, 120),
		cursorChan:  make(chan cursorMsg, 1),
		commandChan: make(chan []byte, 100),
		stopChan:    make(chan struct{}),
		status:      status,
		idrGate:     newIDRRequestGate(time.Now),
	}
	u.abr = &abrState{commandChan: u.commandChan}
	return u
}

func (u *machineUpstream) run() {
	u.status("connecting")
	go u.framePusher()
	u.connectFrameServer()
}

func (u *machineUpstream) stop() {
	u.stopOnce.Do(func() {
		close(u.stopChan)
		u.status("stopped")
		u.mu.Lock()
		if u.conn != nil {
			_ = u.conn.Close()
		}
		if u.cancel != nil {
			u.cancel()
		}
		u.mu.Unlock()
	})
}

func (u *machineUpstream) send(cmd []byte) bool {
	select {
	case u.commandChan <- cmd:
		return true
	default:
		return false
	}
}

func (u *machineUpstream) requestIDR(reason idrReason) bool {
	granted := u.sendIDRRequest()
	u.idrStats.record(reason, granted)
	return granted
}

func (u *machineUpstream) sendIDRRequest() bool {
	u.mu.Lock()
	defer u.mu.Unlock()
	if !u.idrGate.allow() {
		return false
	}
	if u.send([]byte{0x01}) {
		return true
	}
	u.idrGate.last = time.Time{}
	return false
}

// requestInitialIDR sends exactly one immediate request for this session. A
// new session cannot display anything until its first IDR, so it is exempt
// from the two-second feedback gate; the per-session flag prevents an already
// running session from reusing that exemption.
func (u *machineUpstream) requestInitialIDR(session *Session) bool {
	u.mu.Lock()
	defer u.mu.Unlock()
	if session.initialIDRRequested {
		return false
	}
	if !u.send([]byte{0x01}) {
		return false
	}
	session.initialIDRRequested = true
	// Opening the gate window here too: the initial keyframe is the most
	// expensive one, so a drop observed while it drains must not immediately
	// ask for another.
	u.idrGate.last = u.idrGate.now()
	return true
}

func (u *machineUpstream) sendCursor(msg cursorMsg) {
	u.mu.Lock()
	copy := msg
	u.lastCursor = &copy
	u.mu.Unlock()
	select {
	case u.cursorChan <- msg:
	default:
		select {
		case <-u.cursorChan:
		default:
		}
		select {
		case u.cursorChan <- msg:
		default:
		}
	}
}

func (u *machineUpstream) sendViewerCount(count int) bool {
	u.viewerCount.Store(uint32(count))
	cmd := make([]byte, 5)
	cmd[0] = 0x04
	binary.BigEndian.PutUint32(cmd[1:], uint32(count))
	return u.send(cmd)
}

func (u *machineUpstream) bindSession(session *Session) *Session {
	u.mu.Lock()
	defer u.mu.Unlock()
	old := u.session
	u.session = session
	return old
}

func (u *machineUpstream) currentSession() *Session {
	u.mu.Lock()
	defer u.mu.Unlock()
	return u.session
}

func (u *machineUpstream) clearSession(session *Session) {
	u.mu.Lock()
	if u.session == session {
		u.session = nil
	}
	u.mu.Unlock()
}

func (u *machineUpstream) sendResolutionStatus(data []byte) {
	sess := u.currentSession()
	if sess == nil || sess.writer == nil || !json.Valid(data) {
		return
	}
	resp, err := json.Marshal(WSMessage{Type: "resolution-status", Data: data})
	if err == nil {
		_ = sess.writer.write(websocket.TextMessage, resp)
	}
}

func (u *machineUpstream) connectFrameServer() {
	for {
		select {
		case <-u.stopChan:
			return
		default:
		}
		if err := u.connectQUIC(); err != nil {
			log.Printf("[frame:%s] QUIC unavailable (%v), trying TCP", u.id, err)
		} else {
			if !u.wait(time.Second) {
				return
			}
			continue
		}

		log.Printf("[frame:%s] connecting to tcp %s ...", u.id, u.addr)
		conn, err := net.Dial("tcp", u.addr)
		if err != nil {
			log.Printf("[frame:%s] connection failed: %v, retrying in 2s", u.id, err)
			if !u.wait(2 * time.Second) {
				return
			}
			continue
		}
		log.Printf("[frame:%s] connected to %s", u.id, u.addr)
		u.status("connected")
		u.mu.Lock()
		u.conn = conn
		u.mu.Unlock()
		_ = u.sendViewerCount(int(u.viewerCount.Load()))

		// Spawn writer for upstream commands.
		done := make(chan struct{})
		go func() {
			for {
				select {
				case cmd := <-u.commandChan:
					if _, err := conn.Write(cmd); err != nil {
						log.Printf("[frame:%s] write command error: %v", u.id, err)
						return
					}
				case <-done:
					return
				case <-u.stopChan:
					return
				}
			}
		}()
		err = u.readFrames(conn)
		close(done) // Stop the writer.
		conn.Close()
		u.mu.Lock()
		u.conn = nil
		u.mu.Unlock()
		if err != nil {
			log.Printf("[frame:%s] read error: %v, reconnecting in 1s", u.id, err)
			u.status("reconnecting")
		}
		if !u.wait(time.Second) {
			return
		}
	}
}

func (u *machineUpstream) wait(duration time.Duration) bool {
	timer := time.NewTimer(duration)
	defer timer.Stop()
	select {
	case <-timer.C:
		return true
	case <-u.stopChan:
		return false
	}
}

func (u *machineUpstream) readFrames(conn net.Conn) error {
	var frameCount uint64
	for {
		select {
		case <-u.stopChan:
			return nil
		default:
		}
		// Protocol: [1-byte type][8-byte BE capture-ts µs][4-byte BE length][payload].
		// Type 0x01 is H.264; type 0x02 is cursor JSON metadata.
		var hdr [13]byte
		if _, err := io.ReadFull(conn, hdr[:]); err != nil {
			return fmt.Errorf("read header: %w", err)
		}
		messageType := hdr[0]
		tsMicros := binary.BigEndian.Uint64(hdr[1:9])
		payloadLen := binary.BigEndian.Uint32(hdr[9:13])
		if payloadLen == 0 || payloadLen > 10*1024*1024 {
			return fmt.Errorf("invalid frame length: %d", payloadLen)
		}
		data := make([]byte, payloadLen)
		if _, err := io.ReadFull(conn, data); err != nil {
			return fmt.Errorf("read frame data: %w", err)
		}
		switch messageType {
		case 0x01:
			frameCount++
			u.abr.countFrameBytes(len(data))
			if frameCount%300 == 0 {
				log.Printf("[frame:%s] received %d frames (last=%d bytes)", u.id, frameCount, payloadLen)
			}
			frame := frameMsg{tsMicros: tsMicros, data: data, receivedAt: time.Now()}
			select {
			case u.frameChan <- frame:
			default:
				select {
				case <-u.frameChan:
				default:
				}
				u.requestIDR(idrReasonUpstreamDrop)
				u.frameChan <- frame
			}
		case 0x02:
			if !json.Valid(data) {
				log.Printf("[frame:%s] invalid cursor JSON", u.id)
				continue
			}
			u.sendCursor(cursorMsg{tsMicros: tsMicros, data: data})
		case 0x03:
			u.sendResolutionStatus(data)
		default:
			return fmt.Errorf("unknown frame message type: 0x%02x", messageType)
		}
	}
}

func (u *machineUpstream) connectQUIC() error {
	dialCtx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	tlsConf := &tls.Config{
		InsecureSkipVerify: true,
		NextProtos:         []string{"flux-frames"},
	}
	conn, err := quic.DialAddr(dialCtx, u.addr, tlsConf, &quic.Config{
		MaxIdleTimeout:  15 * time.Second,
		KeepAlivePeriod: 3 * time.Second,
	})
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	log.Printf("[frame:%s] connected to quic %s", u.id, u.addr)
	u.status("connected")
	ctx, connCancel := context.WithCancel(context.Background())
	defer connCancel()
	defer conn.CloseWithError(0, "done")
	u.mu.Lock()
	u.cancel = connCancel
	u.mu.Unlock()
	_ = u.sendViewerCount(int(u.viewerCount.Load()))
	defer func() {
		u.mu.Lock()
		u.cancel = nil
		u.mu.Unlock()
		select {
		case <-u.stopChan:
		default:
			u.status("reconnecting")
		}
	}()
	control, err := conn.OpenStreamSync(ctx)
	if err != nil {
		return fmt.Errorf("open control stream: %w", err)
	}
	go func() {
		for {
			select {
			case cmd := <-u.commandChan:
				if _, err := control.Write(cmd); err != nil {
					log.Printf("[frame:%s] quic write command error: %v", u.id, err)
					connCancel()
					return
				}
			case <-ctx.Done():
				return
			case <-u.stopChan:
				connCancel()
				return
			}
		}
	}()
	var frameCount uint64
	for {
		stream, err := conn.AcceptUniStream(ctx)
		if err != nil {
			return fmt.Errorf("accept stream: %w", err)
		}
		// Frame stream: [1-byte type][8-byte BE capture-ts µs][4-byte BE length][payload].
		var hdr [13]byte
		if _, err := io.ReadFull(stream, hdr[:]); err != nil {
			continue
		}
		messageType := hdr[0]
		tsMicros := binary.BigEndian.Uint64(hdr[1:9])
		payloadLen := binary.BigEndian.Uint32(hdr[9:13])
		if payloadLen == 0 || payloadLen > 10*1024*1024 {
			continue
		}
		data := make([]byte, payloadLen)
		if _, err := io.ReadFull(stream, data); err != nil {
			continue
		}
		switch messageType {
		case 0x01:
			frameCount++
			u.abr.countFrameBytes(len(data))
			if frameCount%300 == 0 {
				log.Printf("[frame:%s] received %d frames via quic (last=%d bytes)", u.id, frameCount, payloadLen)
			}
			frame := frameMsg{tsMicros: tsMicros, data: data, receivedAt: time.Now()}
			select {
			case u.frameChan <- frame:
			default:
				select {
				case <-u.frameChan:
				default:
				}
				u.requestIDR(idrReasonUpstreamDrop)
				u.frameChan <- frame
			}
		case 0x02:
			if json.Valid(data) {
				u.sendCursor(cursorMsg{tsMicros: tsMicros, data: data})
			}
		case 0x03:
			u.sendResolutionStatus(data)
		default:
			return fmt.Errorf("unknown frame message type: 0x%02x", messageType)
		}
	}
}

func (u *machineUpstream) framePusher() {
	// The capture source (e.g. a Wayland/mutter screen-cast) delivers frames at
	// a variable, damage-driven rate and the server drains backlogs in bursts,
	// so network arrival time is a poor clock. Pace the RTP timestamp by the
	// frame's capture timestamp (reported by the server) instead, so the browser's
	// jitter buffer can absorb bursts and play at true capture cadence rather than
	// running ahead and stalling.
	var sampleCount uint64
	var lastTs uint64
	var haveLastTs bool
	var stats stageStats
	statsTicker := time.NewTicker(stageStatsInterval)
	defer statsTicker.Stop()
	for {
		select {
		case <-u.stopChan:
			return
		case <-statsTicker.C:
			if summary := stats.summary(); summary != "" {
				log.Printf("[stage:%s] %s", u.id, summary)
			}
			if summary := u.idrStats.drain(); summary != "" {
				log.Printf("[idr:%s] %s", u.id, summary)
			}
			stats = stageStats{}
		case msg := <-u.frameChan:
			queueLen := len(u.frameChan)
			var dropped bool
			msg, dropped = latestFrame(u.frameChan, msg)
			idr := isIDRFrame(msg.data)
			sess := u.currentSession()
			if sess == nil || sess.VideoTrack == nil {
				continue
			}
			if dropped {
				sess.needsIDR = true
				if u.requestIDR(idrReasonStaleQueue) {
					log.Printf("[webrtc:%s] discarded queued frames for newer frame; requesting IDR", u.id)
				}
			}
			sampleCount++
			// New session: skip P-frames until the next live IDR arrives.
			// P-frames can't be decoded without their preceding frames.
			if sess.needsIDR {
				if !idr {
					continue
				}
				if sess.hasStarted {
					log.Printf("[webrtc:%s] recovery IDR arrived (%d bytes), resuming stream", u.id, len(msg.data))
				} else {
					log.Printf("[webrtc:%s] initial IDR arrived (%d bytes), starting stream", u.id, len(msg.data))
				}
				sess.needsIDR = false
			} else if idr {
				log.Printf("[webrtc:%s] IDR sample #%d: %d bytes, NALUs: %s", u.id, sampleCount, len(msg.data), describeNALUs(msg.data))
			}
			if idr {
				sess.hasStarted = true
			}
			// Duration = capture-time gap since the previous sent sample. The
			// server timestamp resets on reconnect, so guard against going
			// backwards and fall back to the nominal duration.
			frameDuration := captureFrameDuration(lastTs, msg.tsMicros, haveLastTs)
			lastTs, haveLastTs = msg.tsMicros, true
			// Log first few frames and IDRs for diagnostics.
			if sampleCount <= 5 && !idr {
				log.Printf("[webrtc:%s] sample #%d: %d bytes, NALUs: %s", u.id, sampleCount, len(msg.data), describeNALUs(msg.data))
			}
			curTicks, remainder := consumeRTPDuration(frameDuration, sess.rtpRemainder)
			sess.rtpRemainder = remainder
			packets := sess.Packetizer.Packetize(msg.data, curTicks)
			packetSizes := make([]int, len(packets))
			for i, packet := range packets {
				packetSizes[i] = packet.MarshalSize()
			}
			schedule := pacingScheduleForFrame(packetSizes, u.abr.targetBitrateKbps(), frameDuration, idr)
			pacingStart := time.Now()
			next, sent := u.writePacedPackets(sess, packets, schedule, idr)
			pacingElapsed := time.Since(pacingStart)
			var relayWait time.Duration
			if !msg.receivedAt.IsZero() {
				relayWait = pacingStart.Sub(msg.receivedAt)
			}
			stats.observe(queueLen, relayWait, pacingElapsed, idr, len(msg.data))
			if idr {
				log.Printf("[webrtc:%s] IDR paced: %d packets, waited %.1fms in relay, sent over %.1fms",
					u.id, len(packets), relayWait.Seconds()*1000, pacingElapsed.Seconds()*1000)
			}
			if next != nil {
				remaining := len(packets) - sent
				if remaining > 0 {
					// Leave the marker clear: the emitted prefix is truncated,
					// not a complete access unit. Recovery comes from the IDR.
					sess.needsIDR = true
					if u.requestIDR(idrReasonAbandoned) {
						log.Printf("[webrtc:%s] abandoned %d paced RTP packets for newer frame; requesting IDR", u.id, remaining)
					}
				}
				msg = *next
				droppedQueued := false
				select {
				case u.frameChan <- msg:
				default:
					select {
					case <-u.frameChan:
						droppedQueued = true
					default:
					}
					u.frameChan <- msg
				}
				if droppedQueued {
					sess.needsIDR = true
					if u.requestIDR(idrReasonRequeueDrop) {
						log.Printf("[webrtc:%s] dropped queued frame while requeueing newer frame; requesting IDR", u.id)
					}
				}
			}
		}
	}
}

func (u *machineUpstream) writePacedPackets(
	sess *Session,
	packets []*rtp.Packet,
	schedule []time.Duration,
	idr bool,
) (*frameMsg, int) {
	start := time.Now()
	for i, packet := range packets {
		wait := time.Until(start.Add(schedule[i]))
		if wait > 0 {
			timer := time.NewTimer(wait)
			if idr {
				select {
				case <-u.stopChan:
					if !timer.Stop() {
						select {
						case <-timer.C:
						default:
						}
					}
					return nil, i
				case <-timer.C:
				}
			} else {
				select {
				case <-u.stopChan:
					if !timer.Stop() {
						select {
						case <-timer.C:
						default:
						}
					}
					return nil, i
				case next := <-u.frameChan:
					if !timer.Stop() {
						select {
						case <-timer.C:
						default:
						}
					}
					latest, _ := latestFrame(u.frameChan, next)
					return &latest, i
				case <-timer.C:
				}
			}
		}
		assignSequenceNumber(packet, &sess.nextSequenceNumber)
		err := sess.VideoTrack.WriteRTP(packet)
		if err != nil {
			log.Printf("[webrtc:%s] write RTP packet error: %v", u.id, err)
		}
		commitSequenceNumber(&sess.nextSequenceNumber, err == nil)
	}
	return nil, len(packets)
}
