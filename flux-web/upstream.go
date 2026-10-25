package main

import (
	"context"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"sync"
	"sync/atomic"
	"time"

	"github.com/quic-go/quic-go"
	"github.com/quic-go/webtransport-go"
)

// ---------------------------------------------------------------------------
// TCP/QUIC frame reader — connects to flux-server's frame server
// ---------------------------------------------------------------------------

type machineUpstream struct {
	id             string
	addr           string
	frameChan      chan frameMsg
	frameQueueMu   sync.Mutex
	frameBuffers   frameBufferPool
	cursorChan     chan cursorMsg
	audioChan      chan audioMsg
	commandChan    chan []byte
	abr            *abrState
	stopChan       chan struct{}
	stopOnce       sync.Once
	viewers        int // Protected by machineRegistry.mu.
	viewerCount    atomic.Uint32
	mu             sync.Mutex
	session        *Session
	lastCursor     *cursorMsg
	conn           net.Conn
	cancel         context.CancelFunc
	status         func(string)
	idrGate        idrRequestGate
	idrStats       idrStats
	metrics        *machineMetrics
	connectedOnce  atomic.Bool
	deps           *relayDeps
	clipRate       clipboardRate
	loginMu        sync.Mutex
	login          loginInfo
	lastPrompt     *loginFrameJSON
	loginSpoken    bool
	supportsLogin  bool
	quicCertSHA256 string
}

const (
	idrRequestInterval    = 2 * time.Second
	stageStatsInterval    = 5 * time.Second
	maxFramePayloadSize   = 10 * 1024 * 1024
	maxCachedFrameBuffers = 4
	// A frame that has waited this long in the relay queue means the relay is
	// genuinely behind; only then does it skip ahead, at the cost of a recovery
	// keyframe. Shorter backlogs are sent in order, because dropping any frame
	// of an IPPP stream freezes the picture until the next keyframe arrives.
	maxRelayBacklog = 150 * time.Millisecond
)

// Keep only a small, bounded free list per upstream, rather than retaining the
// entire queue's high-water mark. Active frames never share a cached buffer.
type frameBufferPool struct {
	mu      sync.Mutex
	buffers [maxCachedFrameBuffers][]byte
	count   int
}

func (p *frameBufferPool) get(size int) []byte {
	p.mu.Lock()
	best := -1
	for i := 0; i < p.count; i++ {
		if cap(p.buffers[i]) >= size && (best < 0 || cap(p.buffers[i]) < cap(p.buffers[best])) {
			best = i
		}
	}
	if best < 0 && p.count > 0 {
		best = p.count - 1
	}
	if best >= 0 {
		data := p.buffers[best]
		p.count--
		p.buffers[best] = p.buffers[p.count]
		p.buffers[p.count] = nil
		p.mu.Unlock()
		if cap(data) >= size {
			return data[:size]
		}
		return make([]byte, size)
	}
	p.mu.Unlock()
	return make([]byte, size)
}

func (p *frameBufferPool) put(data []byte) {
	if cap(data) == 0 || cap(data) > maxFramePayloadSize {
		return
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.count < len(p.buffers) {
		p.buffers[p.count] = data[:0]
		p.count++
	}
}

// Why a keyframe was asked for. Every request path is tagged so the logs show
// whether keyframes are driven by the viewer's decoder (real loss on the path)
// or by the relay's own drop paths (the stream not keeping up locally) — the two
// call for opposite fixes.
type idrReason string

const (
	idrReasonViewerRequest idrReason = "viewer-request"
	idrReasonUpstreamDrop  idrReason = "upstream-queue-full"
	idrReasonStaleQueue    idrReason = "stale-queue-discard"
	idrReasonAwaitingIDR   idrReason = "awaiting-idr"
)

// idrStats counts keyframe requests by origin, including the ones the gate
// suppressed: a high suppressed count means the stream is still asking for
// keyframes far faster than it can send them.
type idrStats struct {
	mu         sync.Mutex
	granted    map[idrReason]int
	suppressed map[idrReason]int
	metrics    *machineMetrics
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
	if s.metrics != nil {
		s.metrics.recordIDR(reason, granted)
	}
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
		idrReasonViewerRequest,
		idrReasonUpstreamDrop,
		idrReasonStaleQueue,
		idrReasonAwaitingIDR,
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

type stageStats struct {
	frames       int
	maxQueueLen  int
	queueLenSum  int
	maxRelayMs   float64
	relayMsSum   float64
	maxSendMs    float64
	sendMsSum    float64
	idrFrames    int
	maxIDRBytes  int
	maxIDRSendMs float64
	resets       int
}

func (s *stageStats) observe(queueLen int, relay, send time.Duration, idr bool, bytes int, reset bool) {
	s.frames++
	if reset {
		s.resets++
	}
	s.queueLenSum += queueLen
	if queueLen > s.maxQueueLen {
		s.maxQueueLen = queueLen
	}
	relayMs := relay.Seconds() * 1000
	s.relayMsSum += relayMs
	if relayMs > s.maxRelayMs {
		s.maxRelayMs = relayMs
	}
	sendMs := send.Seconds() * 1000
	s.sendMsSum += sendMs
	if sendMs > s.maxSendMs {
		s.maxSendMs = sendMs
	}
	if !idr {
		return
	}
	s.idrFrames++
	if bytes > s.maxIDRBytes {
		s.maxIDRBytes = bytes
	}
	if sendMs > s.maxIDRSendMs {
		s.maxIDRSendMs = sendMs
	}
}

func (s *stageStats) summary() string {
	if s.frames == 0 {
		return ""
	}
	frames := float64(s.frames)
	summary := fmt.Sprintf(
		"frames=%d queue avg=%.1f max=%d | relay wait avg=%.1fms max=%.1fms | send avg=%.1fms max=%.1fms",
		s.frames,
		float64(s.queueLenSum)/frames,
		s.maxQueueLen,
		s.relayMsSum/frames,
		s.maxRelayMs,
		s.sendMsSum/frames,
		s.maxSendMs,
	)
	if s.idrFrames > 0 {
		summary += fmt.Sprintf(
			" | idr n=%d max=%d bytes max send=%.1fms",
			s.idrFrames, s.maxIDRBytes, s.maxIDRSendMs,
		)
	}
	if s.resets > 0 {
		summary += fmt.Sprintf(" | resets=%d", s.resets)
	}
	return summary
}

// idrRequestGate bounds the feedback-loop gain: regardless of how many
// independent drop/keyframe-request paths observe corruption, they can produce at most one
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

// latestFrame drains ch, preferring the newest IDR frame over newer P-frames.
// It returns the chosen frame, whether it is an IDR frame, and whether any
// queued frames were discarded.
func latestFrame(ch <-chan frameMsg, current frameMsg) (frameMsg, bool, bool) {
	dropped := false
	currentIsIDR := isIDRFrame(current.data)
	for {
		select {
		case newer := <-ch:
			dropped = true
			newerIsIDR := isIDRFrame(newer.data)
			if newerIsIDR || !currentIsIDR {
				current.release()
				current = newer
				currentIsIDR = newerIsIDR
			} else {
				newer.release()
			}
		default:
			return current, currentIsIDR, dropped
		}
	}
}

// nextFrame picks what to send after current was taken from the queue. Frames
// normally go out in order; only when current has waited longer than
// maxRelayBacklog and newer frames are queued does it skip ahead through
// latestFrame, reporting the drop so the caller recovers with a keyframe.
func nextFrame(ch <-chan frameMsg, current frameMsg, now time.Time) (frameMsg, bool, bool) {
	if len(ch) == 0 || current.receivedAt.IsZero() || now.Sub(current.receivedAt) <= maxRelayBacklog {
		return current, isIDRFrame(current.data), false
	}
	return latestFrame(ch, current)
}

func newMachineUpstream(addr, id string, deps *relayDeps, status func(string), metricChildren ...*machineMetrics) *machineUpstream {
	var metrics *machineMetrics
	if len(metricChildren) > 0 {
		metrics = metricChildren[0]
	}
	if metrics == nil {
		metrics = newRelayMetrics().machine(id)
	}
	u := &machineUpstream{
		id: id, addr: addr,
		frameChan:   make(chan frameMsg, 120),
		cursorChan:  make(chan cursorMsg, 1),
		audioChan:   make(chan audioMsg, 50),
		commandChan: make(chan []byte, 100),
		stopChan:    make(chan struct{}),
		status:      status,
		idrGate:     newIDRRequestGate(time.Now),
		metrics:     metrics,
		deps:        deps,
	}
	u.idrStats.metrics = metrics
	u.abr = &abrState{commandChan: u.commandChan, metrics: metrics}
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
		u.frameQueueMu.Lock()
		for {
			select {
			case frame := <-u.frameChan:
				frame.release()
			default:
				u.frameQueueMu.Unlock()
				u.metrics.owner.deleteUpstreamMachine(u.id)
				return
			}
		}
	})
}

// queueFrame consumes frame even when stopping. Serializing the two producers
// (reader and pacing requeue) keeps eviction + enqueue atomic and nonblocking.
func (u *machineUpstream) queueFrame(frame frameMsg) (dropped bool) {
	u.frameQueueMu.Lock()
	defer u.frameQueueMu.Unlock()
	select {
	case <-u.stopChan:
		frame.release()
		return false
	default:
	}
	select {
	case u.frameChan <- frame:
	default:
		select {
		case old := <-u.frameChan:
			old.release()
			dropped = true
		default:
		}
		u.frameChan <- frame
	}
	return dropped
}

func (u *machineUpstream) readPayload(reader io.Reader, messageType byte, size int) (frameMsg, error) {
	var frame frameMsg
	if messageType == msgVideo {
		frame.buffers = &u.frameBuffers
		frame.data = u.frameBuffers.get(size)
	} else {
		frame.data = make([]byte, size)
	}
	if _, err := io.ReadFull(reader, frame.data); err != nil {
		frame.release()
		return frameMsg{}, err
	}
	return frame, nil
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

func (u *machineUpstream) skipFrameUntilIDR(session *Session, idr bool) bool {
	if !session.needsIDR.Load() || idr {
		return false
	}
	u.requestIDR(idrReasonAwaitingIDR)
	return true
}

func (u *machineUpstream) sendIDRRequest() bool {
	u.mu.Lock()
	defer u.mu.Unlock()
	if !u.idrGate.allow() {
		return false
	}
	if u.send([]byte{cmdIDR}) {
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
	if !session.initialIDRRequested.CompareAndSwap(false, true) {
		return false
	}
	if !u.send([]byte{cmdIDR}) {
		session.initialIDRRequested.Store(false)
		return false
	}
	u.idrStats.record(idrReasonViewerRequest, true)
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

// sendAudio queues an Opus packet for the session's forwardAudio loop,
// dropping the oldest packet when the buffer is full (audio tolerates gaps
// better than latency).
func (u *machineUpstream) sendAudio(msg audioMsg) {
	select {
	case u.audioChan <- msg:
	default:
		select {
		case <-u.audioChan:
		default:
		}
		select {
		case u.audioChan <- msg:
		default:
		}
	}
}

func (u *machineUpstream) sendViewerCount(count int) bool {
	u.viewerCount.Store(uint32(count))
	u.metrics.viewers.Set(float64(count))
	return u.send(u32Command(cmdViewers, uint32(count)))
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
	if sess == nil || !json.Valid(data) {
		return
	}
	if err := sess.sendMetadataStream(streamResolutionStatus, data); err != nil {
		log.Printf("[wt:%s] resolution status stream failed: %v", u.id, err)
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
		// hostd demands the hello as the first command; write it
		// directly and drop anything queued for the dead connection.
		u.drainCommands()
		if hello := helloCommand(); hello != nil {
			if _, err := conn.Write(hello); err != nil {
				log.Printf("[frame:%s] hello write failed: %v", u.id, err)
				conn.Close()
				continue
			}
		}
		u.markConnected()
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
		u.markDisconnected()
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
		header, frame, err := u.readFrame(conn)
		if err != nil {
			return err
		}
		if err := u.dispatchFrame(header, frame, "tcp", &frameCount); err != nil {
			return err
		}
	}
}

// frameHeader is the prefix of every host→relay message:
// [1-byte type][8-byte BE capture-ts µs][4-byte BE length][payload].
type frameHeader struct {
	messageType byte
	tsMicros    uint64
	payloadLen  uint32
}

// readFrame reads one complete host message (header and payload). Errors are
// fatal for the connection, except that callers on QUIC may skip streams the
// host reset (see isStreamReset).
func (u *machineUpstream) readFrame(reader io.Reader) (frameHeader, frameMsg, error) {
	var hdr [13]byte
	if _, err := io.ReadFull(reader, hdr[:]); err != nil {
		return frameHeader{}, frameMsg{}, fmt.Errorf("read header: %w", err)
	}
	header := frameHeader{
		messageType: hdr[0],
		tsMicros:    binary.BigEndian.Uint64(hdr[1:9]),
		payloadLen:  binary.BigEndian.Uint32(hdr[9:13]),
	}
	if header.payloadLen == 0 || header.payloadLen > maxFramePayloadSize {
		return frameHeader{}, frameMsg{}, fmt.Errorf("invalid frame length: %d", header.payloadLen)
	}
	frame, err := u.readPayload(reader, header.messageType, int(header.payloadLen))
	if err != nil {
		return frameHeader{}, frameMsg{}, fmt.Errorf("read frame data: %w", err)
	}
	return header, frame, nil
}

// dispatchFrame routes one host message by type. frameCount counts video
// frames on the current connection for the periodic log line.
func (u *machineUpstream) dispatchFrame(header frameHeader, frame frameMsg, transport string, frameCount *uint64) error {
	data := frame.data
	switch header.messageType {
	case msgVideo:
		*frameCount++
		u.metrics.upstreamFrames.Inc()
		u.metrics.upstreamBytes.Add(float64(len(data)))
		u.abr.countFrameBytes(len(data))
		if *frameCount%300 == 0 {
			log.Printf("[frame:%s] received %d frames via %s (last=%d bytes)", u.id, *frameCount, transport, header.payloadLen)
		}
		frame.tsMicros, frame.receivedAt = header.tsMicros, time.Now()
		if u.queueFrame(frame) {
			u.requestIDR(idrReasonUpstreamDrop)
		}
	case msgCursor:
		if !json.Valid(data) {
			log.Printf("[frame:%s] invalid cursor JSON", u.id)
			return nil
		}
		u.sendCursor(cursorMsg{tsMicros: header.tsMicros, data: data})
	case msgResolutionStatus:
		u.sendResolutionStatus(data)
	case msgAudio:
		u.metrics.upstreamAudioPackets.Inc()
		u.metrics.upstreamAudioBytes.Add(float64(len(data)))
		u.sendAudio(audioMsg{tsMicros: header.tsMicros, data: data})
	case msgClipboard:
		u.handleClipboardFrame(data)
	case msgLogin:
		u.handleLoginFrame(data)
	default:
		return fmt.Errorf("unknown frame message type: 0x%02x", header.messageType)
	}
	return nil
}

// readQUICStream reads and dispatches the single message carried by a QUIC
// uni-stream. A stream the host reset (it abandons superseded P-frames under
// backlog) is dropped; any other error is fatal for the connection.
func (u *machineUpstream) readQUICStream(stream io.Reader, frameCount *uint64) error {
	header, frame, err := u.readFrame(stream)
	if err != nil {
		if isStreamReset(err) {
			return nil
		}
		return err
	}
	return u.dispatchFrame(header, frame, "quic", frameCount)
}

func isStreamReset(err error) bool {
	var streamErr *quic.StreamError
	return errors.As(err, &streamErr) && streamErr.Remote
}

func (u *machineUpstream) connectQUIC() error {
	dialCtx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	tlsConf := &tls.Config{
		NextProtos: []string{"flux-frames"},
	}
	expected := u.pinnedCertSHA256()
	if expected == "" {
		log.Printf("[frame:%s] WARN: no registered cert fingerprint; skipping TLS verification", u.id)
		tlsConf.InsecureSkipVerify = true
	} else {
		tlsConf.InsecureSkipVerify = true // we verify manually below
		tlsConf.VerifyPeerCertificate = func(rawCerts [][]byte, _ [][]*x509.Certificate) error {
			if len(rawCerts) == 0 {
				return fmt.Errorf("no peer certificate")
			}
			sum := sha256.Sum256(rawCerts[0])
			actual := hex.EncodeToString(sum[:])
			if actual != expected {
				return fmt.Errorf("quic certificate fingerprint mismatch: want %s got %s", expected, actual)
			}
			return nil
		}
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
	defer u.markDisconnected()
	u.mu.Lock()
	u.cancel = connCancel
	u.mu.Unlock()
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
	// Same hello-first rule as TCP: write it directly on the command
	// stream before the pump starts draining the shared channel.
	u.drainCommands()
	if hello := helloCommand(); hello != nil {
		if _, err := control.Write(hello); err != nil {
			return fmt.Errorf("hello write: %w", err)
		}
	}
	u.markConnected()
	_ = u.sendViewerCount(int(u.viewerCount.Load()))
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
		if err := u.readQUICStream(stream, &frameCount); err != nil {
			return err
		}
	}
}

func (u *machineUpstream) markConnected() {
	if u.connectedOnce.Swap(true) {
		u.metrics.upstreamReconnects.Inc()
	}
	u.metrics.upstreamConnected.Set(1)
}

func (u *machineUpstream) markDisconnected() {
	u.metrics.upstreamConnected.Set(0)
	u.onUpstreamDisconnected()
}

func (u *machineUpstream) framePusher() {
	var sampleCount uint64
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
			var idr, dropped bool
			msg, idr, dropped = nextFrame(u.frameChan, msg, time.Now())
			sess := u.currentSession()
			if sess == nil {
				msg.release()
				continue
			}
			if dropped {
				sess.needsIDR.Store(true)
				u.metrics.framesDroppedStale.Inc()
				if u.requestIDR(idrReasonStaleQueue) {
					log.Printf("[wt:%s] relay fell more than %s behind; discarded queued frames and requesting IDR", u.id, maxRelayBacklog)
				}
			}
			sampleCount++
			if sess.needsIDR.Load() {
				if u.skipFrameUntilIDR(sess, idr) {
					u.metrics.framesDroppedIDR.Inc()
					msg.release()
					continue
				}
				if sess.hasStarted.Load() {
					log.Printf("[wt:%s] recovery IDR arrived (%d bytes), resuming stream", u.id, len(msg.data))
				} else {
					log.Printf("[wt:%s] initial IDR arrived (%d bytes), starting stream", u.id, len(msg.data))
				}
				sess.needsIDR.Store(false)
			} else if idr {
				log.Printf("[wt:%s] IDR sample #%d: %d bytes, NALUs: %s", u.id, sampleCount, len(msg.data), describeNALUs(msg.data))
			}
			if idr {
				sess.hasStarted.Store(true)
			}
			if sampleCount <= 5 && !idr {
				log.Printf("[wt:%s] sample #%d: %d bytes, NALUs: %s", u.id, sampleCount, len(msg.data), describeNALUs(msg.data))
			}
			frameBytes := len(msg.data)
			writeStarted := time.Now()
			queueWait := time.Duration(0)
			if !msg.receivedAt.IsZero() && !msg.receivedAt.After(writeStarted) {
				queueWait = writeStarted.Sub(msg.receivedAt)
			}
			writeElapsed, reset, err := u.writeFrame(sess, msg, idr)
			msg.release()
			u.metrics.recordFrameWrite(idr, frameBytes, writeElapsed, queueWait, err == nil && !reset, reset)
			if err == nil && !reset {
				sess.firstFrameWritten.Do(func() {
					if !sess.acceptedAt.IsZero() {
						u.metrics.timeToFirstFrame.Observe(time.Since(sess.acceptedAt).Seconds())
					}
				})
			}
			var relayWait time.Duration
			if !msg.receivedAt.IsZero() && !msg.receivedAt.After(time.Now()) {
				relayWait = time.Since(msg.receivedAt) - writeElapsed
			}
			stats.observe(queueLen, relayWait, writeElapsed, idr, frameBytes, reset)
			if err != nil {
				sess.needsIDR.Store(true)
				log.Printf("[wt:%s] video stream write failed: %v", u.id, err)
				sess.releaseNow()
			} else if reset {
				log.Printf("[wt:%s] reset stale delta stream after %.1fms; requested IDR", u.id, writeElapsed.Seconds()*1000)
			}
		}
	}
}

func encodeVideoFrameHeader(seq uint32, msg frameMsg, keyframe bool) [18]byte {
	var header [18]byte
	header[0] = streamVideo
	binary.BigEndian.PutUint32(header[1:5], seq)
	binary.BigEndian.PutUint64(header[5:13], msg.tsMicros)
	if keyframe {
		header[13] = 1
	}
	binary.BigEndian.PutUint32(header[14:18], uint32(len(msg.data)))
	return header
}

func shouldResetFrame(keyframe bool, writingFor time.Duration, queuedFrames int) bool {
	return !keyframe && queuedFrames > 0 && writingFor > 16*time.Millisecond
}

func (u *machineUpstream) writeFrame(sess *Session, msg frameMsg, keyframe bool) (time.Duration, bool, error) {
	start := time.Now()
	seq := sess.nextSequence.Add(1) - 1
	if sess.estimator != nil {
		sess.estimator.onFrameSent(seq, len(msg.data), start)
	}
	forget := func() {
		if sess.estimator != nil {
			sess.estimator.forget(seq)
		}
	}
	stream, err := sess.transport.OpenUniStreamSync(sess.transport.Context())
	if err != nil {
		forget()
		return time.Since(start), false, err
	}
	stopCancel := context.AfterFunc(sess.transport.Context(), func() {
		stream.CancelWrite(webtransport.StreamErrorCode(1))
	})
	defer stopCancel()
	header := encodeVideoFrameHeader(seq, msg, keyframe)
	chunks := [][]byte{header[:]}
	for offset := 0; offset < len(msg.data); offset += 16 * 1024 {
		end := min(offset+16*1024, len(msg.data))
		chunks = append(chunks, msg.data[offset:end])
	}
	for _, chunk := range chunks {
		for len(chunk) > 0 {
			if shouldResetFrame(keyframe, time.Since(start), len(u.frameChan)) {
				stream.CancelWrite(webtransport.StreamErrorCode(2))
				forget()
				sess.needsIDR.Store(true)
				u.requestIDR(idrReasonStaleQueue)
				return time.Since(start), true, nil
			}
			n, writeErr := stream.Write(chunk)
			if writeErr != nil {
				forget()
				return time.Since(start), false, writeErr
			}
			if n == 0 {
				forget()
				return time.Since(start), false, io.ErrShortWrite
			}
			chunk = chunk[n:]
		}
	}
	if err := stream.Close(); err != nil {
		forget()
		return time.Since(start), false, err
	}
	return time.Since(start), false, nil
}

// helloCommand builds the cmdHello command carrying the relay auth token.
// Returns nil when no token is configured.
func helloCommand() []byte {
	token := os.Getenv("FLUX_RELAY_TOKEN")
	if token == "" {
		return nil
	}
	return commandFrame(cmdHello, []byte(token))
}

// setQuicCertSHA256 refreshes the pinned fingerprint from the latest
// registration/heartbeat so a rotated host cert doesn't strand the link.
func (u *machineUpstream) setQuicCertSHA256(v string) {
	u.mu.Lock()
	u.quicCertSHA256 = v
	u.mu.Unlock()
}

func (u *machineUpstream) pinnedCertSHA256() string {
	u.mu.Lock()
	defer u.mu.Unlock()
	return u.quicCertSHA256
}

// drainCommands discards commands queued for a previous connection so the
// new link's first bytes are always the hello.
func (u *machineUpstream) drainCommands() {
	for {
		select {
		case <-u.commandChan:
		default:
			return
		}
	}
}
