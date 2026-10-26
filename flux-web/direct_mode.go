package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/quic-go/webtransport-go"
)

type directStatus struct {
	CertHashes []string `json:"cert_hashes"`
}

const (
	directTicketKeyRefreshInterval = 5 * time.Minute
	directTicketKeyFailureCooldown = 30 * time.Second
)

var errTicketKeyURLDisallowed = errors.New("ticket-key relay URL is not allowed")

type directTicketKeyRefresher struct {
	verifier  *ticketVerifier
	pinned    bool
	cachePath string
	fetch     func(context.Context) ([]byte, error)
	mu        sync.Mutex
	lastAsked atomic.Int64
}

func newDirectTicketKeyRefresher(verifier *ticketVerifier) *directTicketKeyRefresher {
	cachePath := filepath.Join(storeDirFromEnv(), "ticket-key.pub")
	relayURL := strings.TrimRight(strings.TrimSpace(os.Getenv("FLUX_RELAY_URL")), "/")
	token := strings.TrimSpace(os.Getenv("FLUX_RELAY_TOKEN"))
	return &directTicketKeyRefresher{
		verifier:  verifier,
		pinned:    strings.TrimSpace(os.Getenv("FLUX_TICKET_PUBLIC_KEY")) != "",
		cachePath: cachePath,
		fetch: func(ctx context.Context) ([]byte, error) {
			if !ticketKeyURLAllowed(relayURL) {
				return nil, errTicketKeyURLDisallowed
			}
			if token == "" {
				return nil, errors.New("FLUX_RELAY_TOKEN is required to fetch the ticket key")
			}
			request, err := http.NewRequestWithContext(ctx, http.MethodGet, relayURL+"/api/relay/ticket-key", nil)
			if err != nil {
				return nil, err
			}
			request.Header.Set("Authorization", "Bearer "+token)
			response, err := (&http.Client{Timeout: 10 * time.Second}).Do(request)
			if err != nil {
				return nil, err
			}
			defer response.Body.Close()
			if response.StatusCode != http.StatusOK {
				return nil, fmt.Errorf("ticket-key endpoint returned %s", response.Status)
			}
			var body struct {
				Algorithm string `json:"alg"`
				PublicKey string `json:"public_key"`
			}
			if err := json.NewDecoder(response.Body).Decode(&body); err != nil {
				return nil, err
			}
			if body.Algorithm != "ML-DSA-87" {
				return nil, fmt.Errorf("unsupported ticket-key algorithm %q", body.Algorithm)
			}
			publicKey, err := base64.StdEncoding.DecodeString(body.PublicKey)
			if err != nil {
				return nil, fmt.Errorf("decode ticket public key: %w", err)
			}
			return publicKey, nil
		},
	}
}

func (r *directTicketKeyRefresher) refresh(ctx context.Context) error {
	if r == nil || r.pinned {
		return nil
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	publicKey, err := r.fetch(ctx)
	if err != nil {
		return err
	}
	if _, err := newTicketVerifier(publicKey); err != nil {
		return err
	}
	if err := writeTicketKeyCache(r.cachePath, publicKey); err != nil {
		return err
	}
	if err := r.verifier.replacePublicKey(publicKey); err != nil {
		return err
	}
	return nil
}

func writeTicketKeyCache(path string, publicKey []byte) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fmt.Errorf("create ticket key cache directory: %w", err)
	}
	file, err := os.CreateTemp(dir, ".ticket-key-*")
	if err != nil {
		return fmt.Errorf("create temporary ticket key cache: %w", err)
	}
	tempPath := file.Name()
	defer os.Remove(tempPath)
	if err := file.Chmod(0o644); err != nil {
		_ = file.Close()
		return fmt.Errorf("set ticket key cache permissions: %w", err)
	}
	if _, err := file.Write([]byte(base64.StdEncoding.EncodeToString(publicKey))); err != nil {
		_ = file.Close()
		return fmt.Errorf("write ticket key cache: %w", err)
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return fmt.Errorf("sync ticket key cache: %w", err)
	}
	if err := file.Close(); err != nil {
		return fmt.Errorf("close ticket key cache: %w", err)
	}
	if err := os.Rename(tempPath, path); err != nil {
		return fmt.Errorf("replace ticket key cache: %w", err)
	}
	return nil
}

func (r *directTicketKeyRefresher) requestAfterVerificationFailure(ctx context.Context) {
	if r == nil || r.pinned {
		return
	}
	now := time.Now().UnixNano()
	for {
		last := r.lastAsked.Load()
		if last != 0 && time.Duration(now-last) < directTicketKeyFailureCooldown {
			return
		}
		if r.lastAsked.CompareAndSwap(last, now) {
			break
		}
	}
	go func() {
		if err := r.refresh(ctx); err != nil && !errors.Is(err, errTicketKeyURLDisallowed) {
			log.Printf("[direct] ticket key refresh after signature failure failed: %v", err)
		}
	}()
}

func (r *directTicketKeyRefresher) run(ctx context.Context) {
	if r == nil || r.pinned {
		return
	}
	ticker := time.NewTicker(directTicketKeyRefreshInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			if err := r.refresh(ctx); err != nil && !errors.Is(err, errTicketKeyURLDisallowed) {
				log.Printf("[direct] periodic ticket key refresh failed: %v", err)
			}
		}
	}
}

func runDirectMode(registry *machineRegistry, sessions *sessionRegistry, certificates *relayCertificateManager, store *store, frameAddr string) error {
	machineIDPath := strings.TrimSpace(os.Getenv("FLUX_MACHINE_ID_FILE"))
	if machineIDPath == "" {
		machineIDPath = "/var/lib/flux/machine-id"
	}
	machineIDBytes, err := os.ReadFile(machineIDPath)
	if err != nil {
		return fmt.Errorf("read direct machine ID %s: %w", machineIDPath, err)
	}
	machineID := strings.TrimSpace(string(machineIDBytes))
	if machineID == "" {
		return errors.New("direct machine ID is empty")
	}
	if !isLoopbackAddress(frameAddr) {
		return fmt.Errorf("direct mode upstream must be loopback, got %q", frameAddr)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	verifier, err := loadDirectTicketVerifier(ctx)
	if err != nil {
		return err
	}
	keyRefresher := newDirectTicketKeyRefresher(verifier)
	go keyRefresher.run(ctx)
	// TODO(phase 2b): forward login and clipboard audit events to the central relay.
	statusPath := strings.TrimSpace(os.Getenv("FLUX_DIRECT_STATUS_FILE"))
	if statusPath == "" {
		statusPath = "/run/flux/direct.json"
	}
	certificates.setOnHashesChanged(func() {
		if err := writeDirectStatus(statusPath, certificates.certificateHashes()); err != nil {
			log.Printf("[direct] update endpoint status: %v", err)
		}
	})
	if err := writeDirectStatus(statusPath, certificates.certificateHashes()); err != nil {
		return fmt.Errorf("write direct endpoint status: %w", err)
	}
	go certificates.runRotation(ctx)
	server := newWebTransportServer(registry, sessions, certificates, nil, store, &directWebTransportMode{
		machineID:  machineID,
		verifier:   verifier,
		refreshKey: func() { keyRefresher.requestAfterVerificationFailure(ctx) },
	})
	done, err := startWebTransport(server)
	if err != nil {
		return err
	}
	log.Printf("[direct] serving machine %s on WebTransport UDP %s", machineID, webTransportListenAddr())
	return <-done
}

func isLoopbackAddress(address string) bool {
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return false
	}
	if strings.EqualFold(host, "localhost") {
		return true
	}
	ip := net.ParseIP(strings.Trim(host, "[]"))
	return ip != nil && ip.IsLoopback()
}

func loadDirectTicketVerifier(ctx context.Context) (*ticketVerifier, error) {
	decode := func(encoded string) (*ticketVerifier, error) {
		key, err := base64.StdEncoding.DecodeString(strings.TrimSpace(encoded))
		if err != nil {
			return nil, fmt.Errorf("decode ticket public key: %w", err)
		}
		return newTicketVerifier(key)
	}
	if encoded := strings.TrimSpace(os.Getenv("FLUX_TICKET_PUBLIC_KEY")); encoded != "" {
		relayURL := strings.TrimRight(strings.TrimSpace(os.Getenv("FLUX_RELAY_URL")), "/")
		if relayURL != "" && !ticketKeyURLAllowed(relayURL) {
			log.Printf("[direct] refusing ticket-key fetch from disallowed FLUX_RELAY_URL")
		}
		return decode(encoded)
	}
	relayURL := strings.TrimRight(strings.TrimSpace(os.Getenv("FLUX_RELAY_URL")), "/")
	relayURLAllowed := ticketKeyURLAllowed(relayURL)
	unsafeURLLogged := false
	if relayURL != "" && !relayURLAllowed {
		log.Printf("[direct] refusing ticket-key fetch from disallowed FLUX_RELAY_URL")
		unsafeURLLogged = true
	}
	cachePath := filepath.Join(storeDirFromEnv(), "ticket-key.pub")
	var cached string
	if data, err := os.ReadFile(cachePath); err == nil {
		cached = strings.TrimSpace(string(data))
		if _, err := decode(cached); err != nil {
			log.Printf("[direct] cached ticket key is invalid: %v", err)
			cached = ""
		}
	}
	backoff := time.Second
	for {
		token := strings.TrimSpace(os.Getenv("FLUX_RELAY_TOKEN"))
		if relayURLAllowed && token != "" {
			request, err := http.NewRequestWithContext(ctx, http.MethodGet, relayURL+"/api/relay/ticket-key", nil)
			if err == nil {
				request.Header.Set("Authorization", "Bearer "+token)
				client := &http.Client{Timeout: 10 * time.Second}
				response, requestErr := client.Do(request)
				if requestErr == nil {
					var body struct {
						Algorithm string `json:"alg"`
						PublicKey string `json:"public_key"`
					}
					err = json.NewDecoder(response.Body).Decode(&body)
					_ = response.Body.Close()
					if err == nil && response.StatusCode == http.StatusOK && body.Algorithm == "ML-DSA-87" {
						if verifier, keyErr := decode(body.PublicKey); keyErr == nil {
							_ = os.MkdirAll(filepath.Dir(cachePath), 0o700)
							_ = os.WriteFile(cachePath, []byte(body.PublicKey), 0o644)
							return verifier, nil
						} else {
							err = keyErr
						}
					} else if err == nil {
						err = fmt.Errorf("ticket-key endpoint returned %s", response.Status)
					}
				} else {
					err = requestErr
				}
			}
			log.Printf("[direct] ticket key fetch failed: %v", err)
		} else if relayURL == "" || relayURLAllowed {
			log.Printf("[direct] FLUX_RELAY_URL and FLUX_RELAY_TOKEN are required to fetch the ticket key")
		} else if !unsafeURLLogged {
			log.Printf("[direct] refusing ticket-key fetch from disallowed FLUX_RELAY_URL")
			unsafeURLLogged = true
		}
		if cached != "" {
			return decode(cached)
		}
		timer := time.NewTimer(backoff)
		select {
		case <-ctx.Done():
			timer.Stop()
			return nil, ctx.Err()
		case <-timer.C:
		}
		backoff = min(backoff*2, 30*time.Second)
	}
}

func ticketKeyURLAllowed(raw string) bool {
	parsed, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || parsed.Host == "" || parsed.Hostname() == "" {
		return false
	}
	switch strings.ToLower(parsed.Scheme) {
	case "https":
		return true
	case "http":
		host := parsed.Hostname()
		if strings.EqualFold(strings.TrimSuffix(host, "."), "localhost") {
			return true
		}
		ip := net.ParseIP(host)
		return ip != nil && ip.IsLoopback()
	default:
		return false
	}
}

func writeDirectStatus(path string, hashes []string) error {
	data, err := json.Marshal(directStatus{CertHashes: hashes})
	if err != nil {
		return err
	}
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	file, err := os.CreateTemp(dir, ".direct-status-*")
	if err != nil {
		return err
	}
	tempPath := file.Name()
	defer os.Remove(tempPath)
	if err := file.Chmod(0o644); err != nil {
		_ = file.Close()
		return err
	}
	if _, err := file.Write(data); err != nil {
		_ = file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	return os.Rename(tempPath, path)
}

func startWebTransport(server *webtransport.Server) (<-chan error, error) {
	udpConn, err := listenWebTransportUDP()
	if err != nil {
		return nil, err
	}
	done := make(chan error, 1)
	go func() {
		err := server.Serve(udpConn)
		done <- err
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Printf("[webtransport] HTTP/3 server stopped: %v", err)
		}
	}()
	return done, nil
}
