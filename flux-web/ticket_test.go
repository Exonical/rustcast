package main

import (
	"bytes"
	"context"
	"crypto/mldsa"
	"encoding/base64"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func ticketTestSigner(t *testing.T, byteValue byte) *ticketSigner {
	t.Helper()
	seed := bytes.Repeat([]byte{byteValue}, ticketSeedSize)
	signer, err := newTicketSigner(seed)
	if err != nil {
		t.Fatal(err)
	}
	return signer
}

func signedTicketForPayload(t *testing.T, signer *ticketSigner, payload directTicketPayload) string {
	t.Helper()
	if payload.Nonce == "" {
		payload.Nonce = base64.RawURLEncoding.EncodeToString([]byte("0123456789abcdef"))
	}
	encoded, err := json.Marshal(payload)
	if err != nil {
		t.Fatal(err)
	}
	signature, err := signer.key.Sign(nil, encoded, &mldsa.Options{Context: ticketSigningContext})
	if err != nil {
		t.Fatal(err)
	}
	return base64.RawURLEncoding.EncodeToString(encoded) + "." + base64.RawURLEncoding.EncodeToString(signature)
}

func TestTicketSignerRoundTripAndRejectsInvalidTickets(t *testing.T) {
	signer := ticketTestSigner(t, 1)
	user := &identity{Sub: "sub-1", Username: "viewer", Email: "viewer@example.test", Name: "Viewer", Admin: true}
	validTicket, err := signer.sign(user, "machine-1", clipModeBoth)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(validTicket, "=") {
		t.Fatal("ticket must use unpadded base64url")
	}
	verifier, err := newTicketVerifier(signer.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	got, err := verifier.verify(validTicket, "machine-1")
	if err != nil {
		t.Fatalf("verify valid ticket: %v", err)
	}
	if got.Sub != user.Sub || got.Username != user.Username || got.Email != user.Email || got.Name != user.Name ||
		got.Admin != user.Admin || got.ClipboardPolicy != clipModeBoth {
		t.Fatalf("verified identity = %+v, want %+v", got, user)
	}

	parts := strings.Split(validTicket, ".")
	payloadBytes, err := base64.RawURLEncoding.DecodeString(parts[0])
	if err != nil {
		t.Fatal(err)
	}
	tamperedPayload := strings.Replace(string(payloadBytes), `"username":"viewer"`, `"username":"attacker"`, 1)
	tamperedPayloadTicket := base64.RawURLEncoding.EncodeToString([]byte(tamperedPayload)) + "." + parts[1]
	signature, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		t.Fatal(err)
	}
	signature[0] ^= 1
	tamperedSignatureTicket := parts[0] + "." + base64.RawURLEncoding.EncodeToString(signature)

	basePayload := directTicketPayload{
		Version:   1,
		MachineID: "machine-1",
		Sub:       user.Sub,
		Username:  user.Username,
		Email:     user.Email,
		Name:      user.Name,
		Admin:     user.Admin,
		Path:      "direct",
		Expires:   time.Now().Add(time.Minute).Unix(),
	}
	expiredPayload := basePayload
	expiredPayload.Expires = time.Now().Add(-10 * time.Second).Unix()
	wrongPathPayload := basePayload
	wrongPathPayload.Path = "relay"
	wrongVersionPayload := basePayload
	wrongVersionPayload.Version = 2

	wrongKeyVerifier, err := newTicketVerifier(ticketTestSigner(t, 2).publicKey())
	if err != nil {
		t.Fatal(err)
	}
	tests := []struct {
		name     string
		ticket   string
		machine  string
		verifier *ticketVerifier
	}{
		{name: "tampered payload", ticket: tamperedPayloadTicket, machine: "machine-1"},
		{name: "tampered signature", ticket: tamperedSignatureTicket, machine: "machine-1"},
		{name: "wrong machine", ticket: validTicket, machine: "machine-2"},
		{name: "expired", ticket: signedTicketForPayload(t, signer, expiredPayload), machine: "machine-1"},
		{name: "wrong path", ticket: signedTicketForPayload(t, signer, wrongPathPayload), machine: "machine-1"},
		{name: "wrong key", ticket: validTicket, machine: "machine-1", verifier: wrongKeyVerifier},
		{name: "wrong version", ticket: signedTicketForPayload(t, signer, wrongVersionPayload), machine: "machine-1"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			testVerifier := test.verifier
			if testVerifier == nil {
				testVerifier, err = newTicketVerifier(signer.publicKey())
				if err != nil {
					t.Fatal(err)
				}
			}
			if _, err := testVerifier.verify(test.ticket, test.machine); err == nil {
				t.Fatal("verify succeeded, want failure")
			}
		})
	}

	replayVerifier, err := newTicketVerifier(signer.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := replayVerifier.verify(validTicket, "machine-1"); err != nil {
		t.Fatalf("first use of ticket failed: %v", err)
	}
	if _, err := replayVerifier.verify(validTicket, "machine-1"); err == nil {
		t.Fatal("replayed ticket was accepted")
	}
}

func TestTicketWithoutClipboardPolicyFailsClosed(t *testing.T) {
	signer := ticketTestSigner(t, 14)
	verifier, err := newTicketVerifier(signer.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	ticket := signedTicketForPayload(t, signer, directTicketPayload{
		Version:   1,
		MachineID: "machine-1",
		Sub:       "subject",
		Path:      "direct",
		Expires:   time.Now().Add(time.Minute).Unix(),
	})
	user, err := verifier.verify(ticket, "machine-1")
	if err != nil {
		t.Fatal(err)
	}
	if user.ClipboardPolicy != clipModeOff {
		t.Fatalf("missing clipboard policy = %q, want %q", user.ClipboardPolicy, clipModeOff)
	}
}

func TestTicketSignerSeedPersistence(t *testing.T) {
	dataDir := t.TempDir()
	first, err := loadTicketSigner(dataDir)
	if err != nil {
		t.Fatal(err)
	}
	second, err := loadTicketSigner(dataDir)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(first.publicKey(), second.publicKey()) {
		t.Fatal("ticket public key changed after loading the persisted seed")
	}
	info, err := os.Stat(filepath.Join(dataDir, "ticket-mldsa87.seed"))
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("seed permissions = %04o, want 0600", info.Mode().Perm())
	}
}

func TestTicketSignerConcurrentSeedCreation(t *testing.T) {
	dataDir := t.TempDir()
	const workers = 12
	start := make(chan struct{})
	results := make(chan struct {
		publicKey []byte
		err       error
	}, workers)
	for range workers {
		go func() {
			<-start
			signer, err := loadTicketSigner(dataDir)
			if err != nil {
				results <- struct {
					publicKey []byte
					err       error
				}{err: err}
				return
			}
			results <- struct {
				publicKey []byte
				err       error
			}{publicKey: signer.publicKey()}
		}()
	}
	close(start)
	var first []byte
	for range workers {
		result := <-results
		if result.err != nil {
			t.Fatal(result.err)
		}
		if first == nil {
			first = result.publicKey
		} else if !bytes.Equal(first, result.publicKey) {
			t.Fatal("concurrent ticket signers loaded different public keys")
		}
	}
}

func TestDirectTicketKeyRefreshSwapsKeyAndPreservesVerifierState(t *testing.T) {
	oldSigner := ticketTestSigner(t, 10)
	newSigner := ticketTestSigner(t, 11)
	verifier, err := newTicketVerifier(oldSigner.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	user := &identity{Sub: "subject"}
	oldTicket, err := oldSigner.sign(user, "machine-1", clipModeBoth)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := verifier.verify(oldTicket, "machine-1"); err != nil {
		t.Fatal(err)
	}
	newTicket, err := newSigner.sign(user, "machine-1", clipModeBoth)
	if err != nil {
		t.Fatal(err)
	}
	refresher := &directTicketKeyRefresher{
		verifier:  verifier,
		cachePath: filepath.Join(t.TempDir(), "ticket-key.pub"),
		fetch: func(context.Context) ([]byte, error) {
			return newSigner.publicKey(), nil
		},
	}
	if err := refresher.refresh(context.Background()); err != nil {
		t.Fatalf("refresh ticket key: %v", err)
	}
	if len(verifier.nonces) != 1 {
		t.Fatalf("nonce cache size after key swap = %d, want 1", len(verifier.nonces))
	}
	if _, err := verifier.verify(newTicket, "machine-1"); err != nil {
		t.Fatalf("ticket from new signer failed after key swap: %v", err)
	}
	if _, err := verifier.verify(oldTicket, "machine-1"); !errors.Is(err, errInvalidTicketSignature) {
		t.Fatalf("ticket from old signer error = %v, want invalid signature", err)
	}
}

func TestFailedDirectTicketKeyRefreshKeepsCurrentKey(t *testing.T) {
	oldSigner := ticketTestSigner(t, 12)
	newSigner := ticketTestSigner(t, 13)
	verifier, err := newTicketVerifier(oldSigner.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	user := &identity{Sub: "subject"}
	oldTicket, err := oldSigner.sign(user, "machine-1", clipModeBoth)
	if err != nil {
		t.Fatal(err)
	}
	newTicket, err := newSigner.sign(user, "machine-1", clipModeBoth)
	if err != nil {
		t.Fatal(err)
	}
	refresher := &directTicketKeyRefresher{
		verifier: verifier,
		fetch: func(context.Context) ([]byte, error) {
			return nil, errors.New("relay unavailable")
		},
	}
	if err := refresher.refresh(context.Background()); err == nil {
		t.Fatal("failed fetch returned nil")
	}
	if _, err := verifier.verify(oldTicket, "machine-1"); err != nil {
		t.Fatalf("current key stopped verifying after failed refresh: %v", err)
	}
	if _, err := verifier.verify(newTicket, "machine-1"); !errors.Is(err, errInvalidTicketSignature) {
		t.Fatalf("new key ticket error = %v, want invalid signature", err)
	}
}

func TestRelayTicketKeyEndpointRequiresMachineToken(t *testing.T) {
	signer := ticketTestSigner(t, 3)
	certificates, err := newRelayCertificateManager("", "", "localhost")
	if err != nil {
		t.Fatal(err)
	}
	auth := &authService{cfg: authConfig{relayToken: "relay-secret"}}
	handler := newWebHandler(newMachineRegistry(), newSessionRegistry(), certificates, "localhost", auth, nil, signer)

	unauthorized := httptest.NewRecorder()
	handler.ServeHTTP(unauthorized, httptest.NewRequest(http.MethodGet, "/api/relay/ticket-key", nil))
	if unauthorized.Code != http.StatusUnauthorized {
		t.Fatalf("unauthorized status = %d, want %d", unauthorized.Code, http.StatusUnauthorized)
	}

	authorized := httptest.NewRecorder()
	request := httptest.NewRequest(http.MethodGet, "/api/relay/ticket-key", nil)
	request.Header.Set("Authorization", "Bearer relay-secret")
	handler.ServeHTTP(authorized, request)
	if authorized.Code != http.StatusOK {
		t.Fatalf("authorized status = %d, want %d: %s", authorized.Code, http.StatusOK, authorized.Body)
	}
	var response struct {
		Algorithm string `json:"alg"`
		PublicKey string `json:"public_key"`
	}
	if err := json.Unmarshal(authorized.Body.Bytes(), &response); err != nil {
		t.Fatal(err)
	}
	if response.Algorithm != "ML-DSA-87" {
		t.Fatalf("algorithm = %q, want ML-DSA-87", response.Algorithm)
	}
	publicKey, err := base64.StdEncoding.DecodeString(response.PublicKey)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(publicKey, signer.publicKey()) {
		t.Fatal("ticket key endpoint returned a different public key")
	}
}

func TestDirectWebTransportRejectsTicketsAndCountsRejections(t *testing.T) {
	registry := newMachineRegistry()
	registry.seedStatic("default", "Default", "127.0.0.1:1")
	signer := ticketTestSigner(t, 4)
	verifier, err := newTicketVerifier(signer.publicKey())
	if err != nil {
		t.Fatal(err)
	}
	direct := &directWebTransportMode{machineID: "local-machine", verifier: verifier}
	for _, target := range []string{"/wt", "/wt?ticket=invalid"} {
		response := httptest.NewRecorder()
		handleWebTransport(response, httptest.NewRequest(http.MethodGet, target, nil), nil, registry, nil, nil, nil, direct)
		if response.Code != http.StatusUnauthorized {
			t.Fatalf("%s status = %d, want 401", target, response.Code)
		}
	}
	if metrics := scrapeMetrics(registry.metrics); !strings.Contains(metrics, `reason="unauthenticated"} 2`) {
		t.Fatalf("unauthenticated rejection metric missing or incorrect:\n%s", metrics)
	}
}
