package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"log"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

type loginHarness struct {
	u        *machineUpstream
	sessions *sessionRegistry
	sent     [][]byte
}

func newLoginHarness(t *testing.T) *loginHarness {
	t.Helper()
	st, err := openStore(filepath.Join(t.TempDir(), "data"))
	if err != nil {
		t.Fatalf("openStore: %v", err)
	}
	t.Cleanup(func() { st.Close() })
	sessions := newSessionRegistry()
	deps := &relayDeps{store: st, sessions: sessions, name: func(string) string { return "m" }}
	u := newMachineUpstream("127.0.0.1:9", "m1", deps, func(string) {})
	u.loginSpoken = true
	return &loginHarness{u: u, sessions: sessions}
}

// commands returns every host command queued so far. machineUpstream.send
// enqueues on the caller's goroutine, so draining here (on the test
// goroutine) sees everything the preceding calls sent, with no shared state.
func (h *loginHarness) commands() [][]byte {
	for {
		select {
		case cmd := <-h.u.commandChan:
			h.sent = append(h.sent, cmd)
		default:
			return h.sent
		}
	}
}

// waitForSent polls until at least n host commands have been queued.
func (h *loginHarness) waitForSent(t *testing.T, n int) [][]byte {
	t.Helper()
	waitForTestCondition(t, time.Second, func() bool { return len(h.commands()) >= n })
	return h.sent
}

func (h *loginHarness) newSession(id string, sub, username string) (*Session, *[]controlMessage) {
	msgs := &[]controlMessage{}
	sess := &Session{
		id:          id,
		user:        &identity{Sub: sub, Name: username, Username: username},
		sessionDone: make(chan struct{}),
		store:       h.u.deps.store,
		writeFn: func(m controlMessage) error {
			*msgs = append(*msgs, m)
			return nil
		},
	}
	return sess, msgs
}

func loginJSON(v any) []byte {
	d, _ := json.Marshal(v)
	return d
}

func TestLoginPrefillUsername(t *testing.T) {
	h := newLoginHarness(t)
	sess, msgs := h.newSession("s1", "sub1", "akadmin")
	role, reason := h.u.gateViewer(sess)
	if reason != "" || role != "auth" {
		t.Fatalf("gate: %q %q", role, reason)
	}
	h.u.handleLoginFrame(loginJSON(map[string]any{"state": "prompt", "style": "echo_on", "message": "Username"}))
	sent := h.waitForSent(t, 1)
	if len(sent) != 1 || sent[0][0] != 0x08 {
		t.Fatalf("expected 0x08 response, got %v", sent)
	}
	var resp struct {
		Response string `json:"response"`
	}
	_ = json.Unmarshal(sent[0][5:], &resp)
	if resp.Response != "akadmin" {
		t.Fatalf("prefill answered %q", resp.Response)
	}
	if len(*msgs) != 0 {
		t.Fatalf("prompt should not reach the viewer: %v", *msgs)
	}
}

func TestLoginPrefillDisabled(t *testing.T) {
	old := loginPrefill
	loginPrefill = false
	defer func() { loginPrefill = old }()
	h := newLoginHarness(t)
	sess, msgs := h.newSession("s1", "sub1", "")
	_, _ = h.u.gateViewer(sess)
	h.u.handleLoginFrame(loginJSON(map[string]any{"state": "prompt", "style": "echo_on", "message": "Username"}))
	if len(*msgs) != 1 {
		t.Fatalf("expected prompt forwarded, got %v", *msgs)
	}
	// Answering relays a 0x08.
	h.u.sendLoginResponseIfAuth(sess, "devuser")
	if sent := h.waitForSent(t, 1); len(sent) != 1 || sent[0][0] != 0x08 {
		t.Fatalf("expected 0x08, got %v", sent)
	}
	// Another session may not answer.
	h.u.sendLoginResponseIfAuth(&Session{id: "other"}, "x")
	if len(h.commands()) != 1 {
		t.Fatalf("non-auth session sent a response")
	}
}

func TestLoginOkFailedBusy(t *testing.T) {
	h := newLoginHarness(t)
	sess, msgs := h.newSession("s1", "sub1", "akadmin")
	_, _ = h.u.gateViewer(sess)
	h.sessions.add(sessionInfo{ID: "s1", MachineID: "m1"}, sess, nil)

	// failed → stays authenticating, audit row, forwarded.
	h.u.handleLoginFrame(loginJSON(map[string]any{"state": "failed", "message": "bad", "user": "akadmin"}))
	if h.u.loginSnapshot().state != loginAuthenticating {
		t.Fatal("failed should stay authenticating")
	}
	if got := (*msgs)[len(*msgs)-1].Type; got != "login" {
		t.Fatalf("expected login control, got %q", got)
	}

	// ok → active with os info + audit row.
	sess.pendingWidth, sess.pendingHeight = 1920, 1080
	h.u.handleLoginFrame(loginJSON(map[string]any{"state": "ok", "user": "devuser", "uid": 2101, "reattached": false}))
	ls := h.u.loginSnapshot()
	if ls.state != loginActive || ls.osUser != "devuser" || ls.osUID != 2101 {
		t.Fatalf("login state: %+v", ls)
	}
	info := h.sessions.list()[0]
	if info.OSUser != "devuser" || info.OSUID != 2101 {
		t.Fatalf("sessionInfo os fields: %+v", info)
	}
	if sent := h.waitForSent(t, 1); sent[len(sent)-1][0] != 0x01 {
		t.Fatalf("expected IDR after ok, got %v", sent)
	}

	rows, err := h.u.deps.store.listLogins("m1", 10, 0)
	if err != nil || len(rows) != 2 {
		t.Fatalf("host_logins rows: %v %v", rows, err)
	}
	if rows[0].Result != "ok" || rows[1].Result != "failed" || rows[1].OSUser != "akadmin" {
		t.Fatalf("login audit rows: %+v", rows)
	}
}

func TestLoginGateAndLock(t *testing.T) {
	h := newLoginHarness(t)
	s1, _ := h.newSession("s1", "sub1", "a")
	if _, reason := h.u.gateViewer(s1); reason != "" {
		t.Fatal(reason)
	}
	// Second viewer rejected while authenticating.
	s2, _ := h.newSession("s2", "sub2", "b")
	if _, reason := h.u.gateViewer(s2); reason != "login in progress" {
		t.Fatalf("expected rejection, got %q", reason)
	}
	// Login completes for s1.
	h.u.handleLoginFrame(loginJSON(map[string]any{"state": "ok", "user": "devuser", "uid": 2101}))
	// Other-sub rejected while active, same-sub allowed.
	if _, reason := h.u.gateViewer(s2); reason != "in use by devuser" {
		t.Fatalf("expected in use, got %q", reason)
	}
	s3, _ := h.newSession("s3", "sub1", "a")
	if role, reason := h.u.gateViewer(s3); reason != "" || role != "viewer" {
		t.Fatalf("same-sub viewer rejected: %q %q", role, reason)
	}

	// Register sessions; last leave → 0x09.
	h.sessions.add(sessionInfo{ID: "s1", MachineID: "m1"}, s1, nil)
	h.sessions.add(sessionInfo{ID: "s3", MachineID: "m1"}, s3, nil)
	h.sessions.remove("s3")
	h.u.onSessionLeft(s3)
	for _, c := range h.commands() {
		if c[0] == 0x09 {
			t.Fatal("lock sent while a viewer remains")
		}
	}
	h.sessions.remove("s1")
	h.u.onSessionLeft(s1)
	found := false
	for _, c := range h.commands() {
		if c[0] == 0x09 && len(c) == 5 {
			found = true
		}
	}
	if !found {
		t.Fatal("expected 0x09 lock after last viewer left")
	}
	if h.u.loginSnapshot().state != loginIdle {
		t.Fatal("expected idle after lock")
	}
}

func TestLoginBusyEndsViewer(t *testing.T) {
	h := newLoginHarness(t)
	sess, _ := h.newSession("s1", "sub1", "a")
	_, _ = h.u.gateViewer(sess)
	ended := make(chan string, 1)
	h.sessions.add(sessionInfo{ID: "s1", MachineID: "m1"}, sess, func(r string) { ended <- r })
	h.u.handleLoginFrame(loginJSON(map[string]any{"state": "busy", "user": "bryce"}))
	select {
	case r := <-ended:
		if r != "host in use by bryce" {
			t.Fatalf("end reason: %q", r)
		}
	case <-time.After(time.Second):
		t.Fatal("viewer not ended on busy")
	}
	if h.u.loginSnapshot().state != loginIdle {
		t.Fatal("busy should reset to idle")
	}
}

func TestLoginForwardFailuresLogged(t *testing.T) {
	cases := []struct {
		name  string
		frame map[string]any
		want  string
	}{
		{"prompt", map[string]any{"state": "prompt", "style": "echo_off", "message": "Password: "}, "login prompt forward failed"},
		{"ok", map[string]any{"state": "ok", "user": "devuser", "uid": 2101}, "login ok forward failed"},
		{"failed", map[string]any{"state": "failed", "message": "bad", "user": "devuser"}, "login failed forward failed"},
		{"busy", map[string]any{"state": "busy", "user": "other"}, "login busy forward failed"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			var buf bytes.Buffer
			prev := log.Writer()
			log.SetOutput(&buf)
			defer log.SetOutput(prev)

			h := newLoginHarness(t)
			sess, _ := h.newSession("s1", "sub1", "")
			sess.writeFn = func(controlMessage) error { return errors.New("stream closed") }
			_, _ = h.u.gateViewer(sess)
			h.u.handleLoginFrame(loginJSON(tc.frame))
			if !strings.Contains(buf.String(), "[frame:m1] "+tc.want+": stream closed") {
				t.Fatalf("missing %q in log:\n%s", tc.want, buf.String())
			}
		})
	}

	t.Run("replay", func(t *testing.T) {
		var buf bytes.Buffer
		prev := log.Writer()
		log.SetOutput(&buf)
		defer log.SetOutput(prev)

		h := newLoginHarness(t)
		h.u.handleLoginFrame(loginJSON(map[string]any{"state": "prompt", "style": "echo_off", "message": "Password: "}))
		sess, _ := h.newSession("s1", "sub1", "")
		sess.writeFn = func(controlMessage) error { return errors.New("stream closed") }
		h.u.replayPrompt(sess)
		if !strings.Contains(buf.String(), "[frame:m1] login prompt replay forward failed: stream closed") {
			t.Fatalf("missing replay failure in log:\n%s", buf.String())
		}
	})
}
