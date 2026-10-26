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
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/quic-go/webtransport-go"
)

type directStatus struct {
	CertHashes []string `json:"cert_hashes"`
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
	// TODO(phase 2b): forward login and clipboard audit events to the central relay.
	statusPath := strings.TrimSpace(os.Getenv("FLUX_DIRECT_STATUS_FILE"))
	if statusPath == "" {
		statusPath = "/run/flux/direct.json"
	}
	if err := writeDirectStatus(statusPath, certificates.certificateHashes()); err != nil {
		return fmt.Errorf("write direct endpoint status: %w", err)
	}
	go refreshDirectStatus(ctx, statusPath, certificates)
	go certificates.runRotation(ctx)
	server := newWebTransportServer(registry, sessions, certificates, nil, store, &directWebTransportMode{
		machineID: machineID,
		verifier:  verifier,
	})
	done, err := startWebTransport(server)
	if err != nil {
		return err
	}
	log.Printf("[direct] serving machine %s on WebTransport UDP :%d", machineID, webTransportPort)
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
		return decode(encoded)
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
		relayURL := strings.TrimRight(strings.TrimSpace(os.Getenv("FLUX_RELAY_URL")), "/")
		token := strings.TrimSpace(os.Getenv("FLUX_RELAY_TOKEN"))
		if relayURL != "" && token != "" {
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
		} else {
			log.Printf("[direct] FLUX_RELAY_URL and FLUX_RELAY_TOKEN are required to fetch the ticket key")
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

func refreshDirectStatus(ctx context.Context, path string, certificates *relayCertificateManager) {
	ticker := time.NewTicker(time.Minute)
	defer ticker.Stop()
	var previous string
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			hashes := certificates.certificateHashes()
			data, _ := json.Marshal(directStatus{CertHashes: hashes})
			if string(data) != previous {
				if err := writeDirectStatus(path, hashes); err != nil {
					log.Printf("[direct] update endpoint status: %v", err)
				} else {
					previous = string(data)
				}
			}
		}
	}
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
