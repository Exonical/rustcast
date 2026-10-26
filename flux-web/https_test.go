package main

import (
	"bytes"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"net"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
)

func TestWebHTTPSApiUsesTrustedRelayCertificate(t *testing.T) {
	certificates, err := newRelayCertificateManager("", "", "localhost")
	if err != nil {
		t.Fatal(err)
	}
	certificate := certificates.certificate.Load()
	if certificate == nil || certificate.Leaf == nil {
		t.Fatal("relay certificate is missing")
	}

	handler := newWebHandler(newMachineRegistry(), newSessionRegistry(), certificates, "localhost", disabledAuth(t), nil, nil)
	server := httptest.NewUnstartedServer(withAltSvc(handler))
	server.TLS = webHTTPSTLSConfig(certificates)
	server.TLS.Certificates = []tls.Certificate{*certificate}
	server.TLS.NextProtos = []string{"http/1.1"}
	server.StartTLS()
	defer server.Close()

	roots := x509.NewCertPool()
	roots.AddCert(certificate.Leaf)
	client := &http.Client{
		Timeout: 3 * time.Second,
		Transport: &http.Transport{
			TLSClientConfig: &tls.Config{MinVersion: tls.VersionTLS13, RootCAs: roots},
		},
	}
	response, err := client.Get(server.URL + "/api/webtransport")
	if err != nil {
		t.Fatalf("GET /api/webtransport with trusted relay certificate: %v", err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		t.Fatalf("HTTP status = %d, want %d", response.StatusCode, http.StatusOK)
	}
	if got := response.Header.Get("Alt-Svc"); got != `h3=":8443"` {
		t.Fatalf("Alt-Svc = %q, want %q", got, `h3=":8443"`)
	}
	var body struct {
		Host string `json:"host"`
		Port int    `json:"port"`
	}
	if err := json.NewDecoder(response.Body).Decode(&body); err != nil {
		t.Fatal(err)
	}
	if body.Host != "localhost" || body.Port != webTransportPort {
		t.Fatalf("API response = %+v, want host localhost and port %d", body, webTransportPort)
	}
}

func TestHTTPPageRequestsRedirectToHTTPS(t *testing.T) {
	tests := []struct {
		name         string
		httpsAddr    string
		redirectHost string
	}{
		{name: "wildcard bind uses request host", httpsAddr: ":9443", redirectHost: "relay.example"},
		{name: "IPv4 wildcard bind uses request host", httpsAddr: "0.0.0.0:9443", redirectHost: "relay.example"},
		{name: "IPv6 wildcard bind uses request host", httpsAddr: "[::]:9443", redirectHost: "relay.example"},
		{name: "specific bind uses configured host", httpsAddr: "relay-internal:9443", redirectHost: "relay-internal"},
	}
	for _, test := range tests {
		for _, method := range []string{http.MethodGet, http.MethodHead} {
			t.Run(test.name+"/"+method, func(t *testing.T) {
				handler := withHTTPSRedirect(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
					t.Fatal("page handler ran instead of redirecting")
				}), test.httpsAddr)
				request := httptest.NewRequest(method, "http://relay.example:8080/connect?width=1280&height=720", nil)
				response := httptest.NewRecorder()
				handler.ServeHTTP(response, request)

				if response.Code != http.StatusPermanentRedirect {
					t.Fatalf("HTTP status = %d, want %d", response.Code, http.StatusPermanentRedirect)
				}
				if got, want := response.Header().Get("Location"), "https://"+net.JoinHostPort(test.redirectHost, "9443")+"/connect?width=1280&height=720"; got != want {
					t.Fatalf("Location = %q, want %q", got, want)
				}
			})
		}
	}
}

func TestHTTPMachineAPIRequestsAreNotRedirected(t *testing.T) {
	router := gin.New()
	registry := newMachineRegistry()
	registerMachineRoutes(router.Group("/api"), router.Group("/api", passthroughAuth), registry)
	handler := withHTTPSRedirect(router, ":8443")
	body, err := json.Marshal(testRegistration("machine-1"))
	if err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{"/api/machines/register", "/api/machines/machine-1/heartbeat"} {
		request := httptest.NewRequest(http.MethodPost, "http://127.0.0.1:8080"+path, bytes.NewReader(body))
		request.Header.Set("Content-Type", "application/json")
		response := httptest.NewRecorder()
		handler.ServeHTTP(response, request)

		if response.Code != http.StatusOK {
			t.Fatalf("POST %s status = %d, want %d", path, response.Code, http.StatusOK)
		}
		if got := response.Header().Get("Location"); got != "" {
			t.Fatalf("POST %s Location = %q, want no redirect", path, got)
		}
	}
}
