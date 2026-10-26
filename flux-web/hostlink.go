package main

import (
	"context"
	"crypto/subtle"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"sync"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/quic-go/quic-go"
)

const (
	hostLinkMaxHelloSize = 4 * 1024
	hostLinkUnauthorized = quic.ApplicationErrorCode(0x1)
	hostLinkUnknown      = quic.ApplicationErrorCode(0x2)
)

type hostLinkHub struct {
	mu     sync.Mutex
	links  map[string]*quic.Conn
	notify map[string]chan struct{}
	parked prometheus.Gauge
}

func newHostLinkHub(parked prometheus.Gauge) *hostLinkHub {
	return &hostLinkHub{
		links:  make(map[string]*quic.Conn),
		notify: make(map[string]chan struct{}),
		parked: parked,
	}
}

func (h *hostLinkHub) park(id string, conn *quic.Conn) {
	h.mu.Lock()
	old := h.links[id]
	h.links[id] = conn
	if old == nil && h.parked != nil {
		h.parked.Inc()
	}
	h.signalLocked(id)
	h.mu.Unlock()

	if old != nil && old != conn {
		_ = old.CloseWithError(0, "replaced")
	}
	go func() {
		<-conn.Context().Done()
		h.mu.Lock()
		if h.links[id] == conn {
			delete(h.links, id)
			if h.parked != nil {
				h.parked.Dec()
			}
			h.signalLocked(id)
		}
		h.mu.Unlock()
	}()
}

func (h *hostLinkHub) take(id string, stop <-chan struct{}) (*quic.Conn, bool) {
	for {
		select {
		case <-stop:
			return nil, false
		default:
		}

		h.mu.Lock()
		conn := h.links[id]
		if conn != nil {
			select {
			case <-conn.Context().Done():
				delete(h.links, id)
				if h.parked != nil {
					h.parked.Dec()
				}
				h.signalLocked(id)
				h.mu.Unlock()
				continue
			default:
			}
			delete(h.links, id)
			if h.parked != nil {
				h.parked.Dec()
			}
			h.signalLocked(id)
			h.mu.Unlock()
			return conn, true
		}
		changed := h.notifyChannelLocked(id)
		h.mu.Unlock()

		select {
		case <-stop:
			return nil, false
		case <-changed:
		}
	}
}

func (h *hostLinkHub) notifyChannelLocked(id string) chan struct{} {
	if h.notify[id] == nil {
		h.notify[id] = make(chan struct{})
	}
	return h.notify[id]
}

func (h *hostLinkHub) signalLocked(id string) {
	if changed := h.notifyChannelLocked(id); changed != nil {
		close(changed)
	}
	h.notify[id] = make(chan struct{})
}

type hostLinkHello struct {
	MachineID string `json:"machine_id"`
	Token     string `json:"token"`
}

func serveHostLinks(listener *quic.Listener, registry *machineRegistry, token string) {
	for {
		conn, err := listener.Accept(context.Background())
		if err != nil {
			log.Printf("[host-link] listener stopped: %v", err)
			return
		}
		go handleHostLink(conn, registry, token)
	}
}

func handleHostLink(conn *quic.Conn, registry *machineRegistry, token string) {
	reject := func(code quic.ApplicationErrorCode, reason string) {
		_ = conn.CloseWithError(code, reason)
	}
	ctx, cancel := context.WithTimeout(conn.Context(), 5*time.Second)
	defer cancel()
	stream, err := conn.AcceptStream(ctx)
	if err != nil {
		reject(hostLinkUnauthorized, "invalid hello")
		return
	}
	if err := stream.SetReadDeadline(time.Now().Add(5 * time.Second)); err != nil {
		reject(hostLinkUnauthorized, "invalid hello")
		return
	}
	var lengthBytes [2]byte
	if _, err := io.ReadFull(stream, lengthBytes[:]); err != nil {
		reject(hostLinkUnauthorized, "invalid hello")
		return
	}
	length := binary.BigEndian.Uint16(lengthBytes[:])
	if length == 0 || length > hostLinkMaxHelloSize {
		reject(hostLinkUnauthorized, "invalid hello")
		return
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(stream, payload); err != nil {
		reject(hostLinkUnauthorized, "invalid hello")
		return
	}
	var hello hostLinkHello
	if err := json.Unmarshal(payload, &hello); err != nil || hello.MachineID == "" || hello.Token == "" {
		reject(hostLinkUnauthorized, "invalid hello")
		return
	}
	if subtle.ConstantTimeCompare([]byte(hello.Token), []byte(token)) != 1 {
		reject(hostLinkUnauthorized, "unauthorized")
		return
	}

	registry.mu.Lock()
	machine := registry.machines[hello.MachineID]
	validMachine := machine != nil && !machine.static && machine.HostDialsOut
	registry.mu.Unlock()
	if !validMachine {
		reject(hostLinkUnknown, "unknown or non-dial-out machine")
		return
	}
	if err := stream.SetWriteDeadline(time.Now().Add(5 * time.Second)); err != nil {
		reject(hostLinkUnauthorized, "hello acknowledgement failed")
		return
	}
	if _, err := stream.Write([]byte{0x00}); err != nil {
		reject(hostLinkUnauthorized, "hello acknowledgement failed")
		return
	}
	if err := stream.Close(); err != nil {
		reject(hostLinkUnauthorized, "hello acknowledgement failed")
		return
	}
	registry.hostLinks.park(hello.MachineID, conn)
}

func encodeHostLinkHello(machineID, token string) ([]byte, error) {
	payload, err := json.Marshal(hostLinkHello{MachineID: machineID, Token: token})
	if err != nil {
		return nil, fmt.Errorf("encode host-link hello: %w", err)
	}
	if len(payload) == 0 || len(payload) > hostLinkMaxHelloSize {
		return nil, fmt.Errorf("host-link hello length %d exceeds limit", len(payload))
	}
	framed := make([]byte, 2+len(payload))
	binary.BigEndian.PutUint16(framed[:2], uint16(len(payload)))
	copy(framed[2:], payload)
	return framed, nil
}
