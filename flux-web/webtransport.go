package main

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"github.com/quic-go/quic-go"
	"github.com/quic-go/quic-go/http3"
	"github.com/quic-go/webtransport-go"
)

const (
	controlAcceptTimeout = 5 * time.Second
	maxControlMessage    = 1 << 20
	controlWriteTimeout  = 5 * time.Second
	sessionErrorCode     = webtransport.SessionErrorCode(1)

	// A viewer that vanished without closing (page reload, network loss) holds
	// the machine's single viewer slot until this expires.
	viewerIdleTimeout     = 6 * time.Second
	viewerKeepAlivePeriod = 2 * time.Second
)

type quicConnContextKey struct{}

type Session struct {
	transport *webtransport.Session
	control   *webtransport.Stream
	quicConn  *quic.Conn
	id        string

	machine     *machineUpstream
	machineMu   sync.RWMutex
	release     func()
	releaseOnce sync.Once
	sessionDone chan struct{}

	controlMu           sync.Mutex
	needsIDR            atomic.Bool
	hasStarted          atomic.Bool
	initialIDRRequested atomic.Bool
	nextSequence        atomic.Uint32
	estimator           *deliveryEstimator
}

type controlMessage struct {
	Type string          `json:"type"`
	Data json.RawMessage `json:"data,omitempty"`
}

func newWebTransportServer(registry *machineRegistry, sessions *sessionRegistry, certificates *relayCertificateManager) *webtransport.Server {
	mux := http.NewServeMux()
	server := &webtransport.Server{
		H3: &http3.Server{
			Handler:   mux,
			TLSConfig: certificates.tlsConfig(),
			QUICConfig: &quic.Config{
				EnableDatagrams:                  true,
				EnableStreamResetPartialDelivery: true,
				MaxIdleTimeout:                   viewerIdleTimeout,
				KeepAlivePeriod:                  viewerKeepAlivePeriod,
			},
			ConnContext: func(ctx context.Context, conn *quic.Conn) context.Context {
				return context.WithValue(ctx, quicConnContextKey{}, conn)
			},
		},
		CheckOrigin: func(*http.Request) bool { return true },
	}
	mux.HandleFunc("/wt", func(w http.ResponseWriter, r *http.Request) {
		handleWebTransport(w, r, server, registry, sessions)
	})
	return server
}

func handleWebTransport(w http.ResponseWriter, r *http.Request, server *webtransport.Server, registry *machineRegistry, sessions *sessionRegistry) {
	transport, err := server.Upgrade(w, r)
	if err != nil {
		log.Printf("[webtransport] upgrade failed: %v", err)
		return
	}
	defer transport.CloseWithError(0, "")

	controlCtx, cancel := context.WithTimeout(transport.Context(), controlAcceptTimeout)
	control, err := transport.AcceptStream(controlCtx)
	cancel()
	if err != nil {
		_ = transport.CloseWithError(sessionErrorCode, "control stream was not opened")
		return
	}

	session := &Session{
		transport:   transport,
		control:     control,
		sessionDone: make(chan struct{}),
	}
	if conn, ok := r.Context().Value(quicConnContextKey{}).(*quic.Conn); ok {
		session.quicConn = conn
	}
	id, err := sessions.newID()
	if err != nil {
		session.fail("failed to create session")
		return
	}
	session.id = id
	machineID := r.URL.Query().Get("machine")
	if machineID == "" {
		machineID = "default"
	}
	width, height, err := requestedResolution(r)
	if err != nil {
		session.fail(err.Error())
		return
	}
	upstream, err := registry.acquire(machineID)
	if err != nil {
		session.fail(err.Error())
		return
	}
	session.setMachine(upstream)
	session.release = func() {
		sessions.remove(id)
		registry.release(upstream)
	}
	go func() {
		<-transport.Context().Done()
		session.releaseNow()
	}()
	initialKbps := upstream.abr.initialBitrateKbps()
	session.estimator = newDeliveryEstimator(initialKbps, time.Now())
	session.estimator.setCeilingKbps(upstream.abr.senderCeilingKbps())
	upstream.abr.onEstimate(initialKbps)
	old := upstream.bindSession(session)
	if old != nil {
		old.releaseNow()
		_ = old.transport.CloseWithError(sessionErrorCode, "session replaced")
	}
	session.needsIDR.Store(true)
	upstream.requestInitialIDR(session)
	if width != 0 && height != 0 {
		status, _ := json.Marshal(map[string]any{
			"state":  "transitioning",
			"width":  width,
			"height": height,
		})
		upstream.sendResolutionStatus(status)
		if !upstream.send([]byte{0x06, byte(width >> 8), byte(width), byte(height >> 8), byte(height)}) {
			session.fail("resolution change could not be queued")
			return
		}
	}
	clientIP := r.RemoteAddr
	if host, _, splitErr := net.SplitHostPort(r.RemoteAddr); splitErr == nil {
		clientIP = host
	}
	userAgent := r.UserAgent()
	if userAgent == "" {
		userAgent = r.URL.Query().Get("user_agent")
	}
	sessions.add(sessionInfo{
		ID:          id,
		MachineID:   machineID,
		MachineName: registry.displayName(machineID),
		ClientIP:    clientIP,
		UserAgent:   userAgent,
		StartedAt:   time.Now(),
		Width:       width,
		Height:      height,
	}, func(reason string) {
		if err := session.writeControl(controlMessage{
			Type: "session-ended",
			Data: mustJSON(map[string]string{"reason": reason}),
		}); err != nil {
			log.Printf("[webtransport] session %s end message failed: %v", id, err)
		}
		log.Printf("[webtransport] session ended id=%s machine=%s reason=%q", id, machineID, reason)
		_ = session.control.Close()
		select {
		case <-transport.Context().Done():
		case <-time.After(2 * time.Second):
		}
		_ = transport.CloseWithError(0, reason)
		session.releaseNow()
	})
	select {
	case <-session.sessionDone:
		sessions.remove(id)
	default:
	}
	if err := session.writeControl(controlMessage{Type: "session", Data: mustJSON(map[string]string{"id": id})}); err != nil {
		session.fail("failed to initialize session")
		return
	}
	log.Printf("[webtransport] session started id=%s machine=%s client=%s mode=%dx%d", id, machineID, clientIP, width, height)
	go forwardCursorUpdates(session)
	go session.runBandwidthEstimator()
	session.readControlMessages()
	session.releaseNow()
}

func requestedResolution(r *http.Request) (uint32, uint32, error) {
	widthValue, heightValue := r.URL.Query().Get("width"), r.URL.Query().Get("height")
	if widthValue == "" && heightValue == "" {
		return 0, 0, nil
	}
	width, err := strconv.ParseUint(widthValue, 10, 32)
	if err != nil {
		return 0, 0, errors.New("invalid resolution; use even dimensions from 640x480 through 2560x1440")
	}
	height, err := strconv.ParseUint(heightValue, 10, 32)
	if err != nil {
		return 0, 0, errors.New("invalid resolution; use even dimensions from 640x480 through 2560x1440")
	}
	if width < 640 || width > 2560 || height < 480 || height > 1440 || width%2 != 0 || height%2 != 0 {
		return 0, 0, errors.New("invalid resolution; use even dimensions from 640x480 through 2560x1440")
	}
	return uint32(width), uint32(height), nil
}

func (s *Session) setMachine(machine *machineUpstream) {
	s.machineMu.Lock()
	s.machine = machine
	s.machineMu.Unlock()
}

func (s *Session) getMachine() *machineUpstream {
	s.machineMu.RLock()
	defer s.machineMu.RUnlock()
	return s.machine
}

func (s *Session) releaseNow() {
	s.releaseOnce.Do(func() {
		_ = s.transport.CloseWithError(0, "session released")
		if machine := s.getMachine(); machine != nil {
			machine.clearSession(s)
		}
		if s.release != nil {
			s.release()
		}
		close(s.sessionDone)
	})
}

func (s *Session) fail(message string) {
	log.Printf("[webtransport] session failed id=%s reason=%q", s.id, message)
	_ = s.writeControl(controlMessage{
		Type: "error",
		Data: mustJSON(map[string]string{"error": message}),
	})
	_ = s.control.Close()
	select {
	case <-s.transport.Context().Done():
	case <-time.After(2 * time.Second):
	}
	_ = s.transport.CloseWithError(sessionErrorCode, message)
	s.releaseNow()
}

func mustJSON(value any) json.RawMessage {
	data, _ := json.Marshal(value)
	return data
}

func (s *Session) writeControl(message controlMessage) error {
	payload, err := json.Marshal(message)
	if err != nil {
		return err
	}
	if len(payload) > maxControlMessage {
		return errors.New("control message is too large")
	}
	record := make([]byte, 4+len(payload))
	binary.BigEndian.PutUint32(record[:4], uint32(len(payload)))
	copy(record[4:], payload)
	s.controlMu.Lock()
	defer s.controlMu.Unlock()
	_ = s.control.SetWriteDeadline(time.Now().Add(controlWriteTimeout))
	stopCancel := context.AfterFunc(s.transport.Context(), func() {
		s.control.CancelWrite(webtransport.StreamErrorCode(1))
	})
	_, err = s.control.Write(record)
	stopCancel()
	_ = s.control.SetWriteDeadline(time.Time{})
	if err != nil {
		s.releaseNow()
		return err
	}
	return nil
}

func (s *Session) readControlMessages() {
	for {
		var length [4]byte
		if _, err := io.ReadFull(s.control, length[:]); err != nil {
			s.releaseNow()
			return
		}
		n := binary.BigEndian.Uint32(length[:])
		if n == 0 || n > maxControlMessage {
			s.fail("invalid control message length")
			return
		}
		payload := make([]byte, n)
		if _, err := io.ReadFull(s.control, payload); err != nil {
			s.releaseNow()
			return
		}
		var message controlMessage
		if err := json.Unmarshal(payload, &message); err != nil {
			s.fail("invalid control message")
			return
		}
		if err := s.handleControlMessage(message); err != nil {
			s.fail(err.Error())
			return
		}
		select {
		case <-s.sessionDone:
			return
		default:
		}
	}
}

func (s *Session) handleControlMessage(message controlMessage) error {
	machine := s.getMachine()
	switch message.Type {
	case "input":
		if machine == nil || !json.Valid(message.Data) {
			return errors.New("invalid input")
		}
		command := make([]byte, 5+len(message.Data))
		command[0] = 0x02
		binary.BigEndian.PutUint32(command[1:5], uint32(len(message.Data)))
		copy(command[5:], message.Data)
		if !machine.send(command) {
			log.Printf("[control] upstream command channel full, dropped input")
		}
	case "quality":
		if machine == nil {
			return errors.New("no active machine session")
		}
		var quality struct {
			Level uint8 `json:"level"`
			FPS   uint8 `json:"fps"`
		}
		if err := json.Unmarshal(message.Data, &quality); err != nil || quality.Level > 10 || quality.FPS > 144 {
			return errors.New("quality must be 0-10 and FPS must be 0-144")
		}
		log.Printf("[control] quality/fps machine=%s level=%d fps=%d", machine.id, quality.Level, quality.FPS)
		if !machine.send([]byte{0x05, quality.Level, quality.FPS}) {
			log.Printf("[control] upstream command channel full, dropped quality/FPS control")
		}
	case "latency-ping":
		if err := s.writeControl(controlMessage{Type: "latency-pong", Data: message.Data}); err != nil {
			return err
		}
	case "keyframe-request":
		if machine != nil {
			s.needsIDR.Store(true)
			machine.requestIDR(idrReasonViewerRequest)
		}
	case "frame-ack":
		if s.estimator == nil {
			return nil
		}
		var ack struct {
			Seq    uint32  `json:"seq"`
			DoneMs float64 `json:"doneMs"`
		}
		if err := json.Unmarshal(message.Data, &ack); err != nil {
			return fmt.Errorf("invalid frame acknowledgement: %w", err)
		}
		s.estimator.onFrameAck(ack.Seq, ack.DoneMs, time.Now())
	default:
		log.Printf("[control] unknown message type: %s", message.Type)
	}
	return nil
}

func forwardCursorUpdates(session *Session) {
	ticker := time.NewTicker(16 * time.Millisecond)
	defer ticker.Stop()
	machine := session.getMachine()
	if machine == nil {
		return
	}
	machine.mu.Lock()
	var latest *cursorMsg
	if machine.lastCursor != nil {
		copy := *machine.lastCursor
		latest = &copy
	}
	machine.mu.Unlock()
	for {
		select {
		case <-session.sessionDone:
			return
		case msg := <-machine.cursorChan:
			latest = &msg
		case <-ticker.C:
			if latest == nil {
				continue
			}
			if err := session.sendMetadataStream(0x02, latest.data); err != nil {
				session.releaseNow()
				_ = session.transport.CloseWithError(sessionErrorCode, err.Error())
				return
			}
			latest = nil
		}
	}
}

func (s *Session) sendMetadataStream(kind byte, payload []byte) error {
	ctx := s.transport.Context()
	stream, err := s.transport.OpenUniStreamSync(ctx)
	if err != nil {
		return err
	}
	stopCancel := context.AfterFunc(ctx, func() {
		stream.CancelWrite(webtransport.StreamErrorCode(1))
	})
	defer stopCancel()
	header := make([]byte, 5)
	header[0] = kind
	binary.BigEndian.PutUint32(header[1:], uint32(len(payload)))
	for _, part := range [][]byte{header, payload} {
		for len(part) > 0 {
			n, writeErr := stream.Write(part)
			if writeErr != nil {
				stream.CancelWrite(webtransport.StreamErrorCode(1))
				return writeErr
			}
			if n == 0 {
				stream.CancelWrite(webtransport.StreamErrorCode(1))
				return io.ErrShortWrite
			}
			part = part[n:]
		}
	}
	return stream.Close()
}

func (s *Session) runBandwidthEstimator() {
	if s.estimator == nil {
		return
	}
	ticker := time.NewTicker(500 * time.Millisecond)
	logTicker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()
	defer logTicker.Stop()
	for {
		select {
		case <-s.sessionDone:
			return
		case <-ticker.C:
			if machine := s.getMachine(); machine != nil {
				s.estimator.setCeilingKbps(machine.abr.senderCeilingKbps())
			}
			if s.quicConn != nil {
				stats := s.quicConn.ConnectionStats()
				s.estimator.onTransportStats(stats.SmoothedRTT, stats.MinRTT, stats.PacketsSent, stats.PacketsLost)
				if machine := s.getMachine(); machine != nil {
					machine.abr.onRTTSample(stats.SmoothedRTT)
					machine.abr.onReceiverReport(s.estimator.lossRatio())
				}
			}
			kbps, _ := s.estimator.update(time.Now())
			if machine := s.getMachine(); machine != nil {
				machine.abr.onEstimate(kbps)
			}
		case <-logTicker.C:
			target, queue, rtt, loss := s.estimator.diagnostics()
			log.Printf("[bwe] target=%d kbps queue=%.1fms rtt-inflation=%.1fms loss=%.1f%%",
				target, queue.Seconds()*1000, rtt.Seconds()*1000, loss*100)
		}
	}
}
