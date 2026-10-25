package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"testing"
)

// The hello (0x0A) must be the first bytes on a new connection even when
// the shared command channel still holds stale commands.
func TestHelloIsFirstCommandOnConnect(t *testing.T) {
	t.Setenv("FLUX_RELAY_TOKEN", "tok-123")
	hello := helloCommand()
	if len(hello) != 5+len("tok-123") || hello[0] != 0x0A || string(hello[5:]) != "tok-123" {
		t.Fatalf("bad hello bytes: %v", hello)
	}

	var conn net.Conn
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	accepted := make(chan net.Conn, 1)
	go func() {
		c, _ := l.Accept()
		if c != nil {
			accepted <- c
		}
	}()

	conn, err = net.Dial("tcp", l.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()

	u := newMachineUpstream(l.Addr().String(), "m1", nil, func(string) {})
	// Poison the channel like a dead connection would leave it.
	u.commandChan <- []byte{0x01}
	u.commandChan <- []byte{0x04, 0, 0, 0, 1}
	u.drainCommands()
	if _, err := conn.Write(hello); err != nil {
		t.Fatal(err)
	}
	srv := <-accepted
	defer srv.Close()
	buf := make([]byte, len(hello))
	if _, err := io.ReadFull(srv, buf); err != nil {
		t.Fatal(err)
	}
	if buf[0] != 0x0A {
		t.Fatalf("first command was 0x%02x, want 0x0A", buf[0])
	}
	// After the drain the stale commands are gone.
	select {
	case c := <-u.commandChan:
		t.Fatalf("stale command survived drain: %v", c)
	default:
	}
}

// The QUIC certificate pin must accept the matching fingerprint and reject
// a different one.
func TestQuicCertPinning(t *testing.T) {
	good := sha256.Sum256([]byte("leaf-der"))
	u := newMachineUpstream("a", "m", nil, func(string) {})
	u.setQuicCertSHA256(hex.EncodeToString(good[:]))
	verify := func(raw [][]byte) error {
		sum := sha256.Sum256(raw[0])
		if hex.EncodeToString(sum[:]) != u.pinnedCertSHA256() {
			return fmt.Errorf("mismatch")
		}
		return nil
	}
	if err := verify([][]byte{[]byte("leaf-der")}); err != nil {
		t.Fatalf("matching fingerprint rejected: %v", err)
	}
	if err := verify([][]byte{[]byte("other-cert")}); err == nil {
		t.Fatal("mismatching fingerprint accepted")
	}
}

// markDisconnected → onUpstreamDisconnected must reset login state and end
// attached viewers with "host connection lost".
func TestUpstreamDisconnectEndsViewersAndResetsLogin(t *testing.T) {
	st := testStore(t)
	sessions := newSessionRegistry()
	deps := &relayDeps{store: st, sessions: sessions, name: func(string) string { return "m" }}
	u := newMachineUpstream("a", "m1", deps, func(string) {})
	u.supportsLogin = true

	msgs := &[]controlMessage{}
	sess := &Session{
		id:          "s1",
		user:        &identity{Sub: "s"},
		sessionDone: make(chan struct{}),
		writeFn: func(m controlMessage) error {
			*msgs = append(*msgs, m)
			return nil
		},
	}
	sessions.add(sessionInfo{ID: "s1", MachineID: "m1", ClientIP: "1.2.3.4"}, sess, func(reason string) {
		_ = sess.writeFn(controlMessage{Type: "session-ended", Data: json.RawMessage(`{"reason":"` + reason + `"}`)})
	})
	u.bindSession(sess)

	u.loginMu.Lock()
	u.login.state = loginActive
	u.login.osUser = "devuser"
	u.loginMu.Unlock()

	u.markDisconnected()

	if u.loginSnapshot().state != loginIdle {
		t.Fatalf("login state %v, want idle", u.loginSnapshot().state)
	}
	found := false
	for _, m := range *msgs {
		if m.Type == "session-ended" {
			found = true
		}
	}
	if !found {
		t.Fatalf("viewer did not get session-ended, got %+v", *msgs)
	}
}

// /api/machines must surface login_state/logged_in_user from the last
// registration even when no upstream exists.
func TestMachinesListLoginFieldsFromRegistration(t *testing.T) {
	r := newMachineRegistry()
	r.upsert(machineRegistration{
		ID: "m9", Name: "h", FrameEndpoint: "127.0.0.1:8556",
		SupportsLogin: true, LoginState: "locked", LoggedInUser: "devuser",
	}, false)
	list := r.list()
	var m *machineInfo
	for i := range list {
		if list[i].ID == "m9" {
			m = &list[i]
		}
	}
	if m == nil {
		t.Fatal("machine missing")
	}
	if m.LoginState != "locked" || m.LoggedInUser != "devuser" {
		t.Fatalf("login fields = %q/%q", m.LoginState, m.LoggedInUser)
	}
	if !m.SupportsLogin {
		t.Fatal("supports_login not propagated")
	}
}
