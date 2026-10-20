package main

import (
	"encoding/binary"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/gin-gonic/gin"
)

// clipboardDeps wires a temp-store clipboard environment for tests.
func clipboardDeps(t *testing.T) (*store, *machineRegistry, *sessionRegistry, *relayDeps) {
	t.Helper()
	st := testStore(t)
	registry := newMachineRegistry()
	sessions := newSessionRegistry()
	deps := &relayDeps{store: st, sessions: sessions, name: registry.displayName}
	registry.attachDeps(deps)
	return st, registry, sessions, deps
}

// fakeViewer returns a Session that records control messages instead of
// writing to a WebTransport stream.
func fakeViewer(st *store, machine *machineUpstream, sent *[]controlMessage) *Session {
	s := &Session{
		id:       "sess-1",
		clientIP: "192.0.2.10",
		user:     testIdentity(),
		store:    st,
		writeFn: func(m controlMessage) error {
			*sent = append(*sent, m)
			return nil
		},
	}
	s.setMachine(machine)
	return s
}

func statusResult(t *testing.T, msgs []controlMessage) (string, string) {
	t.Helper()
	if len(msgs) != 1 || msgs[0].Type != "clipboard-status" {
		t.Fatalf("expected one clipboard-status message, got %+v", msgs)
	}
	var body struct {
		Result string `json:"result"`
		Policy string `json:"policy"`
	}
	if err := json.Unmarshal(msgs[0].Data, &body); err != nil {
		t.Fatalf("decode status: %v", err)
	}
	return body.Result, body.Policy
}

func clipboardSend(mime, data string) json.RawMessage {
	raw, _ := json.Marshal(map[string]string{"mime": mime, "data": data})
	return raw
}

func TestClientClipboardAllowedEmitsCommand(t *testing.T) {
	st, _, _, deps := clipboardDeps(t)
	u := newMachineUpstream("", "m1", deps, func(string) {})

	var msgs []controlMessage
	s := fakeViewer(st, u, &msgs)
	if err := s.handleClipboard(clipboardSend(clipWireMime, "hello"), st); err != nil {
		t.Fatal(err)
	}
	if result, _ := statusResult(t, msgs); result != clipResultOK {
		t.Fatalf("result = %q, want ok", result)
	}
	select {
	case cmd := <-u.commandChan:
		if cmd[0] != 0x07 {
			t.Fatalf("command type = 0x%02x, want 0x07", cmd[0])
		}
		length := binary.BigEndian.Uint32(cmd[1:5])
		var msg clipboardPayload
		if err := json.Unmarshal(cmd[5:5+length], &msg); err != nil {
			t.Fatalf("decode 0x07 payload: %v", err)
		}
		if msg.Mime != clipWireMime || msg.Data != "hello" {
			t.Fatalf("payload = %+v", msg)
		}
	default:
		t.Fatal("no 0x07 command queued")
	}

	rows, _ := st.listAudit("m1", 10, 0)
	if len(rows) != 1 || rows[0].Result != clipResultOK || rows[0].Direction != clipDirClientToHost {
		t.Fatalf("audit rows = %+v", rows)
	}
	if rows[0].UserSub != "user-1" || rows[0].SessionID != "sess-1" {
		t.Fatalf("audit identity missing: %+v", rows[0])
	}
}

func TestClientClipboardEnforcement(t *testing.T) {
	cases := []struct {
		name       string
		mode       string
		mime       string
		data       string
		wantResult string
		wantCmd    bool
	}{
		{"both allows", clipModeBoth, clipWireMime, "x", clipResultOK, true},
		{"off blocks", clipModeOff, clipWireMime, "x", clipResultBlocked, false},
		{"host_to_client blocks client send", clipModeHostToClient, clipWireMime, "x", clipResultBlocked, false},
		{"client_to_host allows", clipModeClientToHost, clipWireMime, "x", clipResultOK, true},
		{"invalid mime", clipModeBoth, "image/png", "x", clipResultInvalid, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			st, _, _, deps := clipboardDeps(t)
			if err := st.putPolicy("m1", machinePolicy{
				Mode: tc.mode, MaxBytes: defaultClipboardMaxBytes, CaptureContent: true,
			}, "test"); err != nil {
				t.Fatal(err)
			}
			u := newMachineUpstream("", "m1", deps, func(string) {})
			var msgs []controlMessage
			s := fakeViewer(st, u, &msgs)
			if err := s.handleClipboard(clipboardSend(tc.mime, tc.data), st); err != nil {
				t.Fatal(err)
			}
			if result, policy := statusResult(t, msgs); result != tc.wantResult || policy != tc.mode {
				t.Fatalf("status = (%q,%q), want (%q,%q)", result, policy, tc.wantResult, tc.mode)
			}
			select {
			case <-u.commandChan:
				if !tc.wantCmd {
					t.Fatal("unexpected upstream command")
				}
			default:
				if tc.wantCmd {
					t.Fatal("expected upstream command")
				}
			}
			rows, _ := st.listAudit("m1", 10, 0)
			if len(rows) != 1 || rows[0].Result != tc.wantResult {
				t.Fatalf("audit rows = %+v", rows)
			}
		})
	}
}

func TestClientClipboardSizeCap(t *testing.T) {
	st, _, _, deps := clipboardDeps(t)
	if err := st.putPolicy("m1", machinePolicy{
		Mode: clipModeBoth, MaxBytes: 4, CaptureContent: true,
	}, "test"); err != nil {
		t.Fatal(err)
	}
	u := newMachineUpstream("", "m1", deps, func(string) {})
	var msgs []controlMessage
	s := fakeViewer(st, u, &msgs)
	if err := s.handleClipboard(clipboardSend(clipWireMime, "12345"), st); err != nil {
		t.Fatal(err)
	}
	if result, _ := statusResult(t, msgs); result != clipResultTooLarge {
		t.Fatalf("result = %q, want too_large", result)
	}
	select {
	case <-u.commandChan:
		t.Fatal("oversized clipboard reached upstream")
	default:
	}
	rows, _ := st.listAudit("m1", 10, 0)
	if len(rows) != 1 || rows[0].Result != clipResultTooLarge {
		t.Fatalf("audit rows = %+v", rows)
	}
}

func TestUpstreamClipboardReachesViewer(t *testing.T) {
	st, registry, sessions, deps := clipboardDeps(t)
	registry.seedStatic("m1", "Machine One", "127.0.0.1:1")
	u := newMachineUpstream("", "m1", deps, func(string) {})

	var msgs []controlMessage
	viewer := fakeViewer(st, u, &msgs)
	sessions.add(sessionInfo{
		ID: "sess-1", MachineID: "m1", MachineName: "Machine One",
		ClientIP: "192.0.2.10", User: "Test User", UserSub: "user-1",
	}, viewer, nil)

	payload, _ := json.Marshal(clipboardPayload{Mime: clipWireMime, Data: "from host"})
	u.handleClipboardFrame(payload)

	if len(msgs) != 1 || msgs[0].Type != "clipboard" {
		t.Fatalf("viewer got %+v, want one clipboard message", msgs)
	}
	var got clipboardPayload
	if err := json.Unmarshal(msgs[0].Data, &got); err != nil || got.Data != "from host" {
		t.Fatalf("clipboard payload = %+v err=%v", got, err)
	}
	rows, _ := st.listAudit("m1", 10, 0)
	if len(rows) != 1 || rows[0].Result != clipResultOK || rows[0].Direction != clipDirHostToClient {
		t.Fatalf("audit rows = %+v", rows)
	}
	if rows[0].SessionID != "sess-1" || rows[0].UserSub != "user-1" {
		t.Fatalf("audit viewer identity missing: %+v", rows[0])
	}
	_, content, err := st.getAuditContent(rows[0].ID)
	if err != nil || string(content) != "from host" {
		t.Fatalf("audit content = %q err=%v", content, err)
	}
}

func TestUpstreamClipboardBlockedByPolicy(t *testing.T) {
	st, _, sessions, deps := clipboardDeps(t)
	if err := st.putPolicy("m1", machinePolicy{
		Mode: clipModeClientToHost, MaxBytes: defaultClipboardMaxBytes, CaptureContent: true,
	}, "test"); err != nil {
		t.Fatal(err)
	}
	u := newMachineUpstream("", "m1", deps, func(string) {})
	var msgs []controlMessage
	viewer := fakeViewer(st, u, &msgs)
	sessions.add(sessionInfo{ID: "sess-1", MachineID: "m1"}, viewer, nil)

	payload, _ := json.Marshal(clipboardPayload{Mime: clipWireMime, Data: "blocked"})
	u.handleClipboardFrame(payload)

	if len(msgs) != 0 {
		t.Fatalf("blocked clipboard forwarded to viewer: %+v", msgs)
	}
	rows, _ := st.listAudit("m1", 10, 0)
	if len(rows) != 1 || rows[0].Result != clipResultBlocked {
		t.Fatalf("audit rows = %+v", rows)
	}
}

func TestUpstreamClipboardRateLimit(t *testing.T) {
	st, _, _, deps := clipboardDeps(t)
	u := newMachineUpstream("", "m1", deps, func(string) {})
	payload, _ := json.Marshal(clipboardPayload{Mime: clipWireMime, Data: "x"})
	for i := 0; i < clipboardRateLimit+5; i++ {
		u.handleClipboardFrame(payload)
	}
	rows, _ := st.listAudit("m1", 100, 0)
	if len(rows) != clipboardRateLimit {
		t.Fatalf("audited %d events, want rate cap %d", len(rows), clipboardRateLimit)
	}
}

func TestPolicyRoutesRequireAdmin(t *testing.T) {
	gin.SetMode(gin.TestMode)
	st, registry, _, _ := clipboardDeps(t)
	a := &authService{enabled: true}
	block, err := newTestAEAD()
	if err != nil {
		t.Fatal(err)
	}
	a.aead = block
	r := gin.New()
	admin := r.Group("/api", a.requireUser(), a.requireAdmin())
	registerPolicyRoutes(admin, registry, a)

	nonAdmin := testIdentity()
	nonAdmin.Admin = false
	req := httptest.NewRequest(http.MethodGet, "/api/machines/m1/policy", nil)
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, nonAdmin)})
	w := httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusForbidden {
		t.Fatalf("non-admin policy GET = %d, want 403", w.Code)
	}

	// Admin can read and write.
	req = httptest.NewRequest(http.MethodPut, "/api/machines/m1/policy",
		strings.NewReader(`{"clipboard_mode":"off","max_bytes":1024,"capture_content":false}`))
	req.Header.Set("Content-Type", "application/json")
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, testIdentity())})
	w = httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("admin policy PUT = %d (%s), want 200", w.Code, w.Body)
	}
	got := st.getPolicy("m1")
	if got.Mode != clipModeOff || got.MaxBytes != 1024 || got.CaptureContent {
		t.Fatalf("stored policy = %+v", got)
	}
	if !strings.Contains(got.UpdatedBy, "user-1") {
		t.Fatalf("updated_by = %q, want user identity", got.UpdatedBy)
	}

	// Invalid mode is rejected.
	req = httptest.NewRequest(http.MethodPut, "/api/machines/m1/policy",
		strings.NewReader(`{"clipboard_mode":"nope","max_bytes":1024,"capture_content":true}`))
	req.Header.Set("Content-Type", "application/json")
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, testIdentity())})
	w = httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusBadRequest {
		t.Fatalf("invalid policy PUT = %d, want 400", w.Code)
	}

	// Audit listing is reachable for admin.
	req = httptest.NewRequest(http.MethodGet, "/api/audit/clipboard?machine=m1", nil)
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, testIdentity())})
	w = httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("audit list = %d, want 200", w.Code)
	}
}
