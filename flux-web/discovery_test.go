package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestWebTransportDiscoveryReturnsDirectThenRelayCandidates(t *testing.T) {
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
	handler := newWebHandler(registry, newSessionRegistry(), certificates, "relay.example.test", disabledAuth(t), nil, signer)
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
	if body.Host != "relay.example.test" || body.Port != webTransportPort {
		t.Fatalf("legacy discovery fields missing: %+v", body)
	}
	if len(body.Candidates) != 2 || body.Candidates[0].Path != "direct" || body.Candidates[1].Path != "relay" {
		t.Fatalf("candidate order = %+v", body.Candidates)
	}
	if body.Candidates[0].URL != "https://host.example.test:8443/wt" || body.Candidates[0].CertHashes[0] != "aGVsbG8=" {
		t.Fatalf("direct candidate = %+v", body.Candidates[0])
	}
	verifier, err := newTicketVerifier(signer.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := verifier.verify(body.Candidates[0].Ticket, "machine-1"); err != nil {
		t.Fatalf("direct candidate ticket did not verify: %v", err)
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
