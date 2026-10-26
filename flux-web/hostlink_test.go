package main

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/quic-go/quic-go"
)

func newHostLinkTestListener(t *testing.T) *quic.Listener {
	t.Helper()
	cert, err := generateRelayCertificate(time.Now(), "localhost")
	if err != nil {
		t.Fatal(err)
	}
	listener, err := quic.ListenAddr("127.0.0.1:0", &tls.Config{
		Certificates: []tls.Certificate{cert},
		MinVersion:   tls.VersionTLS13,
		MaxVersion:   tls.VersionTLS13,
		NextProtos:   []string{"flux-frames"},
	}, &quic.Config{
		MaxIdleTimeout:  15 * time.Second,
		KeepAlivePeriod: 3 * time.Second,
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	return listener
}

func dialHostLinkTestConn(t *testing.T, listener *quic.Listener) (*quic.Conn, *quic.Conn) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	client, err := quic.DialAddr(ctx, listener.Addr().String(), &tls.Config{
		InsecureSkipVerify: true,
		NextProtos:         []string{"flux-frames"},
	}, &quic.Config{
		MaxIdleTimeout:  15 * time.Second,
		KeepAlivePeriod: 3 * time.Second,
	})
	if err != nil {
		t.Fatal(err)
	}
	server, err := listener.Accept(ctx)
	if err != nil {
		_ = client.CloseWithError(0, "test cleanup")
		t.Fatal(err)
	}
	return client, server
}

func startHostLinkTestServer(t *testing.T, registry *machineRegistry, token string) *quic.Listener {
	t.Helper()
	listener := newHostLinkTestListener(t)
	go serveHostLinks(listener, registry, token)
	return listener
}

func sendHostLinkHello(t *testing.T, conn *quic.Conn, machineID, token string) (*quic.Stream, error) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	stream, err := conn.OpenStreamSync(ctx)
	if err != nil {
		return nil, err
	}
	payload, err := encodeHostLinkHello(machineID, token)
	if err != nil {
		return nil, err
	}
	if _, err := stream.Write(payload); err != nil {
		return nil, err
	}
	if err := stream.Close(); err != nil {
		return nil, err
	}
	return stream, nil
}

func waitForApplicationError(t *testing.T, conn *quic.Conn, want quic.ApplicationErrorCode) {
	t.Helper()
	select {
	case <-conn.Context().Done():
	case <-time.After(5 * time.Second):
		t.Fatal("connection was not closed")
	}
	var appErr *quic.ApplicationError
	if !errors.As(context.Cause(conn.Context()), &appErr) {
		t.Fatalf("connection error = %v, want application error 0x%x", context.Cause(conn.Context()), want)
	}
	if appErr.ErrorCode != want {
		t.Fatalf("application error code = 0x%x, want 0x%x", appErr.ErrorCode, want)
	}
}

func parkRegisteredHostLinkForTest(t *testing.T, registry *machineRegistry, machineID string) *quic.Conn {
	t.Helper()
	listener := startHostLinkTestServer(t, registry, "secret")
	client, err := quic.DialAddr(context.Background(), listener.Addr().String(), &tls.Config{
		InsecureSkipVerify: true,
		NextProtos:         []string{"flux-frames"},
	}, &quic.Config{})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = client.CloseWithError(0, "test cleanup") })

	stream, err := sendHostLinkHello(t, client, machineID, "secret")
	if err != nil {
		t.Fatal(err)
	}
	_ = stream.SetReadDeadline(time.Now().Add(5 * time.Second))
	var ack [1]byte
	if _, err := io.ReadFull(stream, ack[:]); err != nil {
		t.Fatal(err)
	}
	if ack[0] != 0 {
		t.Fatalf("host-link acknowledgement = 0x%02x, want 0x00", ack[0])
	}

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		registry.hostLinks.mu.Lock()
		parked := registry.hostLinks.links[machineID] != nil
		registry.hostLinks.mu.Unlock()
		if parked {
			return client
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("valid host link was not parked")
	return nil
}

func assertHostLinkGauge(t *testing.T, registry *machineRegistry, want string) {
	t.Helper()
	if body := scrapeMetrics(registry.metrics); !strings.Contains(body, "flux_relay_host_links "+want) {
		t.Fatalf("host-link gauge does not contain %s: %s", want, body)
	}
}

func TestHostLinkHubParkAndTake(t *testing.T) {
	listener := newHostLinkTestListener(t)
	client, server := dialHostLinkTestConn(t, listener)
	defer client.CloseWithError(0, "test done")
	defer server.CloseWithError(0, "test done")

	hub := newHostLinkHub(nil)
	hub.park("machine-1", server)
	conn, ok := hub.take("machine-1", nil)
	if !ok || conn != server {
		t.Fatalf("take = (%p, %t), want (%p, true)", conn, ok, server)
	}
}

func TestHostLinkHubReplaceClosesOlderLink(t *testing.T) {
	listener := newHostLinkTestListener(t)
	oldClient, oldServer := dialHostLinkTestConn(t, listener)
	newClient, newServer := dialHostLinkTestConn(t, listener)
	defer oldClient.CloseWithError(0, "test done")
	defer newClient.CloseWithError(0, "test done")
	defer newServer.CloseWithError(0, "test done")

	hub := newHostLinkHub(nil)
	hub.park("machine-1", oldServer)
	hub.park("machine-1", newServer)
	select {
	case <-oldServer.Context().Done():
	case <-time.After(5 * time.Second):
		t.Fatal("replaced link remained open")
	}
	conn, ok := hub.take("machine-1", nil)
	if !ok || conn != newServer {
		t.Fatalf("take = (%p, %t), want replacement (%p, true)", conn, ok, newServer)
	}
}

func TestHostLinkHubDropIfDoesNotDropReplacement(t *testing.T) {
	registry := newMachineRegistry()
	listener := newHostLinkTestListener(t)
	oldClient, oldServer := dialHostLinkTestConn(t, listener)
	newClient, newServer := dialHostLinkTestConn(t, listener)
	defer oldClient.CloseWithError(0, "test done")
	defer newClient.CloseWithError(0, "test done")
	defer newServer.CloseWithError(0, "test done")

	hub := registry.hostLinks
	hub.park("machine-1", oldServer)
	snapshot := hub.peek("machine-1")
	if snapshot != oldServer {
		t.Fatalf("peek = %p, want old link %p", snapshot, oldServer)
	}
	hub.park("machine-1", newServer)
	waitForApplicationError(t, oldServer, 0)

	hub.dropIf("machine-1", snapshot)
	if conn := hub.peek("machine-1"); conn != newServer {
		t.Fatalf("replacement link = %p, want %p", conn, newServer)
	}
	select {
	case <-newServer.Context().Done():
		t.Fatalf("replacement link was closed: %v", context.Cause(newServer.Context()))
	default:
	}
	assertHostLinkGauge(t, registry, "1")
}

func TestHostLinkHubSkipsClosedLinkAndWaitsForNext(t *testing.T) {
	listener := newHostLinkTestListener(t)
	firstClient, firstServer := dialHostLinkTestConn(t, listener)
	defer firstClient.CloseWithError(0, "test done")
	hub := newHostLinkHub(nil)
	hub.park("machine-1", firstServer)
	_ = firstClient.CloseWithError(0, "peer closed")
	select {
	case <-firstServer.Context().Done():
	case <-time.After(5 * time.Second):
		t.Fatal("peer-closed link remained open")
	}

	result := make(chan *quic.Conn, 1)
	go func() {
		conn, ok := hub.take("machine-1", nil)
		if ok {
			result <- conn
		}
	}()
	secondClient, secondServer := dialHostLinkTestConn(t, listener)
	defer secondClient.CloseWithError(0, "test done")
	defer secondServer.CloseWithError(0, "test done")
	hub.park("machine-1", secondServer)
	select {
	case conn := <-result:
		if conn != secondServer {
			t.Fatalf("take returned %p, want %p", conn, secondServer)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("take did not wake for replacement link")
	}
}

func TestHostLinkHubTakeStopsAndPeerCloseUpdatesGauge(t *testing.T) {
	metrics := newRelayMetrics()
	hub := newHostLinkHub(metrics.hostLinks)
	stop := make(chan struct{})
	go func() {
		time.Sleep(10 * time.Millisecond)
		close(stop)
	}()
	if conn, ok := hub.take("machine-1", stop); ok || conn != nil {
		t.Fatalf("take after stop = (%p, %t), want (nil, false)", conn, ok)
	}

	listener := newHostLinkTestListener(t)
	client, server := dialHostLinkTestConn(t, listener)
	hub.park("machine-1", server)
	if body := scrapeMetrics(metrics); !strings.Contains(body, "flux_relay_host_links 1") {
		t.Fatalf("parked link gauge missing 1: %s", body)
	}
	_ = client.CloseWithError(0, "peer closed")
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if strings.Contains(scrapeMetrics(metrics), "flux_relay_host_links 0") {
			_ = server.CloseWithError(0, "test done")
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("parked link gauge did not drop after peer close")
}

func TestServeHostLinksRejectsBadTokenAndUnknownMachine(t *testing.T) {
	tests := []struct {
		name         string
		registration machineRegistration
		token        string
		wantCode     quic.ApplicationErrorCode
	}{
		{
			name:         "bad token",
			registration: machineRegistration{ID: "machine-1", HostDialsOut: true},
			token:        "wrong",
			wantCode:     hostLinkUnauthorized,
		},
		{
			name:         "unknown machine",
			registration: machineRegistration{ID: "machine-1", HostDialsOut: true},
			token:        "secret",
			wantCode:     hostLinkUnknown,
		},
		{
			name:         "not opted in",
			registration: machineRegistration{ID: "machine-1"},
			token:        "secret",
			wantCode:     hostLinkUnknown,
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			registry := newMachineRegistry()
			if test.name == "not opted in" {
				registry.upsert(test.registration, false)
			}
			listener := startHostLinkTestServer(t, registry, "secret")
			client, err := quic.DialAddr(context.Background(), listener.Addr().String(), &tls.Config{
				InsecureSkipVerify: true,
				NextProtos:         []string{"flux-frames"},
			}, &quic.Config{})
			if err != nil {
				t.Fatal(err)
			}
			defer client.CloseWithError(0, "test done")
			_, err = sendHostLinkHello(t, client, "machine-1", test.token)
			if err != nil {
				t.Fatal(err)
			}
			waitForApplicationError(t, client, test.wantCode)
		})
	}
}

func TestServeHostLinksAcknowledgesAndParksValidHello(t *testing.T) {
	registry := newMachineRegistry()
	registry.upsert(machineRegistration{ID: "machine-1", HostDialsOut: true}, false)
	listener := startHostLinkTestServer(t, registry, "secret")
	client, err := quic.DialAddr(context.Background(), listener.Addr().String(), &tls.Config{
		InsecureSkipVerify: true,
		NextProtos:         []string{"flux-frames"},
	}, &quic.Config{})
	if err != nil {
		t.Fatal(err)
	}
	defer client.CloseWithError(0, "test done")

	stream, err := sendHostLinkHello(t, client, "machine-1", "secret")
	if err != nil {
		t.Fatal(err)
	}
	_ = stream.SetReadDeadline(time.Now().Add(5 * time.Second))
	var ack [1]byte
	if _, err := io.ReadFull(stream, ack[:]); err != nil {
		t.Fatal(err)
	}
	if ack[0] != 0x00 {
		t.Fatalf("hello acknowledgement = 0x%02x, want 0x00", ack[0])
	}
	conn, ok := registry.hostLinks.take("machine-1", nil)
	if !ok {
		t.Fatal("valid hello was not parked")
	}
	_ = conn.CloseWithError(0, "test done")
}

func TestAcquireUsesHostDialOutLinkForCommandsAndFrames(t *testing.T) {
	const relayToken = "relay-test-token"
	t.Setenv("FLUX_RELAY_TOKEN", relayToken)
	registry := newMachineRegistry()
	registry.upsert(machineRegistration{
		ID: "machine-1", Name: "Test machine", FrameEndpoint: "127.0.0.1:1", HostDialsOut: true,
	}, false)
	listener := startHostLinkTestServer(t, registry, relayToken)
	upstream, err := registry.acquire("machine-1")
	if err != nil {
		t.Fatal(err)
	}
	defer registry.release(upstream)

	client, err := quic.DialAddr(context.Background(), listener.Addr().String(), &tls.Config{
		InsecureSkipVerify: true,
		NextProtos:         []string{"flux-frames"},
	}, &quic.Config{})
	if err != nil {
		t.Fatal(err)
	}
	defer client.CloseWithError(0, "test done")
	linkHello, err := sendHostLinkHello(t, client, "machine-1", relayToken)
	if err != nil {
		t.Fatal(err)
	}
	_ = linkHello.SetReadDeadline(time.Now().Add(5 * time.Second))
	var ack [1]byte
	if _, err := io.ReadFull(linkHello, ack[:]); err != nil || ack[0] != 0 {
		t.Fatalf("link hello acknowledgement = %x, err=%v", ack, err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	control, err := client.AcceptStream(ctx)
	if err != nil {
		t.Fatal(err)
	}
	wantHello := helloCommand()
	gotHello := make([]byte, len(wantHello))
	if _, err := io.ReadFull(control, gotHello); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(gotHello, wantHello) || gotHello[0] != cmdHello {
		t.Fatalf("first relay command = %x, want cmdHello %x", gotHello, wantHello)
	}

	frame := idrFrame(1)
	stream, err := client.OpenUniStreamSync(ctx)
	if err != nil {
		t.Fatal(err)
	}
	var header [13]byte
	header[0] = msgVideo
	binary.BigEndian.PutUint64(header[1:9], 123)
	binary.BigEndian.PutUint32(header[9:13], uint32(len(frame.data)))
	if _, err := stream.Write(append(header[:], frame.data...)); err != nil {
		t.Fatal(err)
	}
	if err := stream.Close(); err != nil {
		t.Fatal(err)
	}

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if strings.Contains(scrapeMetrics(registry.metrics), `flux_relay_upstream_frames_total{machine="machine-1"} 1`) {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("relay did not dispatch the host video frame")
}

func TestMachineRegistryDeregisterDropsParkedHostLink(t *testing.T) {
	registry := newMachineRegistry()
	registry.upsert(machineRegistration{ID: "machine-1", HostDialsOut: true}, false)
	client := parkRegisteredHostLinkForTest(t, registry, "machine-1")
	assertHostLinkGauge(t, registry, "1")

	if !registry.deregister("machine-1") {
		t.Fatal("machine was not deregistered")
	}
	waitForApplicationError(t, client, 0)
	assertHostLinkGauge(t, registry, "0")
}

func TestMachineRegistryExpiryDropsParkedHostLink(t *testing.T) {
	for _, expireViaList := range []bool{false, true} {
		name := "get"
		if expireViaList {
			name = "list"
		}
		t.Run(name, func(t *testing.T) {
			registry := newMachineRegistry()
			registry.upsert(machineRegistration{ID: "machine-1", HostDialsOut: true}, false)
			client := parkRegisteredHostLinkForTest(t, registry, "machine-1")
			registry.machines["machine-1"].LastSeen = time.Now().Add(-machineExpiry - time.Second)

			if expireViaList {
				registry.list()
			} else if _, err := registry.get("machine-1"); err != errMachineOffline {
				t.Fatalf("get expired machine error = %v, want %v", err, errMachineOffline)
			}
			waitForApplicationError(t, client, 0)
			assertHostLinkGauge(t, registry, "0")
		})
	}
}

func TestMachineRegistryOptOutDropsParkedHostLink(t *testing.T) {
	registry := newMachineRegistry()
	registry.upsert(machineRegistration{ID: "machine-1", HostDialsOut: true}, false)
	client := parkRegisteredHostLinkForTest(t, registry, "machine-1")
	assertHostLinkGauge(t, registry, "1")

	registration := testRegistration("machine-1")
	registry.upsert(registration, false)
	if registry.machines["machine-1"].HostDialsOut {
		t.Fatal("re-registration did not clear host_dials_out")
	}
	waitForApplicationError(t, client, 0)
	assertHostLinkGauge(t, registry, "0")
}

func TestEncodeHostLinkHelloUsesLengthPrefixedJSON(t *testing.T) {
	encoded, err := encodeHostLinkHello("machine-1", "secret")
	if err != nil {
		t.Fatal(err)
	}
	length := binary.BigEndian.Uint16(encoded[:2])
	if int(length) != len(encoded)-2 {
		t.Fatalf("encoded length = %d, payload length = %d", length, len(encoded)-2)
	}
	var hello map[string]string
	if err := json.Unmarshal(encoded[2:], &hello); err != nil {
		t.Fatal(err)
	}
	if hello["machine_id"] != "machine-1" || hello["token"] != "secret" {
		t.Fatalf("decoded hello = %#v", hello)
	}
}
