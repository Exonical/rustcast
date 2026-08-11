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
	id             string
	addr           string
	frameChan      chan frameMsg
	cursorChan     chan cursorMsg
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
	lastIDRRequest time.Time
}

const (
	defaultFrameDuration = 16 * time.Millisecond // ~60fps for the first sample
	minFrameDuration     = 4 * time.Millisecond  // clamp absurdly fast bursts
	maxSaneFrameDuration = 30 * time.Second      // cap corrupted/absurd timestamps
	pacingMultiplier     = 2                     // smooth bursts at 2x target bitrate
	pacingIDRInterval    = 500 * time.Millisecond
)

func pacingSchedule(packetSizes []int, targetKbps uint32) []time.Duration {
	schedule := make([]time.Duration, len(packetSizes))
	if targetKbps == 0 {
		return schedule
	}
	bitsPerSecond := float64(targetKbps) * 1000 * pacingMultiplier
	var elapsedSeconds float64
	for i, size := range packetSizes {
		schedule[i] = time.Duration(elapsedSeconds * float64(time.Second))
		elapsedSeconds += float64(size*8) / bitsPerSecond
	}
	return schedule
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

func (u *machineUpstream) requestIDR() bool {
	u.mu.Lock()
	defer u.mu.Unlock()
	if !u.lastIDRRequest.IsZero() && time.Since(u.lastIDRRequest) < pacingIDRInterval {
		return false
	}
	u.lastIDRRequest = time.Now()
	return u.send([]byte{0x01})
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
			select {
			case u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}:
			default:
				select {
				case <-u.frameChan:
				default:
				}
				u.requestIDR()
				u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}
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
			select {
			case u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}:
			default:
				select {
				case <-u.frameChan:
				default:
				}
				u.requestIDR()
				u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}
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
	for {
		select {
		case <-u.stopChan:
			return
		case msg := <-u.frameChan:
			idr := isIDRFrame(msg.data)
			sess := u.currentSession()
			if sess == nil || sess.VideoTrack == nil {
				continue
			}
			sampleCount++
			// New session: skip P-frames until the next live IDR arrives.
			// P-frames can't be decoded without their preceding frames.
			if sess.needsIDR {
				if !idr {
					continue
				}
				log.Printf("[webrtc:%s] live IDR arrived (%d bytes), starting stream for new session", u.id, len(msg.data))
				sess.needsIDR = false
			}
			// Duration = capture-time gap since the previous sent sample. The
			// server timestamp resets on reconnect, so guard against going
			// backwards and fall back to the nominal duration.
			frameDuration := captureFrameDuration(lastTs, msg.tsMicros, haveLastTs)
			lastTs, haveLastTs = msg.tsMicros, true
			// Log first few frames and IDRs for diagnostics.
			if sampleCount <= 5 || idr {
				log.Printf("[webrtc:%s] sample #%d: %d bytes, NALUs: %s", u.id, sampleCount, len(msg.data), describeNALUs(msg.data))
			}
			curTicks, remainder := consumeRTPDuration(frameDuration, sess.rtpRemainder)
			sess.rtpRemainder = remainder
			packets := sess.Packetizer.Packetize(msg.data, curTicks)
			packetSizes := make([]int, len(packets))
			for i, packet := range packets {
				packetSizes[i] = packet.MarshalSize()
			}
			schedule := pacingSchedule(packetSizes, u.abr.targetBitrateKbps())
			next, sent := u.writePacedPackets(sess, packets, schedule)
			if next != nil {
				remaining := len(packets) - sent
				if remaining > 0 {
					sess.needsIDR = true
					if u.requestIDR() {
						log.Printf("[webrtc:%s] abandoned %d paced RTP packets for newer frame; requesting IDR", u.id, remaining)
					}
				}
				msg = *next
				select {
				case u.frameChan <- msg:
				default:
					select {
					case <-u.frameChan:
					default:
					}
					u.frameChan <- msg
				}
			}
		}
	}
}

func (u *machineUpstream) writePacedPackets(
	sess *Session,
	packets []*rtp.Packet,
	schedule []time.Duration,
) (*frameMsg, int) {
	start := time.Now()
	for i, packet := range packets {
		wait := time.Until(start.Add(schedule[i]))
		if wait > 0 {
			timer := time.NewTimer(wait)
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
				latest := next
				for {
					select {
					case newer := <-u.frameChan:
						latest = newer
					default:
						return &latest, i
					}
				}
			case <-timer.C:
			}
		}
		if err := sess.VideoTrack.WriteRTP(packet); err != nil {
			log.Printf("[webrtc:%s] write RTP packet error: %v", u.id, err)
		}
	}
	return nil, len(packets)
}
