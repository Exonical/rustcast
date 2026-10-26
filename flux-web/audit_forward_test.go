package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
)

func TestAuditForwarderBatchesAndAuthenticates(t *testing.T) {
	gin.SetMode(gin.TestMode)
	var mu sync.Mutex
	var sizes []int
	var authHeaders []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var batch forwardedAuditBatch
		if err := json.NewDecoder(r.Body).Decode(&batch); err != nil {
			t.Errorf("decode request: %v", err)
		}
		mu.Lock()
		sizes = append(sizes, len(batch.Events))
		authHeaders = append(authHeaders, r.Header.Get("Authorization"))
		mu.Unlock()
		w.WriteHeader(http.StatusOK)
	}))
	defer server.Close()
	forwarder := newAuditForwarder("machine-1", server.URL, "relay-token", nil, nil)
	for i := 0; i < 55; i++ {
		forwarder.enqueue(forwardedAuditEvent{EventID: fmt.Sprintf("%032x", i+1), Kind: "login", Result: "ok"})
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan struct{})
	go func() {
		forwarder.run(ctx)
		close(done)
	}()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		mu.Lock()
		complete := len(sizes) == 2
		mu.Unlock()
		if complete {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	cancel()
	<-done
	mu.Lock()
	defer mu.Unlock()
	if len(sizes) != 2 || sizes[0] != 50 || sizes[1] != 5 {
		t.Fatalf("batch sizes = %v, want [50 5]", sizes)
	}
	if len(authHeaders) != 2 || authHeaders[0] != "Bearer relay-token" || authHeaders[1] != "Bearer relay-token" {
		t.Fatalf("authorization headers = %v", authHeaders)
	}
}

func TestAuditForwarderQueuePersistsUntilAcknowledged(t *testing.T) {
	gin.SetMode(gin.TestMode)
	st := testStore(t)
	var mu sync.Mutex
	var sentIDs []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var batch forwardedAuditBatch
		if err := json.NewDecoder(r.Body).Decode(&batch); err != nil {
			t.Errorf("decode request: %v", err)
		}
		mu.Lock()
		for _, event := range batch.Events {
			sentIDs = append(sentIDs, event.EventID)
		}
		mu.Unlock()
		w.WriteHeader(http.StatusOK)
	}))
	defer server.Close()

	first := newAuditForwarder("m", server.URL, "token", nil, st)
	first.enqueue(forwardedAuditEvent{EventID: "event-1", Kind: "login", Result: "ok"})
	first.enqueue(forwardedAuditEvent{EventID: "event-2", Kind: "login", Result: "ok"})
	restarted := newAuditForwarder("m", server.URL, "token", nil, st)
	if len(restarted.queue) != 2 || restarted.queue[0].EventID != "event-1" || restarted.queue[1].EventID != "event-2" {
		t.Fatalf("recovered event IDs = %+v", restarted.queue)
	}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		restarted.run(ctx)
		close(done)
	}()
	waitForTestCondition(t, 2*time.Second, func() bool {
		mu.Lock()
		defer mu.Unlock()
		return len(sentIDs) == 2
	})
	waitForTestCondition(t, 2*time.Second, func() bool {
		var count int
		if err := st.db.QueryRow(`SELECT COUNT(*) FROM audit_forward_queue`).Scan(&count); err != nil {
			return false
		}
		return count == 0
	})
	cancel()
	<-done
	mu.Lock()
	defer mu.Unlock()
	if len(sentIDs) != 2 || sentIDs[0] != "event-1" || sentIDs[1] != "event-2" {
		t.Fatalf("sent event IDs = %v", sentIDs)
	}
}

func TestAuditForwarderDropsExpiredEventsWithoutSending(t *testing.T) {
	var mu sync.Mutex
	requests := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		mu.Lock()
		requests++
		mu.Unlock()
		w.WriteHeader(http.StatusOK)
	}))
	defer server.Close()
	metrics := newRelayMetrics()
	forwarder := newAuditForwarder("m", server.URL, "token", metrics, nil)
	forwarder.retention = 10 * time.Millisecond
	forwarder.enqueue(forwardedAuditEvent{EventID: "expired-event", Kind: "login", Result: "ok"})
	time.Sleep(20 * time.Millisecond)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan struct{})
	go func() {
		forwarder.run(ctx)
		close(done)
	}()
	waitForTestCondition(t, time.Second, func() bool {
		return bytes.Contains([]byte(scrapeMetrics(metrics)),
			[]byte(`flux_relay_audit_forward_dropped_total{path="relay"} 1`))
	})
	cancel()
	<-done
	mu.Lock()
	defer mu.Unlock()
	if requests != 0 {
		t.Fatalf("sent %d expired batches, want none", requests)
	}
}

func TestAuditForwarderDropsOldestAtCapacity(t *testing.T) {
	metrics := newRelayMetrics()
	st := testStore(t)
	forwarder := newAuditForwarder("m", "http://127.0.0.1:1", "token", metrics, st)
	for i := 0; i <= auditForwardQueueLimit; i++ {
		forwarder.enqueue(forwardedAuditEvent{EventID: fmt.Sprintf("%032x", i+1), Kind: "login", Result: "ok"})
	}
	if got := len(forwarder.queue); got != auditForwardQueueLimit {
		t.Fatalf("queue length = %d, want %d", got, auditForwardQueueLimit)
	}
	if forwarder.queue[0].EventID != fmt.Sprintf("%032x", 2) {
		t.Fatalf("oldest queued event = %s, want second event", forwarder.queue[0].EventID)
	}
	body := scrapeMetrics(metrics)
	if !bytes.Contains([]byte(body), []byte("flux_relay_audit_forward_queue{path=\"relay\"} 1000")) ||
		!bytes.Contains([]byte(body), []byte("flux_relay_audit_forward_dropped_total{path=\"relay\"} 1")) {
		t.Fatalf("queue metrics missing drop/gauge values: %s", body)
	}
	var persisted int
	if err := st.db.QueryRow(`SELECT COUNT(*) FROM audit_forward_queue`).Scan(&persisted); err != nil {
		t.Fatal(err)
	}
	if persisted != auditForwardQueueLimit {
		t.Fatalf("persisted queue length = %d, want %d", persisted, auditForwardQueueLimit)
	}
	var oldest int
	if err := st.db.QueryRow(`SELECT COUNT(*) FROM audit_forward_queue WHERE event_id = ?`,
		fmt.Sprintf("%032x", 1)).Scan(&oldest); err != nil {
		t.Fatal(err)
	}
	if oldest != 0 {
		t.Fatal("drop-oldest left the first event persisted")
	}
}

func TestAuditForwarderStartupTrimsPersistedQueue(t *testing.T) {
	st := testStore(t)
	for i := 1; i <= auditForwardQueueLimit+1; i++ {
		eventID := fmt.Sprintf("%032x", i)
		if err := st.persistAuditForwardEvent(forwardedAuditEvent{
			EventID: eventID, Kind: "login", Result: "ok",
		}); err != nil {
			t.Fatal(err)
		}
	}
	metrics := newRelayMetrics()
	forwarder := newAuditForwarder("m", "http://127.0.0.1:1", "token", metrics, st)
	if len(forwarder.queue) != auditForwardQueueLimit {
		t.Fatalf("recovered queue size = %d, want %d", len(forwarder.queue), auditForwardQueueLimit)
	}
	if forwarder.queue[0].EventID != fmt.Sprintf("%032x", 2) {
		t.Fatalf("recovered oldest event = %s", forwarder.queue[0].EventID)
	}
	var persisted int
	if err := st.db.QueryRow(`SELECT COUNT(*) FROM audit_forward_queue`).Scan(&persisted); err != nil {
		t.Fatal(err)
	}
	if persisted != auditForwardQueueLimit ||
		!bytes.Contains([]byte(scrapeMetrics(metrics)), []byte(`flux_relay_audit_forward_dropped_total{path="relay"} 1`)) {
		t.Fatalf("startup trim persisted=%d metrics=%s", persisted, scrapeMetrics(metrics))
	}
}

func TestAuditForwarderStartupDropsExpiredPersistentEvents(t *testing.T) {
	t.Setenv("FLUX_AUDIT_RETENTION_DAYS", "1")
	st := testStore(t)
	oldID := "00000000000000000000000000000001"
	freshID := "00000000000000000000000000000002"
	for _, eventID := range []string{oldID, freshID} {
		if err := st.persistAuditForwardEvent(forwardedAuditEvent{
			EventID: eventID, Kind: "login", Result: "ok",
		}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := st.db.Exec(`UPDATE audit_forward_queue SET created_at = ? WHERE event_id = ?`,
		time.Now().Add(-48*time.Hour).Unix(), oldID); err != nil {
		t.Fatal(err)
	}
	metrics := newRelayMetrics()
	forwarder := newAuditForwarder("m", "http://127.0.0.1:1", "token", metrics, st)
	if len(forwarder.queue) != 1 || forwarder.queue[0].EventID != freshID {
		t.Fatalf("recovered queue = %+v", forwarder.queue)
	}
	var persisted int
	if err := st.db.QueryRow(`SELECT COUNT(*) FROM audit_forward_queue`).Scan(&persisted); err != nil {
		t.Fatal(err)
	}
	if persisted != 1 || !bytes.Contains([]byte(scrapeMetrics(metrics)),
		[]byte(`flux_relay_audit_forward_dropped_total{path="relay"} 1`)) {
		t.Fatalf("persisted=%d metrics=%s", persisted, scrapeMetrics(metrics))
	}
}

func TestAuditForwarderDropsPermanentBatchAndContinues(t *testing.T) {
	metrics := newRelayMetrics()
	var mu sync.Mutex
	var batchSizes []int
	var batchIDs [][]string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var batch forwardedAuditBatch
		if err := json.NewDecoder(r.Body).Decode(&batch); err != nil {
			t.Errorf("decode request: %v", err)
		}
		ids := make([]string, len(batch.Events))
		for i, event := range batch.Events {
			ids[i] = event.EventID
		}
		mu.Lock()
		batchSizes = append(batchSizes, len(batch.Events))
		batchIDs = append(batchIDs, ids)
		attempt := len(batchSizes)
		mu.Unlock()
		if attempt == 1 {
			http.Error(w, "invalid batch", http.StatusBadRequest)
			return
		}
		w.WriteHeader(http.StatusOK)
	}))
	defer server.Close()
	forwarder := newAuditForwarder("m", server.URL, "token", metrics, nil)
	forwarder.backoffStart = time.Minute
	for i := 0; i < auditForwardBatchLimit+1; i++ {
		forwarder.enqueue(forwardedAuditEvent{EventID: fmt.Sprintf("%032x", i+1), Kind: "login", Result: "ok"})
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		forwarder.run(ctx)
		close(done)
	}()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		mu.Lock()
		twoBatchesSent := len(batchSizes) == 2
		mu.Unlock()
		forwarder.mu.Lock()
		queueEmpty := len(forwarder.queue) == 0 && forwarder.inflightSize == 0
		forwarder.mu.Unlock()
		if twoBatchesSent && queueEmpty {
			break
		}
		time.Sleep(time.Millisecond)
	}
	cancel()
	<-done
	mu.Lock()
	defer mu.Unlock()
	if len(batchSizes) != 2 || batchSizes[0] != auditForwardBatchLimit || batchSizes[1] != 1 {
		t.Fatalf("batch sizes = %v, want [%d 1]", batchSizes, auditForwardBatchLimit)
	}
	if len(batchIDs[1]) != 1 || batchIDs[1][0] != fmt.Sprintf("%032x", auditForwardBatchLimit+1) {
		t.Fatalf("batch after permanent rejection = %v, want the next event", batchIDs[1])
	}
	body := scrapeMetrics(metrics)
	if !bytes.Contains([]byte(body), []byte("flux_relay_audit_forward_dropped_total{path=\"relay\"} 50")) ||
		!bytes.Contains([]byte(body), []byte("flux_relay_audit_forward_queue{path=\"relay\"} 0")) {
		t.Fatalf("permanent rejection drop/gauge metrics missing: %s", body)
	}
}

func TestAuditForwarderRetriesNotFoundResponse(t *testing.T) {
	var mu sync.Mutex
	var attempts int
	var eventIDs []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var batch forwardedAuditBatch
		if err := json.NewDecoder(r.Body).Decode(&batch); err != nil {
			t.Errorf("decode request: %v", err)
		}
		mu.Lock()
		attempts++
		eventIDs = append(eventIDs, batch.Events[0].EventID)
		current := attempts
		mu.Unlock()
		if current == 1 {
			http.Error(w, "machine may register later", http.StatusNotFound)
			return
		}
		w.WriteHeader(http.StatusOK)
	}))
	defer server.Close()
	forwarder := newAuditForwarder("m", server.URL, "token", nil, nil)
	forwarder.backoffStart = time.Millisecond
	forwarder.enqueue(forwardedAuditEvent{EventID: "event-one", Kind: "login", Result: "ok"})
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		forwarder.run(ctx)
		close(done)
	}()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		mu.Lock()
		complete := attempts >= 2
		mu.Unlock()
		if complete {
			break
		}
		time.Sleep(time.Millisecond)
	}
	cancel()
	<-done
	mu.Lock()
	defer mu.Unlock()
	if attempts != 2 || len(eventIDs) != 2 || eventIDs[0] != eventIDs[1] {
		t.Fatalf("attempts=%d event IDs=%v; failed batch was not retried", attempts, eventIDs)
	}
}

func TestDirectAuditForwarderDisablesUnsafeOrIncompleteConfiguration(t *testing.T) {
	t.Setenv("FLUX_RELAY_URL", "http://example.test")
	t.Setenv("FLUX_RELAY_TOKEN", "token")
	if forwarder := newDirectAuditForwarder("m", nil, nil); forwarder != nil {
		t.Fatal("unsafe relay URL enabled forwarding")
	}
	t.Setenv("FLUX_RELAY_URL", "https://relay.example.test")
	t.Setenv("FLUX_RELAY_TOKEN", "")
	if forwarder := newDirectAuditForwarder("m", nil, nil); forwarder != nil {
		t.Fatal("missing token enabled forwarding")
	}
	local := httptest.NewServer(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {}))
	defer local.Close()
	t.Setenv("FLUX_RELAY_URL", local.URL)
	t.Setenv("FLUX_RELAY_TOKEN", "token")
	if forwarder := newDirectAuditForwarder("m", nil, nil); forwarder == nil {
		t.Fatal("loopback relay URL was refused")
	}
}

func TestRelayAuditEndpointAuthMachineDedupAndCapturePolicy(t *testing.T) {
	gin.SetMode(gin.TestMode)
	st := testStore(t)
	registry := newMachineRegistry()
	registry.seedStatic("machine-1", "Test Machine", "127.0.0.1:8556")
	router := gin.New()
	api := router.Group("/api", (&authService{cfg: authConfig{relayToken: "relay-token"}}).requireMachineToken())
	registerRelayAuditRoute(api, registry, st)

	request := func(token, machineID, eventID string) *httptest.ResponseRecorder {
		content := "aGVsbG8="
		body, _ := json.Marshal(forwardedAuditBatch{
			MachineID: machineID,
			Events: []forwardedAuditEvent{{
				EventID: eventID, Kind: "clipboard", Timestamp: "2025-01-02T03:04:05Z",
				SessionID: "session-1", UserSub: "sub-1", UserName: "Viewer", ClientIP: "192.0.2.1",
				OSUser: "alice", Direction: clipDirClientToHost, Mime: clipWireMime, Result: clipResultOK,
				Size: 5, SHA256: sha256Hex([]byte("hello")), Data: &content,
			}},
		})
		req := httptest.NewRequest(http.MethodPost, "/api/relay/audit", bytes.NewReader(body))
		if token != "" {
			req.Header.Set("Authorization", "Bearer "+token)
		}
		response := httptest.NewRecorder()
		router.ServeHTTP(response, req)
		return response
	}

	if response := request("", "machine-1", "00000000000000000000000000000001"); response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated status = %d, want 401", response.Code)
	}
	if response := request("relay-token", "unknown", "00000000000000000000000000000002"); response.Code != http.StatusNotFound {
		t.Fatalf("unknown machine status = %d, want 404", response.Code)
	}

	policy := defaultMachinePolicy()
	policy.CaptureContent = false
	if err := st.putPolicy("machine-1", policy, "test"); err != nil {
		t.Fatal(err)
	}
	eventID := "00000000000000000000000000000003"
	for i := 0; i < 2; i++ {
		response := request("relay-token", "machine-1", eventID)
		if response.Code != http.StatusOK {
			t.Fatalf("audit status = %d: %s", response.Code, response.Body)
		}
	}
	var rows int
	var size int64
	var hash string
	var content []byte
	if err := st.db.QueryRow(`SELECT COUNT(*), MAX(size), MAX(sha256), MAX(content) FROM clipboard_events WHERE event_id = ?`, eventID).
		Scan(&rows, &size, &hash, &content); err != nil {
		t.Fatal(err)
	}
	if rows != 1 || size != 5 || hash != sha256Hex([]byte("hello")) || len(content) != 0 {
		t.Fatalf("digest-only deduplicated row: count=%d size=%d hash=%q content=%q", rows, size, hash, content)
	}

	policy.CaptureContent = true
	if err := st.putPolicy("machine-1", policy, "test"); err != nil {
		t.Fatal(err)
	}
	response := request("relay-token", "machine-1", "00000000000000000000000000000004")
	if response.Code != http.StatusOK {
		t.Fatalf("captured audit status = %d: %s", response.Code, response.Body)
	}
	if err := st.db.QueryRow(`SELECT content FROM clipboard_events WHERE event_id = ?`, "00000000000000000000000000000004").Scan(&content); err != nil {
		t.Fatal(err)
	}
	if string(content) != "hello" {
		t.Fatalf("captured content = %q, want hello", content)
	}

	policy.MaxBytes = 4
	if err := st.putPolicy("machine-1", policy, "test"); err != nil {
		t.Fatal(err)
	}
	response = request("relay-token", "machine-1", "00000000000000000000000000000005")
	if response.Code != http.StatusOK {
		t.Fatalf("over-MaxBytes audit status = %d: %s", response.Code, response.Body)
	}
	if err := st.db.QueryRow(`SELECT content FROM clipboard_events WHERE event_id = ?`,
		"00000000000000000000000000000005").Scan(&content); err != nil {
		t.Fatal(err)
	}
	if len(content) != 0 {
		t.Fatalf("content above MaxBytes stored %q, want digest only", content)
	}
}

func TestAuditForwarderLargeCapturedClipboardIsMetadataOnlyAndAccepted(t *testing.T) {
	gin.SetMode(gin.TestMode)
	st := testStore(t)
	registry := newMachineRegistry()
	registry.seedStatic("machine-1", "Test Machine", "127.0.0.1:8556")
	router := gin.New()
	api := router.Group("/api", (&authService{cfg: authConfig{relayToken: "token"}}).requireMachineToken())
	registerRelayAuditRoute(api, registry, st)
	server := httptest.NewServer(router)
	defer server.Close()

	forwarder := newAuditForwarder("machine-1", server.URL, "token", nil, nil)
	data := bytes.Repeat([]byte("x"), 5<<20)
	forwarder.enqueueClipboard(auditEvent{
		MachineID: "machine-1", Direction: clipDirClientToHost, Result: clipResultOK, Data: data,
	}, true)
	batch := forwarder.nextBatch()
	if len(batch) != 1 || batch[0].Data != nil || batch[0].Size != int64(len(data)) ||
		batch[0].SHA256 != sha256Hex(data) {
		t.Fatalf("forwarded large clipboard event = %+v", batch)
	}
	if err := forwarder.postBatch(context.Background(), batch); err != nil {
		t.Fatalf("relay rejected metadata-only large event: %v", err)
	}
	var size int64
	var hash string
	var content []byte
	if err := st.db.QueryRow(`SELECT size, sha256, content FROM clipboard_events WHERE event_id = ?`,
		batch[0].EventID).Scan(&size, &hash, &content); err != nil {
		t.Fatal(err)
	}
	if size != int64(len(data)) || hash != sha256Hex(data) || len(content) != 0 {
		t.Fatalf("large event persisted size=%d hash=%q content=%d bytes", size, hash, len(content))
	}
}

func TestRelayAuditEndpointReturnsUnavailableWhenStoreInsertFails(t *testing.T) {
	gin.SetMode(gin.TestMode)
	st := testStore(t)
	if err := st.db.Close(); err != nil {
		t.Fatal(err)
	}
	registry := newMachineRegistry()
	registry.seedStatic("machine-1", "Test Machine", "127.0.0.1:8556")
	router := gin.New()
	api := router.Group("/api", (&authService{cfg: authConfig{relayToken: "token"}}).requireMachineToken())
	registerRelayAuditRoute(api, registry, st)
	body, err := json.Marshal(forwardedAuditBatch{
		MachineID: "machine-1",
		Events: []forwardedAuditEvent{{
			EventID: "00000000000000000000000000000001", Kind: "login", Result: "ok",
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/api/relay/audit", bytes.NewReader(body))
	request.Header.Set("Authorization", "Bearer token")
	response := httptest.NewRecorder()
	router.ServeHTTP(response, request)
	if response.Code != http.StatusServiceUnavailable || !bytes.Contains(response.Body.Bytes(), []byte("audit store unavailable")) {
		t.Fatalf("closed-store response = %d %s, want 503", response.Code, response.Body)
	}
}

func TestRelayAuditEndpointValidatesBatchBeforeInserting(t *testing.T) {
	gin.SetMode(gin.TestMode)
	st := testStore(t)
	registry := newMachineRegistry()
	registry.seedStatic("machine-1", "Test Machine", "127.0.0.1:8556")
	router := gin.New()
	api := router.Group("/api", (&authService{cfg: authConfig{relayToken: "relay-token"}}).requireMachineToken())
	registerRelayAuditRoute(api, registry, st)

	invalidContent := "not-base64"
	batch := forwardedAuditBatch{
		MachineID: "machine-1",
		Events: []forwardedAuditEvent{
			{
				EventID: "00000000000000000000000000000011", Kind: "login",
				Timestamp: "2025-01-02T03:04:05Z", Result: "ok",
			},
			{
				EventID: "00000000000000000000000000000012", Kind: "clipboard",
				Timestamp: "2025-01-02T03:04:05Z", Size: 5,
				SHA256: sha256Hex([]byte("hello")), Data: &invalidContent,
				Direction: clipDirClientToHost, Result: clipResultOK,
			},
		},
	}
	body, err := json.Marshal(batch)
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/api/relay/audit", bytes.NewReader(body))
	request.Header.Set("Authorization", "Bearer relay-token")
	response := httptest.NewRecorder()
	router.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest {
		t.Fatalf("invalid batch status = %d, want 400: %s", response.Code, response.Body)
	}
	var inserted int
	if err := st.db.QueryRow(`SELECT
		(SELECT COUNT(*) FROM host_logins) + (SELECT COUNT(*) FROM clipboard_events)`).Scan(&inserted); err != nil {
		t.Fatal(err)
	}
	if inserted != 0 {
		t.Fatalf("invalid batch inserted %d events, want none", inserted)
	}
}

func TestRelayAuditEndpointRejectsOversizedBody(t *testing.T) {
	gin.SetMode(gin.TestMode)
	registry := newMachineRegistry()
	router := gin.New()
	api := router.Group("/api", (&authService{cfg: authConfig{relayToken: "token"}}).requireMachineToken())
	registerRelayAuditRoute(api, registry, nil)
	request := httptest.NewRequest(http.MethodPost, "/api/relay/audit", bytes.NewReader(bytes.Repeat([]byte("x"), auditForwardBodyLimit+1)))
	request.Header.Set("Authorization", "Bearer token")
	response := httptest.NewRecorder()
	router.ServeHTTP(response, request)
	if response.Code != http.StatusRequestEntityTooLarge {
		t.Fatalf("oversized status = %d, want 413", response.Code)
	}
}
