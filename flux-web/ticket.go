package main

import (
	"crypto/mldsa"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	ticketSigningContext = "flux-direct-ticket-v1"
	ticketSeedSize       = 32
	ticketNonceSize      = 16
	ticketClockSkew      = 5 * time.Second
)

type directTicketPayload struct {
	Version   int    `json:"v"`
	MachineID string `json:"machine_id"`
	Sub       string `json:"sub"`
	Username  string `json:"username"`
	Email     string `json:"email"`
	Name      string `json:"name"`
	Admin     bool   `json:"admin"`
	Clipboard string `json:"clipboard,omitempty"`
	Path      string `json:"path"`
	Expires   int64  `json:"exp"`
	Nonce     string `json:"nonce"`
}

type ticketSigner struct {
	key *mldsa.PrivateKey
}

var errInvalidTicketSignature = errors.New("invalid ticket signature")

func newTicketSigner(seed []byte) (*ticketSigner, error) {
	if len(seed) != ticketSeedSize {
		return nil, fmt.Errorf("ML-DSA-87 seed has length %d, want %d", len(seed), ticketSeedSize)
	}
	key, err := mldsa.NewPrivateKey(mldsa.MLDSA87(), seed)
	if err != nil {
		return nil, fmt.Errorf("load ML-DSA-87 private key: %w", err)
	}
	return &ticketSigner{key: key}, nil
}

func loadTicketSigner(dataDir string) (*ticketSigner, error) {
	path := filepath.Join(dataDir, "ticket-mldsa87.seed")
	seed, err := os.ReadFile(path)
	if err == nil {
		if err := os.Chmod(path, 0o600); err != nil {
			return nil, fmt.Errorf("protect ticket signing seed: %w", err)
		}
		return newTicketSigner(seed)
	}
	if !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("read ticket signing seed: %w", err)
	}
	if err := os.MkdirAll(dataDir, 0o700); err != nil {
		return nil, fmt.Errorf("create ticket data directory: %w", err)
	}
	seed = make([]byte, ticketSeedSize)
	if _, err := rand.Read(seed); err != nil {
		return nil, fmt.Errorf("generate ticket signing seed: %w", err)
	}
	file, err := os.CreateTemp(dataDir, ".ticket-mldsa87-seed-*")
	if err != nil {
		return nil, fmt.Errorf("create temporary ticket signing seed: %w", err)
	}
	tempPath := file.Name()
	defer os.Remove(tempPath)
	if err := file.Chmod(0o600); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("protect temporary ticket signing seed: %w", err)
	}
	if _, err := file.Write(seed); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("write ticket signing seed: %w", err)
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("sync ticket signing seed: %w", err)
	}
	if err := file.Close(); err != nil {
		return nil, fmt.Errorf("close ticket signing seed: %w", err)
	}
	if err := os.Link(tempPath, path); err != nil {
		if errors.Is(err, os.ErrExist) {
			seed, readErr := readTicketSeed(path)
			if readErr != nil {
				return nil, fmt.Errorf("read concurrently created ticket signing seed: %w", readErr)
			}
			return newTicketSigner(seed)
		}
		return nil, fmt.Errorf("publish ticket signing seed: %w", err)
	}
	return newTicketSigner(seed)
}

func readTicketSeed(path string) ([]byte, error) {
	var seed []byte
	var err error
	for attempt := 0; attempt < 5; attempt++ {
		seed, err = os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		if len(seed) >= ticketSeedSize {
			return seed, nil
		}
		if attempt < 4 {
			time.Sleep(10 * time.Millisecond)
		}
	}
	return nil, fmt.Errorf("ticket signing seed has length %d, want at least %d", len(seed), ticketSeedSize)
}

func (s *ticketSigner) publicKey() []byte {
	if s == nil || s.key == nil {
		return nil
	}
	return append([]byte(nil), s.key.PublicKey().Bytes()...)
}

func (s *ticketSigner) sign(user *identity, machineID, clipboardPolicy string) (string, error) {
	if s == nil || s.key == nil || user == nil || machineID == "" {
		return "", errors.New("missing ticket signer, identity, or machine ID")
	}
	nonce := make([]byte, ticketNonceSize)
	if _, err := rand.Read(nonce); err != nil {
		return "", fmt.Errorf("generate ticket nonce: %w", err)
	}
	payload := directTicketPayload{
		Version:   1,
		MachineID: machineID,
		Sub:       user.Sub,
		Username:  user.Username,
		Email:     user.Email,
		Name:      user.Name,
		Admin:     user.Admin,
		Clipboard: clipboardPolicy,
		Path:      "direct",
		Expires:   time.Now().Add(wtTicketTTL).Unix(),
		Nonce:     base64.RawURLEncoding.EncodeToString(nonce),
	}
	encoded, err := json.Marshal(payload)
	if err != nil {
		return "", fmt.Errorf("encode direct ticket: %w", err)
	}
	signature, err := s.key.Sign(rand.Reader, encoded, &mldsa.Options{Context: ticketSigningContext})
	if err != nil {
		return "", fmt.Errorf("sign direct ticket: %w", err)
	}
	return base64.RawURLEncoding.EncodeToString(encoded) + "." + base64.RawURLEncoding.EncodeToString(signature), nil
}

type ticketVerifier struct {
	key    atomic.Pointer[mldsa.PublicKey]
	mu     sync.Mutex
	nonces map[string]time.Time
}

func newTicketVerifier(publicKey []byte) (*ticketVerifier, error) {
	key, err := mldsa.NewPublicKey(mldsa.MLDSA87(), publicKey)
	if err != nil {
		return nil, fmt.Errorf("load ML-DSA-87 public key: %w", err)
	}
	verifier := &ticketVerifier{nonces: make(map[string]time.Time)}
	verifier.key.Store(key)
	return verifier, nil
}

func (v *ticketVerifier) replacePublicKey(publicKey []byte) error {
	key, err := mldsa.NewPublicKey(mldsa.MLDSA87(), publicKey)
	if err != nil {
		return fmt.Errorf("load ML-DSA-87 public key: %w", err)
	}
	v.key.Store(key)
	return nil
}

func (v *ticketVerifier) verify(ticket, machineID string) (*identity, error) {
	if v == nil {
		return nil, errors.New("ticket verifier is unavailable")
	}
	key := v.key.Load()
	if key == nil {
		return nil, errors.New("ticket verifier is unavailable")
	}
	parts := strings.Split(ticket, ".")
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
		return nil, errors.New("malformed ticket")
	}
	payloadBytes, err := base64.RawURLEncoding.DecodeString(parts[0])
	if err != nil {
		return nil, errors.New("malformed ticket payload")
	}
	signature, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return nil, errors.New("malformed ticket signature")
	}
	if err := mldsa.Verify(key, payloadBytes, signature, &mldsa.Options{Context: ticketSigningContext}); err != nil {
		return nil, errInvalidTicketSignature
	}
	var payload directTicketPayload
	if err := json.Unmarshal(payloadBytes, &payload); err != nil {
		return nil, errors.New("malformed ticket payload")
	}
	if payload.Version != 1 {
		return nil, errors.New("unsupported ticket version")
	}
	if payload.Path != "direct" {
		return nil, errors.New("ticket is not for the direct path")
	}
	if payload.MachineID == "" || payload.MachineID != machineID {
		return nil, errors.New("ticket machine does not match")
	}
	if payload.Sub == "" {
		return nil, errors.New("ticket subject is missing")
	}
	nonce, err := base64.RawURLEncoding.DecodeString(payload.Nonce)
	if err != nil || len(nonce) != ticketNonceSize {
		return nil, errors.New("invalid ticket nonce")
	}
	expires := time.Unix(payload.Expires, 0)
	now := time.Now()
	if now.After(expires.Add(ticketClockSkew)) {
		return nil, errors.New("ticket has expired")
	}
	nonceKey := string(nonce)
	v.mu.Lock()
	for key, expiry := range v.nonces {
		if now.After(expiry.Add(ticketClockSkew)) {
			delete(v.nonces, key)
		}
	}
	if _, seen := v.nonces[nonceKey]; seen {
		v.mu.Unlock()
		return nil, errors.New("ticket has already been used")
	}
	v.nonces[nonceKey] = expires
	v.mu.Unlock()
	clipboardPolicy := ticketClipboardPolicy(payload.Clipboard)
	return &identity{
		Sub:             payload.Sub,
		Username:        payload.Username,
		Email:           payload.Email,
		Name:            payload.Name,
		Admin:           payload.Admin,
		ClipboardPolicy: clipboardPolicy,
		Expires:         expires,
	}, nil
}

func ticketClipboardPolicy(mode string) string {
	switch mode {
	case clipModeBoth, clipModeOff, clipModeHostToClient, clipModeClientToHost:
		return mode
	default:
		return clipModeOff
	}
}
