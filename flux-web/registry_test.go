package main

import (
	"bytes"
	"encoding/json"
	"net"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
)

func testRegistration(id string) machineRegistration {
	return machineRegistration{
		ID: id, Name: "Test machine", FrameEndpoint: "127.0.0.1:1",
	}
}

func TestMachineRegistryHeartbeatKeepsOnline(t *testing.T) {
	r := newMachineRegistry()
	r.upsert(testRegistration("machine-1"), false)
	first := r.machines["machine-1"].LastSeen
	time.Sleep(time.Millisecond)
	r.upsert(testRegistration("machine-1"), false)
	m, err := r.get("machine-1")
	if err != nil {
		t.Fatalf("get after heartbeat: %v", err)
	}
	if m.Status != "online" || !m.LastSeen.After(first) {
		t.Fatalf("heartbeat did not keep machine online: %+v", m.machineInfo)
	}
}

func TestMachineRegistryExpiryAndStaticEntry(t *testing.T) {
	r := newMachineRegistry()
	r.seedStatic("default", "Default", "127.0.0.1:1")
	r.upsert(testRegistration("machine-1"), false)
	r.machines["machine-1"].LastSeen = time.Now().Add(-machineExpiry - time.Second)

	if _, err := r.get("machine-1"); err != errMachineOffline {
		t.Fatalf("expected offline error, got %v", err)
	}
	if _, err := r.acquire("machine-1"); err != errMachineOffline {
		t.Fatalf("expected acquire offline error, got %v", err)
	}
	if _, err := r.get("default"); err != nil {
		t.Fatalf("static entry expired: %v", err)
	}
	if r.machines["machine-1"].Status != "offline" {
		t.Fatal("expired machine was not marked offline")
	}
}

func TestMachineRegistryAcquireReleaseLifecycle(t *testing.T) {
	r := newMachineRegistry()
	r.upsert(testRegistration("machine-1"), false)
	first, err := r.acquire("machine-1")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := r.acquire("machine-1"); err != errMachineInUse {
		t.Fatalf("expected machine-in-use error, got %v", err)
	}
	r.release(first)
	if first.viewers != 0 || r.machines["machine-1"].upstream != nil {
		t.Fatal("last release did not tear down upstream")
	}
	fresh, err := r.acquire("machine-1")
	if err != nil {
		t.Fatal(err)
	}
	if fresh == first {
		t.Fatal("subsequent acquire reused stopped upstream")
	}
	r.release(fresh)
}

func TestHeartbeatRejectsPathIDMismatch(t *testing.T) {
	gin.SetMode(gin.TestMode)
	r := gin.New()
	registerMachineRoutes(r, newMachineRegistry())
	body, _ := json.Marshal(testRegistration("body-id"))
	request := httptest.NewRequest(http.MethodPost, "/api/machines/path-id/heartbeat", bytes.NewReader(body))
	request.Header.Set("Content-Type", "application/json")
	response := httptest.NewRecorder()
	r.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest {
		t.Fatalf("expected 400, got %d", response.Code)
	}
}

func TestRewriteFrameEndpoint(t *testing.T) {
	tests := []struct {
		name     string
		endpoint string
		clientIP string
		want     string
	}{
		{
			name:     "loopback rewritten",
			endpoint: "127.0.0.1:8556",
			clientIP: "192.168.1.70",
			want:     "192.168.1.70:8556",
		},
		{
			name:     "unspecified rewritten",
			endpoint: "0.0.0.0:8556",
			clientIP: "192.168.1.70",
			want:     "192.168.1.70:8556",
		},
		{
			name:     "IPv6 loopback rewritten",
			endpoint: "[::1]:8556",
			clientIP: "2001:db8::70",
			want:     "[2001:db8::70]:8556",
		},
		{
			name:     "IPv6 unspecified rewritten",
			endpoint: "[::]:8556",
			clientIP: "fe80::70",
			want:     "[fe80::70]:8556",
		},
		{
			name:     "routable host left alone",
			endpoint: "192.168.1.70:8556",
			clientIP: "192.168.1.71",
			want:     "192.168.1.70:8556",
		},
		{
			name:     "IPv6 routable host left alone",
			endpoint: "[2001:db8::70]:8556",
			clientIP: "2001:db8::71",
			want:     "[2001:db8::70]:8556",
		},
		{
			name:     "port preserved",
			endpoint: "127.0.0.1:49152",
			clientIP: "10.0.0.12",
			want:     "10.0.0.12:49152",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			gin.SetMode(gin.TestMode)
			c, _ := gin.CreateTestContext(httptest.NewRecorder())
			c.Request = httptest.NewRequest(http.MethodPost, "/", nil)
			c.Request.RemoteAddr = net.JoinHostPort(test.clientIP, "12345")
			if got := rewriteFrameEndpoint(c, test.endpoint); got != test.want {
				t.Fatalf("rewriteFrameEndpoint(%q) = %q, want %q", test.endpoint, got, test.want)
			}
		})
	}
}
