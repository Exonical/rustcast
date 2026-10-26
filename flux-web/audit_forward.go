package main

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
)

const (
	auditForwardQueueLimit = 1000
	auditForwardBatchLimit = 50
	auditForwardBatchBytes = 6 << 20
	auditForwardBodyLimit  = 8 << 20
)

type auditSink interface {
	recordLogin(hostLogin)
	recordClipboard(auditEvent, bool)
}

type storeAuditSink struct {
	store *store
}

func (s storeAuditSink) recordLogin(ev hostLogin) {
	if s.store == nil {
		return
	}
	if err := s.store.insertLogin(ev); err != nil {
		log.Printf("[login] audit insert failed machine=%s: %v", ev.MachineID, err)
	}
}

func (s storeAuditSink) recordClipboard(ev auditEvent, capture bool) {
	if s.store == nil {
		return
	}
	if err := s.store.insertAudit(ev, capture); err != nil {
		log.Printf("[clipboard] audit insert failed machine=%s: %v", ev.MachineID, err)
	}
}

type forwardingAuditSink struct {
	local     auditSink
	forwarder *auditForwarder
	machineID string
}

func (s forwardingAuditSink) recordLogin(ev hostLogin) {
	if s.local != nil {
		s.local.recordLogin(ev)
	}
	if s.forwarder != nil {
		ev.MachineID = s.machineID
		s.forwarder.enqueueLogin(ev)
	}
}

func (s forwardingAuditSink) recordClipboard(ev auditEvent, capture bool) {
	if s.local != nil {
		s.local.recordClipboard(ev, capture)
	}
	if s.forwarder != nil {
		ev.MachineID = s.machineID
		s.forwarder.enqueueClipboard(ev, capture)
	}
}

type forwardedAuditEvent struct {
	EventID   string  `json:"event_id"`
	Kind      string  `json:"kind"`
	Timestamp string  `json:"ts"`
	SessionID string  `json:"session_id,omitempty"`
	UserSub   string  `json:"user_sub,omitempty"`
	UserName  string  `json:"user_name,omitempty"`
	ClientIP  string  `json:"client_ip,omitempty"`
	OSUser    string  `json:"os_user,omitempty"`
	Direction string  `json:"direction,omitempty"`
	Mime      string  `json:"mime,omitempty"`
	Result    string  `json:"result"`
	Size      int64   `json:"size,omitempty"`
	SHA256    string  `json:"sha256,omitempty"`
	Data      *string `json:"data,omitempty"`
}

type forwardedAuditBatch struct {
	MachineID string                `json:"machine_id"`
	Events    []forwardedAuditEvent `json:"events"`
}

type relayAuditEvent struct {
	EventID   string  `json:"event_id"`
	Kind      string  `json:"kind"`
	Timestamp string  `json:"ts"`
	SessionID string  `json:"session_id"`
	UserSub   string  `json:"user_sub"`
	UserName  string  `json:"user_name"`
	ClientIP  string  `json:"client_ip"`
	OSUser    string  `json:"os_user"`
	Direction string  `json:"direction"`
	Mime      string  `json:"mime"`
	Result    string  `json:"result"`
	Size      int64   `json:"size"`
	SHA256    string  `json:"sha256"`
	Data      *string `json:"data"`
}

func registerRelayAuditRoute(machineAPI *gin.RouterGroup, registry *machineRegistry, store *store) {
	machineAPI.POST("/relay/audit", func(c *gin.Context) {
		body, err := relayAuditBody(c)
		if err != nil {
			var tooLarge *http.MaxBytesError
			if errors.As(err, &tooLarge) {
				c.JSON(http.StatusRequestEntityTooLarge, gin.H{"error": "audit request exceeds 8 MiB"})
			} else {
				c.JSON(http.StatusBadRequest, gin.H{"error": "invalid audit request"})
			}
			return
		}
		var batch forwardedAuditBatch
		if err := json.Unmarshal(body, &batch); err != nil || batch.MachineID == "" || batch.Events == nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "invalid audit request"})
			return
		}
		if len(batch.Events) > auditForwardBatchLimit {
			c.JSON(http.StatusBadRequest, gin.H{"error": "audit batch exceeds 50 events"})
			return
		}
		_, ok := registry.registeredMachine(batch.MachineID)
		if !ok {
			c.JSON(http.StatusNotFound, gin.H{"error": "unknown machine"})
			return
		}
		machineName := registry.displayName(batch.MachineID)
		captureContent := store != nil && store.getPolicy(batch.MachineID).CaptureContent
		type stagedAuditRow struct {
			login     *hostLogin
			clipboard *auditEvent
		}
		rows := make([]stagedAuditRow, 0, len(batch.Events))
		for _, event := range batch.Events {
			if len(event.EventID) != 32 {
				c.JSON(http.StatusBadRequest, gin.H{"error": "invalid audit event ID"})
				return
			}
			if _, err := hex.DecodeString(event.EventID); err != nil {
				c.JSON(http.StatusBadRequest, gin.H{"error": "invalid audit event ID"})
				return
			}
			switch event.Kind {
			case "login":
				row := hostLogin{
					MachineID: batch.MachineID, MachineName: machineName,
					EventID: event.EventID, Timestamp: event.Timestamp,
					SessionID: event.SessionID, UserSub: event.UserSub, UserName: event.UserName,
					ClientIP: event.ClientIP, OSUser: event.OSUser, Result: event.Result,
				}
				rows = append(rows, stagedAuditRow{login: &row})
			case "clipboard":
				if event.Size < 0 || len(event.SHA256) != sha256.Size*2 {
					c.JSON(http.StatusBadRequest, gin.H{"error": "invalid clipboard audit metadata"})
					return
				}
				if digest, err := hex.DecodeString(event.SHA256); err != nil || len(digest) != sha256.Size {
					c.JSON(http.StatusBadRequest, gin.H{"error": "invalid clipboard audit metadata"})
					return
				}
				var data []byte
				if event.Data != nil {
					decoded, err := base64.StdEncoding.DecodeString(*event.Data)
					if err != nil {
						c.JSON(http.StatusBadRequest, gin.H{"error": "invalid clipboard audit content"})
						return
					}
					if int64(len(decoded)) != event.Size || sha256Hex(decoded) != event.SHA256 {
						c.JSON(http.StatusBadRequest, gin.H{"error": "clipboard audit digest mismatch"})
						return
					}
					data = decoded
				}
				if !captureContent {
					data = nil
				}
				row := auditEvent{
					MachineID: batch.MachineID, MachineName: machineName,
					EventID: event.EventID, Timestamp: event.Timestamp,
					SessionID: event.SessionID, UserSub: event.UserSub, UserName: event.UserName,
					ClientIP: event.ClientIP, OSUser: event.OSUser, Direction: event.Direction,
					Mime: event.Mime, Result: event.Result, Data: data,
					Size: event.Size, SHA256: event.SHA256,
				}
				rows = append(rows, stagedAuditRow{clipboard: &row})
			default:
				c.JSON(http.StatusBadRequest, gin.H{"error": "unknown audit event kind"})
				return
			}
		}
		sink := storeAuditSink{store: store}
		for _, row := range rows {
			if row.login != nil {
				sink.recordLogin(*row.login)
			} else if row.clipboard != nil {
				sink.recordClipboard(*row.clipboard, captureContent)
			}
		}
		c.JSON(http.StatusOK, gin.H{"accepted": len(batch.Events)})
	})
}

type auditForwarder struct {
	machineID string
	endpoint  string
	token     string
	client    *http.Client
	metrics   *relayMetrics

	mu           sync.Mutex
	queue        []forwardedAuditEvent
	inflightSize int
	wake         chan struct{}
	backoffStart time.Duration
}

func newDirectAuditForwarder(machineID string, metrics *relayMetrics) *auditForwarder {
	relayURL := strings.TrimRight(strings.TrimSpace(os.Getenv("FLUX_RELAY_URL")), "/")
	token := strings.TrimSpace(os.Getenv("FLUX_RELAY_TOKEN"))
	if relayURL == "" || token == "" || !ticketKeyURLAllowed(relayURL) {
		log.Printf("[audit] WARN: direct audit forwarding disabled; a safe FLUX_RELAY_URL and FLUX_RELAY_TOKEN are required")
		return nil
	}
	return newAuditForwarder(machineID, relayURL, token, metrics)
}

func newAuditForwarder(machineID, relayURL, token string, metrics *relayMetrics) *auditForwarder {
	return &auditForwarder{
		machineID:    machineID,
		endpoint:     strings.TrimRight(relayURL, "/") + "/api/relay/audit",
		token:        token,
		client:       &http.Client{Timeout: 10 * time.Second},
		metrics:      metrics,
		wake:         make(chan struct{}, 1),
		backoffStart: time.Second,
	}
}

func (f *auditForwarder) enqueueLogin(ev hostLogin) {
	if f == nil {
		return
	}
	eventID, err := newAuditEventID()
	if err != nil {
		log.Printf("[audit] create login event ID: %v", err)
		return
	}
	f.enqueue(forwardedAuditEvent{
		EventID: eventID, Kind: "login", Timestamp: auditTimestamp(ev.Timestamp),
		SessionID: ev.SessionID, UserSub: ev.UserSub, UserName: ev.UserName,
		ClientIP: ev.ClientIP, OSUser: ev.OSUser, Result: ev.Result,
	})
}

func (f *auditForwarder) enqueueClipboard(ev auditEvent, capture bool) {
	if f == nil {
		return
	}
	eventID, err := newAuditEventID()
	if err != nil {
		log.Printf("[audit] create clipboard event ID: %v", err)
		return
	}
	size := int64(len(ev.Data))
	hash := sha256Hex(ev.Data)
	event := forwardedAuditEvent{
		EventID: eventID, Kind: "clipboard", Timestamp: auditTimestamp(ev.Timestamp),
		SessionID: ev.SessionID, UserSub: ev.UserSub, UserName: ev.UserName,
		ClientIP: ev.ClientIP, OSUser: ev.OSUser, Direction: ev.Direction,
		Mime: ev.Mime, Result: ev.Result, Size: size, SHA256: hash,
	}
	if capture {
		data := base64.StdEncoding.EncodeToString(ev.Data)
		event.Data = &data
	}
	f.enqueue(event)
}

func newAuditEventID() (string, error) {
	random := make([]byte, 16)
	if _, err := rand.Read(random); err != nil {
		return "", err
	}
	return hex.EncodeToString(random), nil
}

func auditTimestamp(timestamp string) string {
	if _, err := time.Parse(time.RFC3339Nano, timestamp); err == nil {
		return timestamp
	}
	return time.Now().UTC().Format(time.RFC3339Nano)
}

func (f *auditForwarder) enqueue(event forwardedAuditEvent) {
	f.mu.Lock()
	if len(f.queue)+f.inflightSize >= auditForwardQueueLimit {
		if len(f.queue) > 0 {
			f.queue = f.queue[1:]
			if f.metrics != nil {
				f.metrics.auditForwardDropped.Inc()
			}
		}
	}
	f.queue = append(f.queue, event)
	f.updateQueueGaugeLocked()
	f.mu.Unlock()
	select {
	case f.wake <- struct{}{}:
	default:
	}
}

func (f *auditForwarder) updateQueueGaugeLocked() {
	if f.metrics != nil {
		f.metrics.auditForwardQueue.Set(float64(len(f.queue) + f.inflightSize))
	}
}

func (f *auditForwarder) nextBatch() []forwardedAuditEvent {
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.queue) == 0 {
		return nil
	}
	batch := make([]forwardedAuditEvent, 0, min(len(f.queue), auditForwardBatchLimit))
	for _, event := range f.queue {
		if len(batch) == auditForwardBatchLimit {
			break
		}
		candidate := append(append([]forwardedAuditEvent(nil), batch...), event)
		body, err := json.Marshal(forwardedAuditBatch{MachineID: f.machineID, Events: candidate})
		if err != nil {
			break
		}
		if len(batch) > 0 && len(body) > auditForwardBatchBytes {
			break
		}
		batch = candidate
	}
	f.queue = append([]forwardedAuditEvent(nil), f.queue[len(batch):]...)
	f.inflightSize = len(batch)
	f.updateQueueGaugeLocked()
	return batch
}

func (f *auditForwarder) completeBatch(count int) {
	f.mu.Lock()
	f.inflightSize -= count
	f.updateQueueGaugeLocked()
	f.mu.Unlock()
}

func (f *auditForwarder) retryBatch(batch []forwardedAuditEvent) {
	f.mu.Lock()
	queue := make([]forwardedAuditEvent, 0, len(batch)+len(f.queue))
	queue = append(queue, batch...)
	queue = append(queue, f.queue...)
	f.queue = queue
	f.inflightSize -= len(batch)
	f.updateQueueGaugeLocked()
	f.mu.Unlock()
}

func (f *auditForwarder) postBatch(ctx context.Context, events []forwardedAuditEvent) error {
	body, err := json.Marshal(forwardedAuditBatch{MachineID: f.machineID, Events: events})
	if err != nil {
		return fmt.Errorf("encode audit batch: %w", err)
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, f.endpoint, strings.NewReader(string(body)))
	if err != nil {
		return err
	}
	request.Header.Set("Authorization", "Bearer "+f.token)
	request.Header.Set("Content-Type", "application/json")
	response, err := f.client.Do(request)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	_, _ = io.Copy(io.Discard, io.LimitReader(response.Body, 4096))
	if response.StatusCode < http.StatusOK || response.StatusCode >= http.StatusMultipleChoices {
		if response.StatusCode == http.StatusBadRequest || response.StatusCode == http.StatusRequestEntityTooLarge {
			return &auditForwardPermanentError{status: response.Status}
		}
		return fmt.Errorf("audit endpoint returned %s", response.Status)
	}
	return nil
}

type auditForwardPermanentError struct {
	status string
}

func (e *auditForwardPermanentError) Error() string {
	return fmt.Sprintf("audit endpoint permanently rejected batch: %s", e.status)
}

func (f *auditForwarder) run(ctx context.Context) {
	backoff := f.backoffStart
	for {
		batch := f.nextBatch()
		if len(batch) == 0 {
			select {
			case <-ctx.Done():
				return
			case <-f.wake:
			}
			continue
		}
		if err := f.postBatch(ctx, batch); err != nil {
			var permanent *auditForwardPermanentError
			if errors.As(err, &permanent) {
				f.completeBatch(len(batch))
				if f.metrics != nil {
					f.metrics.auditForwardDropped.Add(float64(len(batch)))
				}
				log.Printf("[audit] dropping permanently rejected batch events=%d status=%s", len(batch), permanent.status)
				backoff = f.backoffStart
				continue
			}
			f.retryBatch(batch)
			timer := time.NewTimer(backoff)
			select {
			case <-ctx.Done():
				timer.Stop()
				return
			case <-timer.C:
			}
			backoff = min(backoff*2, 60*time.Second)
			continue
		}
		f.completeBatch(len(batch))
		backoff = f.backoffStart
	}
}

func relayAuditBody(c *gin.Context) ([]byte, error) {
	reader := http.MaxBytesReader(c.Writer, c.Request.Body, auditForwardBodyLimit)
	body, err := io.ReadAll(reader)
	if err != nil {
		var tooLarge *http.MaxBytesError
		if errors.As(err, &tooLarge) {
			return nil, tooLarge
		}
		return nil, err
	}
	return body, nil
}
