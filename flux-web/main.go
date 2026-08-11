package main

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"sync"
	"time"

	"github.com/gin-contrib/cors"
	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
	"github.com/pion/ice/v4"
	"github.com/pion/interceptor"
	"github.com/pion/interceptor/pkg/cc"
	"github.com/pion/interceptor/pkg/gcc"
	"github.com/pion/rtcp"
	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
	"github.com/pion/webrtc/v4"
	"golang.org/x/net/ipv4"
	"golang.org/x/net/ipv6"
)

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

var (
	// Shared UDP mux for all WebRTC traffic: a single socket lets us mark
	// packets with a DSCP class and gives a fixed port to open in firewalls.
	iceUDPMux ice.UDPMux
)

// webrtcUDPPort is the single UDP port used for all WebRTC media traffic.
const webrtcUDPPort = 8443

// dscpAF41 is the AF41 (DSCP 34) per-hop behavior for interactive video,
// expressed as the value of the IP TOS byte / IPv6 traffic class (34 << 2).
const dscpAF41 = 34 << 2

// initUDPMux binds the WebRTC media socket and marks its packets AF41 so
// WMM-capable WiFi gear and QoS-enabled routers prioritize the video stream.
// DSCP marking is best-effort: Windows ignores socket-level TOS (use a
// New-NetQosPolicy instead), and some home routers wash the field.
func initUDPMux() error {
	conn, err := net.ListenUDP("udp", &net.UDPAddr{Port: webrtcUDPPort})
	if err != nil {
		return fmt.Errorf("listen udp :%d: %w", webrtcUDPPort, err)
	}
	if err := ipv4.NewConn(conn).SetTOS(dscpAF41); err != nil {
		log.Printf("[webrtc] DSCP(IPv4) not set: %v", err)
	}
	if err := ipv6.NewConn(conn).SetTrafficClass(dscpAF41); err != nil {
		log.Printf("[webrtc] DSCP(IPv6) not set: %v", err)
	}
	iceUDPMux = webrtc.NewICEUDPMux(nil, conn)
	log.Printf("[webrtc] media UDP mux on :%d (DSCP AF41)", webrtcUDPPort)
	return nil
}

// Session wraps a single WebRTC peer connection + video track.
type Session struct {
	PeerConnection      *webrtc.PeerConnection
	VideoTrack          *webrtc.TrackLocalStaticRTP
	Packetizer          rtp.Packetizer
	rtpRemainder        float64
	nextSequenceNumber  uint16
	needsIDR            bool // true until the first IDR is sent to this session
	initialIDRRequested bool
	hasStarted          bool
	machine             *machineUpstream
	release             func()
	releaseOnce         sync.Once
	sessionDone         chan struct{}
	writer              *wsWriter
	bwe                 cc.BandwidthEstimator
}

type wsWriter struct {
	mu sync.Mutex
	ws *websocket.Conn
}

const signalingIdleTimeout = 45 * time.Second
const signalingPingInterval = 15 * time.Second

func (w *wsWriter) write(messageType int, payload []byte) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.ws.WriteMessage(messageType, payload)
}

// frameMsg is one encoded H.264 access unit plus the capture timestamp
// (microseconds since capture start) reported by the Rust server. The
// timestamp lets the pusher pace playout by true capture spacing instead of
// bursty network arrival time.
type frameMsg struct {
	tsMicros uint64
	data     []byte
	// receivedAt is when the relay finished reading the frame off the wire, so
	// the time it then spends queued behind the pusher can be measured.
	receivedAt time.Time
}

type cursorMsg struct {
	tsMicros uint64
	data     []byte
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

func newMediaEngineAndInterceptors() (
	*webrtc.MediaEngine,
	*interceptor.Registry,
	func() cc.BandwidthEstimator,
	error,
) {
	// Use default codecs — lets browser and pion negotiate H.264 profile freely
	m := &webrtc.MediaEngine{}
	if err := m.RegisterDefaultCodecs(); err != nil {
		return nil, nil, nil, fmt.Errorf("register default codecs: %w", err)
	}

	// Default interceptors provide the NACK responder (RTP retransmission of
	// lost packets — essential on lossy links like WiFi), RTCP sender reports
	// for A/V sync, and TWCC feedback. Without them every lost packet
	// corrupts the stream until the next keyframe.
	i := &interceptor.Registry{}
	if err := webrtc.RegisterDefaultInterceptors(m, i); err != nil {
		return nil, nil, nil, fmt.Errorf("register default interceptors: %w", err)
	}

	// Send-side bandwidth estimation over that feedback. It is observational:
	// pacing stays in the relay's frame pusher (hence the no-op pacer here) and
	// the encoder target still comes from abrState, so GCC's estimate can be
	// compared against ours on real links before it is given control.
	var estimator cc.BandwidthEstimator
	ccFactory, err := cc.NewInterceptor(func() (cc.BandwidthEstimator, error) {
		return gcc.NewSendSideBWE(gcc.SendSideBWEPacer(gcc.NewNoOpPacer()))
	})
	if err != nil {
		return nil, nil, nil, fmt.Errorf("create congestion controller: %w", err)
	}
	ccFactory.OnNewPeerConnection(func(_ string, bwe cc.BandwidthEstimator) {
		estimator = bwe
	})
	i.Add(ccFactory)

	// The TWCC writer must be the outermost interceptor so it adds the
	// transport-cc extension before GCC inspects each outbound packet.
	if err := webrtc.ConfigureTWCCHeaderExtensionSender(m, i); err != nil {
		return nil, nil, nil, fmt.Errorf("configure twcc header extension: %w", err)
	}

	return m, i, func() cc.BandwidthEstimator { return estimator }, nil
}

func newSession() (*Session, error) {
	m, i, bandwidthEstimator, err := newMediaEngineAndInterceptors()
	if err != nil {
		return nil, err
	}

	se := webrtc.SettingEngine{}
	if iceUDPMux != nil {
		se.SetICEUDPMux(iceUDPMux)
	}

	api := webrtc.NewAPI(
		webrtc.WithMediaEngine(m),
		webrtc.WithInterceptorRegistry(i),
		webrtc.WithSettingEngine(se),
	)

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
	videoTrack, err := webrtc.NewTrackLocalStaticRTP(
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
	packetizer := rtp.NewPacketizer(
		1200,
		0,
		0,
		&codecs.H264Payloader{},
		rtp.NewRandomSequencer(),
		90000,
	)

	sender, err := pc.AddTrack(videoTrack)
	if err != nil {
		pc.Close()
		return nil, fmt.Errorf("add track: %w", err)
	}

	session := &Session{PeerConnection: pc, VideoTrack: videoTrack, Packetizer: packetizer}
	session.sessionDone = make(chan struct{})
	session.bwe = bandwidthEstimator()
	go logBandwidthEstimate(session)

	// Read RTCP from the browser: on PLI/FIR (decoder lost reference frames,
	// e.g. after WiFi packet loss the NACK window couldn't cover) request a
	// fresh IDR from the capture server so the picture recovers immediately
	// instead of staying corrupted until the next scheduled keyframe.
	go forwardKeyframeRequests(session, sender)

	pc.OnICEConnectionStateChange(func(state webrtc.ICEConnectionState) {
		log.Printf("[webrtc] ICE connection state: %s", state.String())
		if state == webrtc.ICEConnectionStateFailed ||
			state == webrtc.ICEConnectionStateDisconnected ||
			state == webrtc.ICEConnectionStateClosed {
			session.releaseNow()
		}
	})

	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		log.Printf("[webrtc] connection state: %s", state.String())
	})

	return session, nil
}

func (s *Session) releaseNow() {
	s.releaseOnce.Do(func() {
		if s.machine != nil {
			s.machine.clearSession(s)
		}
		if s.release != nil {
			s.release()
		}
		close(s.sessionDone)
	})
}

// bweLogInterval is how often GCC's view of the path is logged. It is a
// diagnostic cadence, not a control loop: slow enough to read in a terminal,
// frequent enough to line up with the keyframe and pacing lines around it.
const bweLogInterval = 2 * time.Second

// logBandwidthEstimate reports GCC's send-side estimate alongside the target
// abrState is actually asking the encoder for, so the two can be compared on a
// real link. GCC sees per-packet arrival times (TWCC) rather than the RTCP
// receiver reports abrState uses, so a persistent gap between the two means our
// loss/RTT signal is the weaker estimator.
func logBandwidthEstimate(session *Session) {
	if session.bwe == nil {
		return
	}
	ticker := time.NewTicker(bweLogInterval)
	defer ticker.Stop()
	for {
		select {
		case <-session.sessionDone:
			return
		case <-ticker.C:
			stats := session.bwe.GetStats()
			var abrTarget uint32
			if session.machine != nil {
				abrTarget = session.machine.abr.targetBitrateKbps()
			}
			log.Printf("[bwe] gcc=%d kbps abr=%d kbps loss=%.1f%% delay=%.1fms (threshold %.1fms) usage=%v state=%v",
				session.bwe.GetTargetBitrate()/1000,
				abrTarget,
				floatStat(stats, "averageLoss")*100,
				floatStat(stats, "delayEstimate"),
				floatStat(stats, "delayThreshold"),
				stats["usage"],
				stats["state"],
			)
		}
	}
}

func floatStat(stats map[string]any, key string) float64 {
	value, _ := stats[key].(float64)
	return value
}

// rttFromReport computes the round-trip time from an RTCP reception report
// (RFC 3550 §6.4.1): RTT = arrival − LSR − DLSR, all in NTP middle-32 units
// of 1/65536 s. Zero LSR means the receiver has not seen a sender report yet.
func rttFromReport(report rtcp.ReceptionReport, arrival time.Time) time.Duration {
	if report.LastSenderReport == 0 {
		return 0
	}
	const ntpEpochOffset = 2208988800 // seconds between 1900 (NTP) and 1970 (Unix)
	secs := uint64(arrival.Unix()) + ntpEpochOffset
	frac := uint64(arrival.Nanosecond()) << 32 / uint64(time.Second)
	nowNTP32 := uint32(secs<<16 | frac>>16)
	elapsed := nowNTP32 - report.LastSenderReport - report.Delay
	// Discard wrapped/garbage values (> ~10s is not a real media RTT).
	if elapsed > 10*65536 {
		return 0
	}
	return time.Duration(elapsed) * time.Second / 65536
}

// forwardKeyframeRequests reads incoming RTCP on the video sender and turns
// PLI/FIR feedback into requests through the upstream's shared IDR gate.
func forwardKeyframeRequests(session *Session, sender *webrtc.RTPSender) {
	for {
		packets, _, err := sender.ReadRTCP()
		if err != nil {
			return
		}
		for _, pkt := range packets {
			if rr, ok := pkt.(*rtcp.ReceiverReport); ok && session.machine != nil {
				for _, report := range rr.Reports {
					session.machine.abr.onReceiverReport(float64(report.FractionLost) / 256.0)
					if rtt := rttFromReport(report, time.Now()); rtt > 0 {
						session.machine.abr.onRTTSample(rtt)
					}
				}
			}
			switch pkt.(type) {
			case *rtcp.PictureLossIndication, *rtcp.FullIntraRequest:
				if session.machine == nil {
					continue
				}
				if !session.machine.requestIDR(idrReasonViewerPLI) {
					continue
				}
				log.Printf("[webrtc] PLI from viewer → requested IDR from upstream")
			}
		}
	}
}

func exchangeOffer(session *Session, offerSDP string) (string, error) {
	if err := session.PeerConnection.SetRemoteDescription(
		webrtc.SessionDescription{
			Type: webrtc.SDPTypeOffer,
			SDP:  offerSDP,
		},
	); err != nil {
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
	SDP       string `json:"sd"`
	MachineID string `json:"machine_id,omitempty"`
	Width     uint32 `json:"width,omitempty"`
	Height    uint32 `json:"height,omitempty"`
}

func handleSignaling(c *gin.Context, registry *machineRegistry) {
	ws, err := upgrader.Upgrade(c.Writer, c.Request, nil)
	if err != nil {
		log.Printf("[ws] upgrade error: %v", err)
		return
	}
	defer ws.Close()
	writer := &wsWriter{ws: ws}
	_ = ws.SetReadDeadline(time.Now().Add(signalingIdleTimeout))
	ws.SetPongHandler(func(string) error {
		return ws.SetReadDeadline(time.Now().Add(signalingIdleTimeout))
	})
	pingDone := make(chan struct{})
	defer close(pingDone)
	go func() {
		ticker := time.NewTicker(signalingPingInterval)
		defer ticker.Stop()
		for {
			select {
			case <-ticker.C:
				if err := ws.WriteControl(
					websocket.PingMessage,
					nil,
					time.Now().Add(5*time.Second),
				); err != nil {
					return
				}
			case <-pingDone:
				return
			}
		}
	}()

	log.Printf("[ws] client connected: %s", c.ClientIP())
	var session *Session
	defer func() {
		if session != nil {
			session.releaseNow()
			session.PeerConnection.Close()
		}
	}()

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
				sendWSError(writer, "Invalid offer")
				continue
			}

			log.Printf("[ws] received offer from %s", c.ClientIP())

			machineID := offerData.MachineID
			if machineID == "" {
				machineID = "default"
			}
			upstream, err := registry.acquire(machineID)
			if err != nil {
				sendWSError(writer, err.Error()+": "+machineID)
				continue
			}

			next, err := newSession()
			if err != nil {
				registry.release(upstream)
				sendWSError(writer, "Failed to create session")
				continue
			}

			next.machine = upstream
			next.release = func() { registry.release(upstream) }
			next.writer = writer

			if old := upstream.bindSession(next); old != nil {
				old.releaseNow()
				old.PeerConnection.Close()
			}
			if session != nil {
				session.releaseNow()
				session.PeerConnection.Close()
			}
			session = next
			if offerData.Width != 0 || offerData.Height != 0 {
				if offerData.Width < 640 || offerData.Height < 480 ||
					offerData.Width > 2560 || offerData.Height > 1440 ||
					offerData.Width%2 != 0 || offerData.Height%2 != 0 {
					next.releaseNow()
					next.PeerConnection.Close()
					sendWSError(writer, "Invalid resolution; use even dimensions from 640x480 through 2560x1440")
					session = nil
					continue
				}
				command := make([]byte, 5)
				command[0] = 0x06
				binary.BigEndian.PutUint16(command[1:3], uint16(offerData.Width))
				binary.BigEndian.PutUint16(command[3:5], uint16(offerData.Height))
				if !upstream.send(command) {
					next.releaseNow()
					next.PeerConnection.Close()
					sendWSError(writer, "Resolution change could not be queued")
					session = nil
					continue
				}
				status, _ := json.Marshal(map[string]any{
					"state":  "transitioning",
					"width":  offerData.Width,
					"height": offerData.Height,
				})
				resp, _ := json.Marshal(WSMessage{Type: "resolution-status", Data: status})
				_ = writer.write(websocket.TextMessage, resp)
			}
			go forwardCursorUpdates(writer, next)
			next.needsIDR = true

			next.PeerConnection.OnICECandidate(func(candidate *webrtc.ICECandidate) {
				if candidate == nil {
					return
				}
				data, _ := json.Marshal(candidate.ToJSON())
				resp, _ := json.Marshal(WSMessage{Type: "new-ice-candidate", Data: data})
				_ = writer.write(websocket.TextMessage, resp)
			})

			answerSDP, err := exchangeOffer(next, offerData.SDP)
			if err != nil {
				log.Printf("[ws] exchange offer error: %v", err)
				next.releaseNow()
				next.PeerConnection.Close()
				sendWSError(writer, "Failed to exchange offer")
				continue
			}

			if upstream.requestInitialIDR(next) {
				log.Printf("[ws] requested initial IDR from upstream")
			}
			answerData, _ := json.Marshal(map[string]string{"sd": answerSDP})
			resp, _ := json.Marshal(WSMessage{Type: "answer", Data: answerData})
			_ = writer.write(websocket.TextMessage, resp)
			log.Printf("[ws] sent answer to %s", c.ClientIP())
		case "new-ice-candidate":
			var candidate webrtc.ICECandidateInit
			if err := json.Unmarshal(msg.Data, &candidate); err == nil && session != nil && candidate.Candidate != "" {
				if err := session.PeerConnection.AddICECandidate(candidate); err != nil {
					log.Printf("[ws] add ICE candidate error: %v", err)
				}
			}

		case "latency-ping":
			// Echo for control-path RTT measurement: same WS + goroutine that
			// carries input events, so it measures what input actually rides.
			resp, _ := json.Marshal(WSMessage{Type: "latency-pong", Data: msg.Data})
			_ = writer.write(websocket.TextMessage, resp)

		case "input":
			if session == nil || session.machine == nil {
				sendWSError(writer, "No active machine session")
				continue
			}
			payload := []byte(msg.Data)
			// Forward input event to the selected machine's frame server.
			// Protocol: [0x02][4-byte len][JSON payload]
			// We receive just the JSON payload in msg.Data.
			packet := make([]byte, 5+len(payload))
			packet[0] = 0x02
			binary.BigEndian.PutUint32(packet[1:5], uint32(len(payload)))
			copy(packet[5:], payload)
			if !session.machine.send(packet) {
				log.Printf("[ws] upstream command channel full, dropped input event")
			}
		case "quality":
			if session == nil || session.machine == nil {
				sendWSError(writer, "No active machine session")
				continue
			}
			var control struct {
				Level uint8 `json:"level"`
				FPS   uint8 `json:"fps"`
			}
			if err := json.Unmarshal(msg.Data, &control); err != nil || control.Level > 10 || control.FPS > 144 {
				sendWSError(writer, "Quality must be 0-10 and FPS must be 0-144")
				continue
			}
			if !session.machine.send([]byte{0x05, control.Level, control.FPS}) {
				log.Printf("[ws] upstream command channel full, dropped quality/FPS control")
			}
		default:
			log.Printf("[ws] unknown message type: %s", msg.Type)
		}
	}
}

func sendWSError(writer *wsWriter, msg string) {
	data, _ := json.Marshal(map[string]string{"error": msg})
	resp, _ := json.Marshal(WSMessage{Type: "error", Data: data})
	_ = writer.write(websocket.TextMessage, resp)
}

func forwardCursorUpdates(writer *wsWriter, session *Session) {
	ticker := time.NewTicker(16 * time.Millisecond)
	defer ticker.Stop()
	var latest *cursorMsg
	if session.machine != nil {
		session.machine.mu.Lock()
		if session.machine.lastCursor != nil {
			copy := *session.machine.lastCursor
			latest = &copy
		}
		session.machine.mu.Unlock()
	}
	for {
		select {
		case <-session.sessionDone:
			return
		case msg := <-session.machine.cursorChan:
			latest = &msg
		case <-ticker.C:
			if latest == nil {
				continue
			}
			resp, err := json.Marshal(WSMessage{Type: "cursor", Data: json.RawMessage(latest.data)})
			if err == nil {
				if err := writer.write(websocket.TextMessage, resp); err != nil {
					return
				}
			}
			latest = nil
		}
	}
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds | log.Lshortfile)
	frameServerAddr := os.Getenv("FLUX_SERVER_ADDR")
	if frameServerAddr == "" {
		frameServerAddr = "127.0.0.1:8556"
	}
	registry := newMachineRegistry()
	// The relay remains the frame connection initiator. This static entry
	// preserves single-host deployments and legacy offers without a machine ID.
	registry.seedStatic("default", "Configured Flux Server", frameServerAddr)
	if err := initUDPMux(); err != nil {
		log.Printf("[webrtc] UDP mux unavailable, falling back to ephemeral ports: %v", err)
	}
	webAddr := ":8080"
	gin.SetMode(gin.ReleaseMode)
	r := gin.Default()
	r.Use(cors.Default())
	registerMachineRoutes(r, registry)
	r.GET("/ws/signaling", func(c *gin.Context) { handleSignaling(c, registry) })
	r.NoRoute(gin.WrapH(http.FileServer(http.Dir("./ui/out"))))
	log.Printf("flux-web listening on http://localhost%s", webAddr)
	if err := r.Run(webAddr); err != nil {
		log.Fatalf("server error: %v", err)
	}
}
