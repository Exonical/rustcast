package main

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/gin-contrib/cors"
	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
	"github.com/pion/webrtc/v4"
	"github.com/pion/webrtc/v4/pkg/media"
)

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

var (
	// Current active WebRTC session (single-viewer for now)
	currentSession   *Session
	currentSessionMu sync.Mutex

	// Latest H.264 frame from the Rust capture server
	frameChan = make(chan frameMsg, 120)

	// Command channel to send requests to the Rust server
	upstreamCommandChan = make(chan []byte, 100)
)

// Session wraps a single WebRTC peer connection + video track.
type Session struct {
	PeerConnection *webrtc.PeerConnection
	VideoTrack     *webrtc.TrackLocalStaticSample
	needsIDR       bool // true until the first IDR is sent to this session
}

// ---------------------------------------------------------------------------
// TCP frame reader — connects to flux-server's frame server
// ---------------------------------------------------------------------------

// frameMsg is one encoded H.264 access unit plus the capture timestamp
// (microseconds since capture start) reported by the Rust server. The
// timestamp lets the pusher pace playout by true capture spacing instead of
// bursty network arrival time.
type frameMsg struct {
	tsMicros uint64
	data     []byte
}

func connectFrameServer(addr string) {
	for {
		log.Printf("[frame] connecting to %s ...", addr)
		conn, err := net.Dial("tcp", addr)
		if err != nil {
			log.Printf("[frame] connection failed: %v, retrying in 2s", err)
			time.Sleep(2 * time.Second)
			continue
		}
		log.Printf("[frame] connected to %s", addr)

		// Spawn writer for upstream commands
		done := make(chan struct{})
		go func() {
			for {
				select {
				case cmd := <-upstreamCommandChan:
					if _, err := conn.Write(cmd); err != nil {
						log.Printf("[frame] write command error: %v", err)
						return
					}
				case <-done:
					return
				}
			}
		}()

		err = readFrames(conn)
		close(done) // Stop the writer
		conn.Close()
		if err != nil {
			log.Printf("[frame] read error: %v, reconnecting in 1s", err)
		}
		time.Sleep(1 * time.Second)
	}
}

func readFrames(conn net.Conn) error {
	var frameCount uint64
	for {
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

		// Read frame data
		data := make([]byte, frameLen)
		if _, err := io.ReadFull(conn, data); err != nil {
			return fmt.Errorf("read frame data: %w", err)
		}

		frameCount++
		if frameCount%300 == 0 {
			log.Printf("[frame] received %d frames (last=%d bytes)", frameCount, frameLen)
		}

		msg := frameMsg{tsMicros: tsMicros, data: data}
		// Non-blocking send to frameChan
		select {
		case frameChan <- msg:
		default:
			// Drop oldest frame if channel is full
			select {
			case <-frameChan:
			default:
			}
			frameChan <- msg
		}
	}
}

// ---------------------------------------------------------------------------
// Frame pusher — writes frames from frameChan to the active WebRTC track
// ---------------------------------------------------------------------------

func framePusher() {
	// The capture source (e.g. a Wayland/mutter screen-cast) delivers frames at
	// a variable, damage-driven rate and the server drains backlogs in bursts,
	// so network arrival time is a poor clock. Pace the RTP timestamp by the
	// frame's capture timestamp (reported by the server) instead, so the
	// browser's jitter buffer can absorb bursts and play at true capture
	// cadence rather than running ahead and stalling.
	const (
		defaultFrameDuration = 16 * time.Millisecond  // ~60fps for the first sample
		minFrameDuration     = 4 * time.Millisecond   // clamp absurdly fast bursts
		maxFrameDuration     = 500 * time.Millisecond // clamp long idle gaps
	)
	var (
		sampleCount uint64
		lastTs      uint64
		haveLastTs  bool
	)

	for msg := range frameChan {
		idr := isIDRFrame(msg.data)

		currentSessionMu.Lock()
		sess := currentSession
		currentSessionMu.Unlock()

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
			log.Printf("[webrtc] live IDR arrived (%d bytes), starting stream for new session", len(msg.data))
			sess.needsIDR = false
		}

		// Duration = capture-time gap since the previous sent sample. The
		// server timestamp resets on reconnect, so guard against going
		// backwards and fall back to the nominal duration.
		frameDuration := defaultFrameDuration
		if haveLastTs && msg.tsMicros > lastTs {
			frameDuration = time.Duration(msg.tsMicros-lastTs) * time.Microsecond
			if frameDuration < minFrameDuration {
				frameDuration = minFrameDuration
			} else if frameDuration > maxFrameDuration {
				frameDuration = maxFrameDuration
			}
		}
		lastTs = msg.tsMicros
		haveLastTs = true

		// Log first few frames and IDRs for diagnostics
		if sampleCount <= 5 || (idr && sampleCount > 5) {
			naluTypes := describeNALUs(msg.data)
			log.Printf("[webrtc] sample #%d: %d bytes, NALUs: %s", sampleCount, len(msg.data), naluTypes)
		}

		err := sess.VideoTrack.WriteSample(media.Sample{
			Data:     msg.data,
			Duration: frameDuration,
		})
		if err != nil {
			log.Printf("[webrtc] write sample error: %v", err)
		}
	}
}

// describeNALUs parses Annex B start codes and returns NALU type descriptions.
func describeNALUs(data []byte) string {
	var types []string
	i := 0
	for i < len(data)-4 {
		// Look for start code 00 00 00 01 or 00 00 01
		if data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1 {
			if i+4 < len(data) {
				naluType := data[i+4] & 0x1F
				types = append(types, naluTypeName(naluType))
			}
			i += 4
		} else if data[i] == 0 && data[i+1] == 0 && data[i+2] == 1 {
			if i+3 < len(data) {
				naluType := data[i+3] & 0x1F
				types = append(types, naluTypeName(naluType))
			}
			i += 3
		} else {
			i++
		}
	}
	if len(types) == 0 {
		return fmt.Sprintf("no-start-codes (first 8 bytes: %X)", data[:min(8, len(data))])
	}
	result := ""
	for i, t := range types {
		if i > 0 {
			result += ", "
		}
		result += t
	}
	return result
}

// isIDRFrame checks if the H.264 Annex B data contains an IDR NALU (type 5).
func isIDRFrame(data []byte) bool {
	i := 0
	for i < len(data)-4 {
		if data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1 {
			if i+4 < len(data) && (data[i+4]&0x1F) == 5 {
				return true
			}
			i += 4
		} else if data[i] == 0 && data[i+1] == 0 && data[i+2] == 1 {
			if i+3 < len(data) && (data[i+3]&0x1F) == 5 {
				return true
			}
			i += 3
		} else {
			i++
		}
	}
	return false
}

func naluTypeName(t byte) string {
	switch t {
	case 1:
		return "P-slice"
	case 5:
		return "IDR"
	case 6:
		return "SEI"
	case 7:
		return "SPS"
	case 8:
		return "PPS"
	case 9:
		return "AUD"
	default:
		return fmt.Sprintf("type-%d", t)
	}
}

// ---------------------------------------------------------------------------
// WebRTC session management
// ---------------------------------------------------------------------------

func newSession() (*Session, error) {
	// Use default codecs — lets browser and pion negotiate H.264 profile freely
	m := &webrtc.MediaEngine{}
	if err := m.RegisterDefaultCodecs(); err != nil {
		return nil, fmt.Errorf("register default codecs: %w", err)
	}

	api := webrtc.NewAPI(webrtc.WithMediaEngine(m))

	config := webrtc.Configuration{
		ICEServers: []webrtc.ICEServer{
			{URLs: []string{"stun:stun.l.google.com:19302"}},
		},
	}

	pc, err := api.NewPeerConnection(config)
	if err != nil {
		return nil, fmt.Errorf("create peer connection: %w", err)
	}

	// Create H.264 video track
	videoTrack, err := webrtc.NewTrackLocalStaticSample(
		webrtc.RTPCodecCapability{
			MimeType:  webrtc.MimeTypeH264,
			ClockRate: 90000,
		},
		"video", "flux-screen",
	)
	if err != nil {
		pc.Close()
		return nil, fmt.Errorf("create video track: %w", err)
	}

	if _, err = pc.AddTrack(videoTrack); err != nil {
		pc.Close()
		return nil, fmt.Errorf("add track: %w", err)
	}

	pc.OnICEConnectionStateChange(func(state webrtc.ICEConnectionState) {
		log.Printf("[webrtc] ICE connection state: %s", state.String())
		if state == webrtc.ICEConnectionStateFailed || state == webrtc.ICEConnectionStateDisconnected || state == webrtc.ICEConnectionStateClosed {
			currentSessionMu.Lock()
			if currentSession != nil && currentSession.PeerConnection == pc {
				currentSession = nil
				log.Printf("[webrtc] session cleared")
			}
			currentSessionMu.Unlock()
		}
	})

	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		log.Printf("[webrtc] connection state: %s", state.String())
	})

	return &Session{
		PeerConnection: pc,
		VideoTrack:     videoTrack,
	}, nil
}

func exchangeOffer(session *Session, offerSDP string) (string, error) {
	offer := webrtc.SessionDescription{
		Type: webrtc.SDPTypeOffer,
		SDP:  offerSDP,
	}

	if err := session.PeerConnection.SetRemoteDescription(offer); err != nil {
		return "", fmt.Errorf("set remote description: %w", err)
	}

	answer, err := session.PeerConnection.CreateAnswer(nil)
	if err != nil {
		return "", fmt.Errorf("create answer: %w", err)
	}

	if err := session.PeerConnection.SetLocalDescription(answer); err != nil {
		return "", fmt.Errorf("set local description: %w", err)
	}

	// Wait for ICE gathering to complete
	gatherComplete := webrtc.GatheringCompletePromise(session.PeerConnection)
	<-gatherComplete

	return session.PeerConnection.LocalDescription().SDP, nil
}

// ---------------------------------------------------------------------------
// WebSocket signaling
// ---------------------------------------------------------------------------

var upgrader = websocket.Upgrader{
	CheckOrigin: func(r *http.Request) bool { return true },
}

type WSMessage struct {
	Type string          `json:"type"`
	Data json.RawMessage `json:"data"`
}

type OfferData struct {
	SDP string `json:"sd"`
}

func handleSignaling(c *gin.Context) {
	ws, err := upgrader.Upgrade(c.Writer, c.Request, nil)
	if err != nil {
		log.Printf("[ws] upgrade error: %v", err)
		return
	}
	defer ws.Close()

	log.Printf("[ws] client connected: %s", c.ClientIP())

	for {
		_, msgBytes, err := ws.ReadMessage()
		if err != nil {
			log.Printf("[ws] read error: %v", err)
			return
		}

		var msg WSMessage
		if err := json.Unmarshal(msgBytes, &msg); err != nil {
			log.Printf("[ws] parse error: %v", err)
			continue
		}

		switch msg.Type {
		case "offer":
			var offerData OfferData
			if err := json.Unmarshal(msg.Data, &offerData); err != nil {
				log.Printf("[ws] parse offer error: %v", err)
				continue
			}

			log.Printf("[ws] received offer from %s", c.ClientIP())

			session, err := newSession()
			if err != nil {
				log.Printf("[ws] create session error: %v", err)
				sendWSError(ws, "Failed to create session")
				continue
			}

			// Set up ICE candidate trickle to client
			session.PeerConnection.OnICECandidate(func(candidate *webrtc.ICECandidate) {
				if candidate == nil {
					return
				}
				candidateJSON := candidate.ToJSON()
				data, _ := json.Marshal(candidateJSON)
				resp := WSMessage{Type: "new-ice-candidate", Data: data}
				respBytes, _ := json.Marshal(resp)
				ws.WriteMessage(websocket.TextMessage, respBytes)
			})

			answerSDP, err := exchangeOffer(session, offerData.SDP)
			if err != nil {
				log.Printf("[ws] exchange offer error: %v", err)
				session.PeerConnection.Close()
				sendWSError(ws, "Failed to exchange offer")
				continue
			}

			// Replace current session
			currentSessionMu.Lock()
			if currentSession != nil {
				currentSession.PeerConnection.Close()
			}
			currentSession = session
			currentSession.needsIDR = true
			currentSessionMu.Unlock()

			// Request an immediate IDR frame from the Rust server
			select {
			case upstreamCommandChan <- []byte{0x01}:
				log.Printf("[ws] requested IDR from upstream")
			default:
				log.Printf("[ws] upstream command channel full, dropped IDR request")
			}

			// Send answer back
			answerData, _ := json.Marshal(map[string]string{"sd": answerSDP})
			resp := WSMessage{Type: "answer", Data: answerData}
			respBytes, _ := json.Marshal(resp)
			ws.WriteMessage(websocket.TextMessage, respBytes)
			log.Printf("[ws] sent answer to %s", c.ClientIP())

		case "new-ice-candidate":
			var candidate webrtc.ICECandidateInit
			if err := json.Unmarshal(msg.Data, &candidate); err != nil {
				log.Printf("[ws] parse ICE candidate error: %v", err)
				continue
			}

			if candidate.Candidate == "" {
				continue
			}

			currentSessionMu.Lock()
			sess := currentSession
			currentSessionMu.Unlock()

			if sess == nil {
				log.Printf("[ws] no active session for ICE candidate")
				continue
			}

			if err := sess.PeerConnection.AddICECandidate(candidate); err != nil {
				log.Printf("[ws] add ICE candidate error: %v", err)
			}

		case "input":
			// Forward input event to Rust frame server
			// Protocol: [0x02][4-byte len][JSON payload]
			// We receive just the JSON payload in msg.Data

			// 1. Calculate length
			payload := []byte(msg.Data)
			payloadLen := uint32(len(payload))

			// 2. Construct packet
			// [0x02] + [len (4 bytes)] + [payload]
			packet := make([]byte, 1+4+len(payload))
			packet[0] = 0x02
			binary.BigEndian.PutUint32(packet[1:5], payloadLen)
			copy(packet[5:], payload)

			// 3. Send to upstream channel (non-blocking drop if full)
			select {
			case upstreamCommandChan <- packet:
				// log.Printf("[ws] forwarded input event (%d bytes)", len(payload))
			default:
				log.Printf("[ws] upstream command channel full, dropped input event")
			}

		default:
			log.Printf("[ws] unknown message type: %s", msg.Type)
		}
	}
}

func sendWSError(ws *websocket.Conn, msg string) {
	data, _ := json.Marshal(map[string]string{"error": msg})
	resp := WSMessage{Type: "error", Data: data}
	respBytes, _ := json.Marshal(resp)
	ws.WriteMessage(websocket.TextMessage, respBytes)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds | log.Lshortfile)

	frameServerAddr := "127.0.0.1:8556"
	webAddr := ":8080"

	// Start TCP frame reader (connects to Rust flux-server)
	go connectFrameServer(frameServerAddr)

	// Start frame pusher (writes to WebRTC track)
	go framePusher()

	// HTTP server
	gin.SetMode(gin.ReleaseMode)
	r := gin.Default()
	r.Use(cors.Default())

	// WebSocket signaling endpoint
	r.GET("/ws/signaling", handleSignaling)

	// Serve the Next.js static export from ui/out/
	// Use NoRoute to avoid conflict with /ws/* routes
	r.NoRoute(gin.WrapH(http.FileServer(http.Dir("./ui/out"))))

	log.Printf("flux-web listening on http://localhost%s", webAddr)
	if err := r.Run(webAddr); err != nil {
		log.Fatalf("server error: %v", err)
	}
}
