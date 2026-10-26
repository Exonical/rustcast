package main

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/sha256"
	"crypto/tls"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/url"
	"testing"
	"time"

	"github.com/quic-go/quic-go"
	"github.com/quic-go/webtransport-go"
)

func TestVideoFrameHeaderEncoding(t *testing.T) {
	frame := frameMsg{
		tsMicros: 0x1112131415161718,
		data:     []byte{1, 2, 3},
	}
	header := encodeVideoFrameHeader(0x01020304, frame, true)
	if header[0] != streamVideo || binary.BigEndian.Uint32(header[1:5]) != 0x01020304 ||
		binary.BigEndian.Uint64(header[5:13]) != frame.tsMicros || header[13] != 1 ||
		binary.BigEndian.Uint32(header[14:18]) != 3 {
		t.Fatalf("unexpected video stream header: %x", header)
	}
}

func TestSendResolutionChangeQueuesCommandUnlessFull(t *testing.T) {
	u := newMachineUpstream("a", "m", nil, func(string) {})
	if !u.sendResolutionChange(1920, 1080) {
		t.Fatal("sendResolutionChange with free command channel = false, want true")
	}
	if cmd := <-u.commandChan; !bytes.Equal(cmd, []byte{cmdResolution, 0x07, 0x80, 0x04, 0x38}) {
		t.Fatalf("resolution command = %x, want 0607800438", cmd)
	}
	for len(u.commandChan) < cap(u.commandChan) {
		u.commandChan <- nil
	}
	if u.sendResolutionChange(1920, 1080) {
		t.Fatal("sendResolutionChange with full command channel = true, want false")
	}
}

func TestResolutionParseErrorsShareInvalidResolution(t *testing.T) {
	for _, query := range []string{"width=x&height=480", "width=640&height=x", "width=641&height=480"} {
		r := &http.Request{URL: &url.URL{RawQuery: query}}
		if _, _, err := requestedResolution(r); err != errInvalidResolution {
			t.Fatalf("requestedResolution(%q) error = %v, want errInvalidResolution", query, err)
		}
	}
}

func TestStaleFrameResetDecision(t *testing.T) {
	tests := []struct {
		name       string
		keyframe   bool
		writingFor time.Duration
		queued     int
		want       bool
	}{
		{name: "queued delta over limit", writingFor: 17 * time.Millisecond, queued: 1, want: true},
		{name: "queued delta at limit", writingFor: 16 * time.Millisecond, queued: 1},
		{name: "no newer frame", writingFor: 20 * time.Millisecond},
		{name: "never reset keyframe", keyframe: true, writingFor: time.Second, queued: 1},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := shouldResetFrame(test.keyframe, test.writingFor, test.queued); got != test.want {
				t.Fatalf("shouldResetFrame() = %t, want %t", got, test.want)
			}
		})
	}
}

func TestRelayCertificatePropertiesAndHash(t *testing.T) {
	now := time.Now()
	manager, err := newRelayCertificateManager("", "", "relay.example.test")
	if err != nil {
		t.Fatal(err)
	}
	certificate := manager.certificate.Load()
	if certificate == nil || certificate.Leaf == nil {
		t.Fatal("generated certificate is missing")
	}
	leaf := certificate.Leaf
	key, ok := certificate.PrivateKey.(*ecdsa.PrivateKey)
	if !ok || key.Curve.Params().BitSize != 256 {
		t.Fatalf("certificate key = %T, want ECDSA P-256", certificate.PrivateKey)
	}
	if remaining := leaf.NotAfter.Sub(now); remaining < selfSignedCertificateLifetime-time.Minute ||
		remaining > selfSignedCertificateLifetime+time.Minute {
		t.Fatalf("certificate lifetime remaining = %v, want about %v", remaining, selfSignedCertificateLifetime)
	}
	for _, host := range []string{"localhost", "relay.example.test", "127.0.0.1", "::1"} {
		if err := leaf.VerifyHostname(host); err != nil {
			t.Fatalf("certificate is missing SAN %q: %v", host, err)
		}
	}
	hash := sha256.Sum256(certificate.Certificate[0])
	if hashes := manager.certificateHashes(); len(hashes) != 1 || hashes[0] != base64.StdEncoding.EncodeToString(hash[:]) {
		t.Fatalf("certificate hashes = %v, want current DER SHA-256", hashes)
	}
	config := manager.tlsConfig()
	if config.MinVersion != tls.VersionTLS13 || config.MaxVersion != tls.VersionTLS13 ||
		len(config.NextProtos) != 1 || config.NextProtos[0] != "h3" {
		t.Fatalf("TLS config = %+v, want TLS 1.3 and h3 ALPN", config)
	}
	if configuredHashes := (&relayCertificateManager{configured: true}).certificateHashes(); len(configuredHashes) != 0 {
		t.Fatalf("configured certificate hashes = %v, want empty", configuredHashes)
	}
}

func TestLoopbackWebTransportVideoAndKeyframeRequest(t *testing.T) {
	registry := newMachineRegistry()
	sessions := newSessionRegistry()
	registry.seedStatic("default", "test", "127.0.0.1:0")
	upstream := newMachineUpstream("", "default", nil, func(string) {})
	registry.mu.Lock()
	registry.machines["default"].upstream = upstream
	registry.mu.Unlock()
	go upstream.framePusher()
	defer upstream.stop()

	certificates, err := newRelayCertificateManager("", "", "localhost")
	if err != nil {
		t.Fatal(err)
	}
	server := newWebTransportServer(registry, sessions, certificates, nil, nil, nil)
	packetConn, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	serveDone := make(chan struct{})
	go func() {
		defer close(serveDone)
		_ = server.Serve(packetConn)
	}()
	defer func() {
		_ = server.Close()
		_ = packetConn.Close()
		select {
		case <-serveDone:
		case <-time.After(time.Second):
			t.Error("WebTransport server did not stop")
		}
	}()
	time.Sleep(25 * time.Millisecond)

	address := packetConn.LocalAddr().String()
	transport := &webtransport.Transport{
		TLSClientConfig: &tls.Config{
			InsecureSkipVerify: true,
			NextProtos:         []string{"h3"},
		},
		QUICConfig: &quic.Config{
			EnableDatagrams:                  true,
			EnableStreamResetPartialDelivery: true,
		},
	}
	defer transport.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	target := (&url.URL{Scheme: "https", Host: address, Path: "/wt", RawQuery: "machine=default"}).String()
	_, clientSession, err := transport.Dial(ctx, target, nil)
	if err != nil {
		t.Fatalf("dial WebTransport session: %v", err)
	}
	defer clientSession.CloseWithError(0, "test complete")
	control, err := clientSession.OpenStreamSync(ctx)
	if err != nil {
		t.Fatalf("open client control stream: %v", err)
	}
	writeTestControlMessage(t, control, controlMessage{
		Type: "latency-ping",
		Data: json.RawMessage(`{"t":0}`),
	})
	connected := readTestControlMessage(t, control)
	if connected.Type != "session" {
		t.Fatalf("first control message type = %q, want session", connected.Type)
	}
	expectClipboardStatus(t, control)
	var sessionID struct {
		ID string `json:"id"`
	}
	if err := json.Unmarshal(connected.Data, &sessionID); err != nil {
		t.Fatalf("decode session message: %v", err)
	}
	if len(sessionID.ID) != 32 {
		t.Fatalf("session id = %q, want 32 hex characters", sessionID.ID)
	}
	for _, char := range sessionID.ID {
		if !((char >= '0' && char <= '9') || (char >= 'a' && char <= 'f')) {
			t.Fatalf("session id = %q, want lowercase hexadecimal", sessionID.ID)
		}
	}
	listed := sessions.list()
	if len(listed) != 1 || listed[0].ID != sessionID.ID || listed[0].MachineID != "default" ||
		listed[0].Width != 0 || listed[0].Height != 0 {
		t.Fatalf("unexpected session registry entry: %+v", listed)
	}
	pong := readTestControlMessage(t, control)
	if pong.Type != "latency-pong" {
		t.Fatalf("second control message type = %q, want latency-pong", pong.Type)
	}
	waitForTestCondition(t, 3*time.Second, func() bool {
		return upstream.currentSession() != nil
	})

	waitForUpstreamIDR(t, upstream)
	draining := true
	for draining {
		select {
		case <-upstream.commandChan:
		default:
			draining = false
		}
	}
	writeTestControlMessage(t, control, controlMessage{
		Type: "resolution",
		Data: json.RawMessage(`{"width":641,"height":480}`),
	})
	rejected := readTestControlMessage(t, control)
	if rejected.Type != "error" {
		t.Fatalf("invalid resolution response type = %q, want error", rejected.Type)
	}
	writeTestControlMessage(t, control, controlMessage{
		Type: "latency-ping",
		Data: json.RawMessage(`{"t":1}`),
	})
	if continued := readTestControlMessage(t, control); continued.Type != "latency-pong" {
		t.Fatalf("control response after invalid resolution = %q, want latency-pong", continued.Type)
	}
	writeTestControlMessage(t, control, controlMessage{
		Type: "resolution",
		Data: json.RawMessage(`{"width":1280,"height":720}`),
	})
	select {
	case command := <-upstream.commandChan:
		want := []byte{cmdResolution, 0x05, 0x00, 0x02, 0xd0}
		if string(command) != string(want) {
			t.Fatalf("upstream resolution command = %x, want %x", command, want)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("resolution request did not reach upstream as cmdResolution")
	}
	statusStream, err := clientSession.AcceptUniStream(ctx)
	if err != nil {
		t.Fatalf("accept resolution status stream: %v", err)
	}
	var statusHeader [5]byte
	if _, err := io.ReadFull(statusStream, statusHeader[:]); err != nil {
		t.Fatalf("read resolution status header: %v", err)
	}
	if statusHeader[0] != streamResolutionStatus {
		t.Fatalf("resolution status stream type = %02x, want streamResolutionStatus", statusHeader[0])
	}
	statusPayload := make([]byte, binary.BigEndian.Uint32(statusHeader[1:]))
	if _, err := io.ReadFull(statusStream, statusPayload); err != nil {
		t.Fatalf("read resolution status payload: %v", err)
	}
	var status struct {
		State  string `json:"state"`
		Width  uint32 `json:"width"`
		Height uint32 `json:"height"`
	}
	if err := json.Unmarshal(statusPayload, &status); err != nil {
		t.Fatalf("decode resolution status: %v", err)
	}
	if status.State != "transitioning" || status.Width != 1280 || status.Height != 720 {
		t.Fatalf("resolution status = %+v, want transitioning 1280x720", status)
	}
	upstream.mu.Lock()
	upstream.idrGate.last = time.Now().Add(-idrRequestInterval)
	upstream.mu.Unlock()
	writeTestControlMessage(t, control, controlMessage{Type: "keyframe-request"})
	select {
	case command := <-upstream.commandChan:
		if len(command) != 1 || command[0] != cmdIDR {
			t.Fatalf("upstream command = %x, want cmdIDR", command)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("keyframe-request did not reach upstream as cmdIDR")
	}

	payload := []byte{0, 0, 0, 1, 0x65, 1, 2, 3}
	if upstream.queueFrame(frameMsg{tsMicros: 123456, data: payload, receivedAt: time.Now()}) {
		t.Fatal("test frame was unexpectedly dropped")
	}
	video, err := clientSession.AcceptUniStream(ctx)
	if err != nil {
		t.Fatalf("accept video stream: %v", err)
	}
	var header [18]byte
	if _, err := io.ReadFull(video, header[:]); err != nil {
		t.Fatalf("read video header: %v", err)
	}
	if header[0] != 1 || binary.BigEndian.Uint32(header[1:5]) != 0 ||
		binary.BigEndian.Uint64(header[5:13]) != 123456 || header[13] != 1 ||
		binary.BigEndian.Uint32(header[14:18]) != uint32(len(payload)) {
		t.Fatalf("unexpected loopback video header: %x", header)
	}
	received := make([]byte, len(payload))
	if _, err := io.ReadFull(video, received); err != nil {
		t.Fatalf("read video payload: %v", err)
	}
	if string(received) != string(payload) {
		t.Fatalf("video payload = %x, want %x", received, payload)
	}
	disconnected := make(chan bool, 1)
	go func() { disconnected <- sessions.disconnect(sessionID.ID) }()
	ended := readTestControlMessage(t, control)
	if ended.Type != "session-ended" {
		t.Fatalf("end control type = %q, want session-ended", ended.Type)
	}
	var endedData struct {
		Reason string `json:"reason"`
	}
	if err := json.Unmarshal(ended.Data, &endedData); err != nil || endedData.Reason == "" {
		t.Fatalf("session-ended reason = %+v, err=%v", endedData, err)
	}
	_ = clientSession.CloseWithError(0, "session-ended received")
	if !<-disconnected {
		t.Fatal("active session could not be disconnected")
	}
	waitForTestCondition(t, time.Second, func() bool {
		return len(sessions.list()) == 0 && registry.list()[0].Viewers == 0
	})
}

func TestAudioControlMessageTogglesFlag(t *testing.T) {
	s := &Session{}
	s.audioEnabled.Store(true)
	if err := s.handleControlMessage(controlMessage{
		Type: "audio",
		Data: json.RawMessage(`{"enabled":false}`),
	}); err != nil {
		t.Fatalf("handleControlMessage(audio) = %v", err)
	}
	if s.audioEnabled.Load() {
		t.Fatal("audioEnabled still true after disable")
	}
	if err := s.handleControlMessage(controlMessage{
		Type: "audio",
		Data: json.RawMessage(`{"enabled":true}`),
	}); err != nil || !s.audioEnabled.Load() {
		t.Fatalf("audio re-enable failed: err=%v enabled=%v", err, s.audioEnabled.Load())
	}
	if err := s.handleControlMessage(controlMessage{
		Type: "audio",
		Data: json.RawMessage(`{}`),
	}); err != nil {
		t.Fatalf("audio control with missing field = %v", err)
	}
}

func TestLoopbackWebTransportAudioStream(t *testing.T) {
	registry := newMachineRegistry()
	sessions := newSessionRegistry()
	registry.seedStatic("default", "test", "127.0.0.1:0")
	upstream := newMachineUpstream("", "default", nil, func(string) {})
	registry.mu.Lock()
	registry.machines["default"].upstream = upstream
	registry.mu.Unlock()
	go upstream.framePusher()
	defer upstream.stop()

	certificates, err := newRelayCertificateManager("", "", "localhost")
	if err != nil {
		t.Fatal(err)
	}
	server := newWebTransportServer(registry, sessions, certificates, nil, nil, nil)
	packetConn, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	serveDone := make(chan struct{})
	go func() {
		defer close(serveDone)
		_ = server.Serve(packetConn)
	}()
	defer func() {
		_ = server.Close()
		_ = packetConn.Close()
		select {
		case <-serveDone:
		case <-time.After(time.Second):
			t.Error("WebTransport server did not stop")
		}
	}()
	time.Sleep(25 * time.Millisecond)

	transport := &webtransport.Transport{
		TLSClientConfig: &tls.Config{InsecureSkipVerify: true, NextProtos: []string{"h3"}},
		QUICConfig:      &quic.Config{EnableDatagrams: true, EnableStreamResetPartialDelivery: true},
	}
	defer transport.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	target := (&url.URL{Scheme: "https", Host: packetConn.LocalAddr().String(), Path: "/wt", RawQuery: "machine=default"}).String()
	_, clientSession, err := transport.Dial(ctx, target, nil)
	if err != nil {
		t.Fatalf("dial WebTransport session: %v", err)
	}
	defer clientSession.CloseWithError(0, "test complete")
	control, err := clientSession.OpenStreamSync(ctx)
	if err != nil {
		t.Fatalf("open client control stream: %v", err)
	}
	writeTestControlMessage(t, control, controlMessage{
		Type: "latency-ping",
		Data: json.RawMessage(`{"t":0}`),
	})
	connected := readTestControlMessage(t, control)
	if connected.Type != "session" {
		t.Fatalf("first control message type = %q, want session", connected.Type)
	}
	expectClipboardStatus(t, control)
	if pong := readTestControlMessage(t, control); pong.Type != "latency-pong" {
		t.Fatalf("second control message type = %q, want latency-pong", pong.Type)
	}
	waitForTestCondition(t, 3*time.Second, func() bool {
		return upstream.currentSession() != nil
	})

	opus := []byte{0xde, 0xad, 0xbe, 0xef}
	upstreamPayload := append([]byte{0x00, 0x00, 0x00, 0x2a}, opus...)
	upstream.sendAudio(audioMsg{tsMicros: 987654, data: upstreamPayload})
	audio, err := clientSession.AcceptUniStream(ctx)
	if err != nil {
		t.Fatalf("accept audio stream: %v", err)
	}
	var header [17]byte
	if _, err := io.ReadFull(audio, header[:]); err != nil {
		t.Fatalf("read audio header: %v", err)
	}
	if header[0] != streamAudio || binary.BigEndian.Uint64(header[1:9]) != 987654 ||
		binary.BigEndian.Uint32(header[9:13]) != 42 ||
		binary.BigEndian.Uint32(header[13:17]) != uint32(len(opus)) {
		t.Fatalf("unexpected audio stream header: %x", header)
	}
	received := make([]byte, len(opus))
	if _, err := io.ReadFull(audio, received); err != nil {
		t.Fatalf("read audio payload: %v", err)
	}
	if !bytes.Equal(received, opus) {
		t.Fatalf("audio payload = %x, want %x", received, opus)
	}

	// Disabling audio stops forwarding.
	writeTestControlMessage(t, control, controlMessage{
		Type: "audio",
		Data: json.RawMessage(`{"enabled":false}`),
	})
	waitForTestCondition(t, 3*time.Second, func() bool {
		return !upstream.currentSession().audioEnabled.Load()
	})
	upstream.sendAudio(audioMsg{tsMicros: 1, data: upstreamPayload})
	done := make(chan struct{})
	go func() {
		defer close(done)
		_, _ = clientSession.AcceptUniStream(ctx)
	}()
	select {
	case <-done:
		t.Fatal("audio stream forwarded while disabled")
	case <-time.After(300 * time.Millisecond):
	}
}

func TestLoopbackWebTransportClientCloseRemovesSession(t *testing.T) {
	registry := newMachineRegistry()
	sessions := newSessionRegistry()
	registry.seedStatic("default", "test", "127.0.0.1:0")
	upstream := newMachineUpstream("", "default", nil, func(string) {})
	registry.mu.Lock()
	registry.machines["default"].upstream = upstream
	registry.mu.Unlock()
	go upstream.framePusher()
	defer upstream.stop()

	certificates, err := newRelayCertificateManager("", "", "localhost")
	if err != nil {
		t.Fatal(err)
	}
	server := newWebTransportServer(registry, sessions, certificates, nil, nil, nil)
	packetConn, err := net.ListenPacket("udp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	serveDone := make(chan struct{})
	go func() {
		defer close(serveDone)
		_ = server.Serve(packetConn)
	}()
	defer func() {
		_ = server.Close()
		_ = packetConn.Close()
		select {
		case <-serveDone:
		case <-time.After(time.Second):
			t.Error("WebTransport server did not stop")
		}
	}()
	time.Sleep(25 * time.Millisecond)

	transport := &webtransport.Transport{
		TLSClientConfig: &tls.Config{InsecureSkipVerify: true, NextProtos: []string{"h3"}},
		QUICConfig:      &quic.Config{EnableDatagrams: true, EnableStreamResetPartialDelivery: true},
	}
	defer transport.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	target := (&url.URL{Scheme: "https", Host: packetConn.LocalAddr().String(), Path: "/wt", RawQuery: "machine=default&width=1280&height=720"}).String()
	_, clientSession, err := transport.Dial(ctx, target, nil)
	if err != nil {
		t.Fatalf("dial WebTransport session: %v", err)
	}
	control, err := clientSession.OpenStreamSync(ctx)
	if err != nil {
		t.Fatalf("open client control stream: %v", err)
	}
	writeTestControlMessage(t, control, controlMessage{
		Type: "latency-ping",
		Data: json.RawMessage(`{"t":0}`),
	})
	connected := readTestControlMessage(t, control)
	if connected.Type != "session" {
		t.Fatalf("first control message type = %q, want session", connected.Type)
	}
	listed := sessions.list()
	if len(listed) != 1 || listed[0].MachineID != "default" || listed[0].Width != 1280 || listed[0].Height != 720 {
		t.Fatalf("unexpected registered session: %+v", listed)
	}
	waitForTestCondition(t, time.Second, func() bool {
		return len(sessions.list()) == 1 && registry.list()[0].Viewers == 1
	})

	_ = clientSession.CloseWithError(0, "viewer closed")
	waitForTestCondition(t, time.Second, func() bool {
		return len(sessions.list()) == 0 && registry.list()[0].Viewers == 0
	})
}

func waitForUpstreamIDR(t *testing.T, upstream *machineUpstream) {
	t.Helper()
	deadline := time.After(3 * time.Second)
	for {
		select {
		case command := <-upstream.commandChan:
			if len(command) == 1 && command[0] == cmdIDR {
				return
			}
		case <-deadline:
			t.Fatal("initial IDR request did not reach upstream")
		}
	}
}

func writeTestControlMessage(t *testing.T, stream io.Writer, message controlMessage) {
	t.Helper()
	payload, err := json.Marshal(message)
	if err != nil {
		t.Fatal(err)
	}
	record := make([]byte, 4+len(payload))
	binary.BigEndian.PutUint32(record, uint32(len(payload)))
	copy(record[4:], payload)
	if _, err := stream.Write(record); err != nil {
		t.Fatalf("write control message: %v", err)
	}
}

func expectClipboardStatus(t *testing.T, stream io.Reader) {
	t.Helper()
	status := readTestControlMessage(t, stream)
	if status.Type != "clipboard-status" {
		t.Fatalf("post-session control message type = %q, want clipboard-status", status.Type)
	}
}

func readTestControlMessage(t *testing.T, stream io.Reader) controlMessage {
	t.Helper()
	var header [4]byte
	if _, err := io.ReadFull(stream, header[:]); err != nil {
		t.Fatalf("read control header: %v", err)
	}
	length := binary.BigEndian.Uint32(header[:])
	payload := make([]byte, length)
	if _, err := io.ReadFull(stream, payload); err != nil {
		t.Fatalf("read control payload: %v", err)
	}
	var message controlMessage
	if err := json.Unmarshal(payload, &message); err != nil {
		t.Fatalf("decode control message: %v", err)
	}
	return message
}

func waitForTestCondition(t *testing.T, timeout time.Duration, condition func() bool) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if condition() {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("timed out waiting for test condition")
}
