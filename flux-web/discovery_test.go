package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestWebTransportDiscoveryReturnsDirectThenRelayCandidates(t *testing.T) {
	t.Setenv("FLUX_WT_ADDR", ":9443")
	registry := newMachineRegistry()
	registration := testRegistration("machine-1")
	registration.DirectCandidates = []directCandidate{{
		URL:        "https://host.example.test:8443/wt",
		CertHashes: []string{"aGVsbG8="},
	}}
	registry.upsert(registration, false)
	certificates, err := newRelayCertificateManager("", "", "relay.example.test")
	if err != nil {
		t.Fatal(err)
	}
	signer := ticketTestSigner(t, 7)
	clipboardStore := testStore(t)
	handler := newWebHandler(registry, newSessionRegistry(), certificates, "relay.example.test", disabledAuth(t), clipboardStore, signer)
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=machine-1", nil))
	if response.Code != http.StatusOK {
		t.Fatalf("discovery status = %d: %s", response.Code, response.Body)
	}
	var body struct {
		Host       string   `json:"host"`
		Port       int      `json:"port"`
		CertHashes []string `json:"certHashes"`
		Ticket     string   `json:"ticket"`
		Candidates []struct {
			Path       string   `json:"path"`
			URL        string   `json:"url"`
			Host       string   `json:"host"`
			Port       int      `json:"port"`
			CertHashes []string `json:"certHashes"`
			Ticket     string   `json:"ticket"`
		} `json:"candidates"`
	}
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	var legacy map[string]json.RawMessage
	if err := json.Unmarshal(response.Body.Bytes(), &legacy); err != nil {
		t.Fatal(err)
	}
	for _, field := range []string{"host", "port", "certHashes", "ticket"} {
		if _, ok := legacy[field]; !ok {
			t.Fatalf("legacy field %q is missing", field)
		}
	}
	if body.Host != "relay.example.test" || body.Port != 9443 {
		t.Fatalf("legacy discovery fields missing: %+v", body)
	}
	if len(body.Candidates) != 2 || body.Candidates[0].Path != "direct" || body.Candidates[1].Path != "relay" {
		t.Fatalf("candidate order = %+v", body.Candidates)
	}
	if body.Candidates[1].Port != 9443 {
		t.Fatalf("relay candidate port = %d, want 9443", body.Candidates[1].Port)
	}
	if body.Candidates[0].URL != "https://host.example.test:8443/wt" || body.Candidates[0].CertHashes[0] != "aGVsbG8=" {
		t.Fatalf("direct candidate = %+v", body.Candidates[0])
	}
	verifier, err := newTicketVerifier(signer.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	user, err := verifier.verify(body.Candidates[0].Ticket, "machine-1")
	if err != nil {
		t.Fatalf("direct candidate ticket did not verify: %v", err)
	}
	if user.ClipboardPolicy != clipModeBoth {
		t.Fatalf("default ticket clipboard policy = %q, want %q", user.ClipboardPolicy, clipModeBoth)
	}
	if !user.ClipboardCapture || user.ClipboardMaxBytes != defaultClipboardMaxBytes {
		t.Fatalf("default ticket clipboard details = capture:%t max:%d", user.ClipboardCapture, user.ClipboardMaxBytes)
	}
	if _, err := verifier.verify(body.Candidates[0].Ticket, "machine-1"); err == nil {
		t.Fatal("direct ticket replay unexpectedly verified")
	}

	if err := clipboardStore.putPolicy("machine-1", machinePolicy{
		Mode: clipModeHostToClient, MaxBytes: defaultClipboardMaxBytes, CaptureContent: true,
	}, "test"); err != nil {
		t.Fatal(err)
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=machine-1", nil))
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	user, err = verifier.verify(body.Candidates[0].Ticket, "machine-1")
	if err != nil {
		t.Fatalf("verify ticket with updated clipboard policy: %v", err)
	}
	if user.ClipboardPolicy != clipModeHostToClient {
		t.Fatalf("ticket clipboard policy = %q, want %q", user.ClipboardPolicy, clipModeHostToClient)
	}
	if !user.ClipboardCapture || user.ClipboardMaxBytes != defaultClipboardMaxBytes {
		t.Fatalf("updated ticket clipboard details = capture:%t max:%d", user.ClipboardCapture, user.ClipboardMaxBytes)
	}
}

func TestWebTransportDiscoveryWithoutOnlineDirectCandidateReturnsRelayOnly(t *testing.T) {
	registry := newMachineRegistry()
	registry.upsert(testRegistration("machine-1"), false)
	certificates, err := newRelayCertificateManager("", "", "relay.example.test")
	if err != nil {
		t.Fatal(err)
	}
	handler := newWebHandler(registry, newSessionRegistry(), certificates, "relay.example.test", disabledAuth(t), nil, ticketTestSigner(t, 8))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=missing", nil))
	var body struct {
		Candidates []struct {
			Path string `json:"path"`
		} `json:"candidates"`
	}
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if len(body.Candidates) != 1 || body.Candidates[0].Path != "relay" {
		t.Fatalf("candidates for unknown machine = %+v, want relay only", body.Candidates)
	}
}

func TestDiscoveryConnectionPathPoliciesAndProbeTickets(t *testing.T) {
	registry := newMachineRegistry()
	registration := testRegistration("machine-1")
	registration.DirectCandidates = []directCandidate{{URL: "https://host.example.test:8443/wt"}}
	registry.upsert(registration, false)
	certificates, err := newRelayCertificateManager("", "", "relay.example.test")
	if err != nil {
		t.Fatal(err)
	}
	store := testStore(t)
	signer := ticketTestSigner(t, 9)
	handler := newWebHandler(registry, newSessionRegistry(), certificates, "relay.example.test", disabledAuth(t), store, signer)

	setPath := func(path string) {
		t.Helper()
		policy := store.getPolicy("machine-1")
		policy.ConnectionPath = path
		if err := store.putPolicy("machine-1", policy, "test"); err != nil {
			t.Fatal(err)
		}
	}

	setPath("direct_only")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=machine-1", nil))
	var body struct {
		Host       string `json:"host"`
		Port       int    `json:"port"`
		Ticket     string `json:"ticket"`
		Candidates []struct {
			Path   string `json:"path"`
			Ticket string `json:"ticket"`
		} `json:"candidates"`
	}
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if response.Code != http.StatusOK || len(body.Candidates) != 1 || body.Candidates[0].Path != "direct" ||
		body.Ticket != "" || body.Host != "relay.example.test" || body.Port != 8443 {
		t.Fatalf("direct_only discovery = status %d, %+v", response.Code, body)
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=machine-1&probe=1", nil))
	var probe map[string]json.RawMessage
	if err := json.Unmarshal(response.Body.Bytes(), &probe); err != nil {
		t.Fatal(err)
	}
	if response.Code != http.StatusOK || len(probe) != 1 || probe["candidates"] == nil {
		t.Fatalf("probe response has legacy fields or wrong status: %d %s", response.Code, response.Body)
	}
	if err := json.Unmarshal(probe["candidates"], &body.Candidates); err != nil {
		t.Fatal(err)
	}
	if len(body.Candidates) != 1 || ticketPathFromPayload(body.Candidates[0].Ticket) != "probe" {
		t.Fatalf("probe candidates = %+v", body.Candidates)
	}

	setPath("relay_only")
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=machine-1", nil))
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if response.Code != http.StatusOK || len(body.Candidates) != 1 || body.Candidates[0].Path != "relay" {
		t.Fatalf("relay_only discovery = status %d, %+v", response.Code, body)
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=machine-1&probe=1", nil))
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if response.Code != http.StatusOK || len(body.Candidates) != 0 {
		t.Fatalf("relay_only probe = status %d, %+v", response.Code, body)
	}

	registration.DirectCandidates = nil
	registry.upsert(registration, false)
	setPath("direct_only")
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/api/webtransport?machine=machine-1", nil))
	if response.Code != http.StatusConflict || strings.TrimSpace(response.Body.String()) != "{\"error\":\"no direct endpoint available\"}" {
		t.Fatalf("direct_only without endpoint = %d %s", response.Code, response.Body)
	}
}

func TestRelayWebTransportRejectsDirectOnlyAndProbeTickets(t *testing.T) {
	st := testStore(t)
	policy := defaultMachinePolicy()
	policy.ConnectionPath = "direct_only"
	if err := st.putPolicy("machine-1", policy, "test"); err != nil {
		t.Fatal(err)
	}
	registry := newMachineRegistry()
	registry.seedStatic("machine-1", "Machine", "127.0.0.1:8556")
	request := httptest.NewRequest(http.MethodGet, "/wt?machine=machine-1", nil)
	response := httptest.NewRecorder()
	handleWebTransport(response, request, nil, registry, newSessionRegistry(), disabledAuth(t), st, nil)
	if response.Code != http.StatusForbidden || !strings.Contains(response.Body.String(), "path_policy") {
		t.Fatalf("direct_only relay response = %d %s", response.Code, response.Body)
	}

	user := &identity{Sub: "viewer"}
	signer := ticketTestSigner(t, 10)
	probeTicket, err := signer.sign(user, "machine-1", defaultMachinePolicy(), "probe")
	if err != nil {
		t.Fatal(err)
	}
	policy.ConnectionPath = "auto"
	if err := st.putPolicy("machine-1", policy, "test"); err != nil {
		t.Fatal(err)
	}
	request = httptest.NewRequest(http.MethodGet, "/wt?machine=machine-1&ticket="+probeTicket, nil)
	response = httptest.NewRecorder()
	handleWebTransport(response, request, nil, registry, newSessionRegistry(), disabledAuth(t), st, nil)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("relay accepted probe ticket: %d %s", response.Code, response.Body)
	}
}
