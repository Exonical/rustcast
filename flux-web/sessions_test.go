package main

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
)

func TestSessionRegistryListOrderAndRemove(t *testing.T) {
	registry := newSessionRegistry()
	start := time.Now()
	registry.add(sessionInfo{ID: "later", StartedAt: start.Add(time.Second)}, nil, nil)
	registry.add(sessionInfo{ID: "earlier", StartedAt: start}, nil, nil)

	listed := registry.list()
	if len(listed) != 2 || listed[0].ID != "earlier" || listed[1].ID != "later" {
		t.Fatalf("sessions not sorted by StartedAt: %+v", listed)
	}

	registry.remove("earlier")
	if listed := registry.list(); len(listed) != 1 || listed[0].ID != "later" {
		t.Fatalf("remove did not delete session: %+v", listed)
	}
}

func TestSessionRegistryDisconnectCallsEndOnce(t *testing.T) {
	registry := newSessionRegistry()
	var calls atomic.Int32
	registry.add(sessionInfo{ID: "session"}, nil, func(reason string) {
		if reason != "disconnected by operator" {
			t.Errorf("disconnect reason = %q", reason)
		}
		calls.Add(1)
	})
	if !registry.disconnect("session") || !registry.disconnect("session") {
		t.Fatal("known session should disconnect")
	}
	if calls.Load() != 1 {
		t.Fatalf("end called %d times, want once", calls.Load())
	}
	if registry.disconnect("unknown") {
		t.Fatal("unknown session should not disconnect")
	}
}

func TestSessionRoutesListAndDisconnect(t *testing.T) {
	gin.SetMode(gin.TestMode)
	router := gin.New()
	registry := newSessionRegistry()
	var endCalls atomic.Int32
	registry.add(sessionInfo{
		ID:          "session-1",
		MachineID:   "machine-1",
		MachineName: "Workstation",
		ClientIP:    "192.0.2.1",
		UserAgent:   "Chrome",
		StartedAt:   time.Unix(1, 0).UTC(),
		Width:       1280,
		Height:      720,
	}, nil, func(string) { endCalls.Add(1) })
	registerSessionRoutes(router.Group("/api"), router.Group("/api"), registry)

	listResponse := httptest.NewRecorder()
	router.ServeHTTP(listResponse, httptest.NewRequest(http.MethodGet, "/api/sessions", nil))
	if listResponse.Code != http.StatusOK {
		t.Fatalf("GET /api/sessions status = %d, want 200", listResponse.Code)
	}
	var sessions []sessionInfo
	if err := json.Unmarshal(listResponse.Body.Bytes(), &sessions); err != nil {
		t.Fatalf("decode session list: %v", err)
	}
	if len(sessions) != 1 || sessions[0].Width != 1280 || sessions[0].Height != 720 {
		t.Fatalf("unexpected session list: %+v", sessions)
	}

	disconnect := func(id string) *httptest.ResponseRecorder {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/api/sessions/"+id+"/disconnect", nil))
		return response
	}
	if response := disconnect("session-1"); response.Code != http.StatusNoContent {
		t.Fatalf("disconnect status = %d, want 204", response.Code)
	}
	if endCalls.Load() != 1 {
		t.Fatalf("end called %d times, want once", endCalls.Load())
	}
	response := disconnect("unknown")
	var body map[string]string
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatalf("decode unknown disconnect response: %v", err)
	}
	if response.Code != http.StatusNotFound || body["error"] != "session not found" {
		t.Fatalf("unknown disconnect response = %d %q", response.Code, response.Body.String())
	}
}
