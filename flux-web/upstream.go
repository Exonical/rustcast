package main

import (
	"context"
	"crypto/tls"
	"encoding/binary"
	"fmt"
	"io"
	"log"
	"net"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/webrtc/v4/pkg/media"
	"github.com/quic-go/quic-go"
)

// ---------------------------------------------------------------------------
// TCP/QUIC frame reader — connects to flux-server's frame server
// ---------------------------------------------------------------------------

type machineUpstream struct {
	id          string
	addr        string
	frameChan   chan frameMsg
	commandChan chan []byte
	abr         *abrState
	stopChan    chan struct{}
	stopOnce    sync.Once
	viewers     int // Protected by machineRegistry.mu.
	viewerCount atomic.Uint32
	mu          sync.Mutex
	session     *Session
	conn        net.Conn
	cancel      context.CancelFunc
	status      func(string)
}

const (
	defaultFrameDuration = 16 * time.Millisecond // ~60fps for the first sample
	minFrameDuration     = 4 * time.Millisecond  // clamp absurdly fast bursts
	maxSaneFrameDuration = 30 * time.Second      // cap corrupted/absurd timestamps
)

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
		// Protocol: [8-byte BE capture-ts µs][4-byte BE length][H.264 data]
		var hdr [12]byte
		if _, err := io.ReadFull(conn, hdr[:]); err != nil {
			return fmt.Errorf("read header: %w", err)
		}
		tsMicros := binary.BigEndian.Uint64(hdr[0:8])
		frameLen := binary.BigEndian.Uint32(hdr[8:12])
		if frameLen == 0 || frameLen > 10*1024*1024 {
			return fmt.Errorf("invalid frame length: %d", frameLen)
		}
		// Read frame data.
		data := make([]byte, frameLen)
		if _, err := io.ReadFull(conn, data); err != nil {
			return fmt.Errorf("read frame data: %w", err)
		}
		frameCount++
		u.abr.countFrameBytes(len(data))
		if frameCount%300 == 0 {
			log.Printf("[frame:%s] received %d frames (last=%d bytes)", u.id, frameCount, frameLen)
		}
		// Non-blocking send; drop oldest frame if channel is full.
		select {
		case u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}:
		default:
			// Drop oldest frame if channel is full.
			select {
			case <-u.frameChan:
			default:
			}
			u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}
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
		// Frame stream: [8-byte BE capture-ts µs][4-byte BE length][H.264 data]
		var hdr [12]byte
		if _, err := io.ReadFull(stream, hdr[:]); err != nil {
			continue
		}
		tsMicros := binary.BigEndian.Uint64(hdr[0:8])
		frameLen := binary.BigEndian.Uint32(hdr[8:12])
		if frameLen == 0 || frameLen > 10*1024*1024 {
			continue
		}
		data := make([]byte, frameLen)
		if _, err := io.ReadFull(stream, data); err != nil {
			continue
		}
		frameCount++
		u.abr.countFrameBytes(len(data))
		if frameCount%300 == 0 {
			log.Printf("[frame:%s] received %d frames via quic (last=%d bytes)", u.id, frameCount, frameLen)
		}
		select {
		case u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}:
		default:
			select {
			case <-u.frameChan:
			default:
			}
			u.frameChan <- frameMsg{tsMicros: tsMicros, data: data}
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
			if err := sess.VideoTrack.WriteSample(media.Sample{Data: msg.data, Duration: frameDuration}); err != nil {
				log.Printf("[webrtc:%s] write sample error: %v", u.id, err)
			}
		}
	}
}
