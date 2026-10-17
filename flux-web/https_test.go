package main

import (
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
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

	handler := newWebHandler(newMachineRegistry(), newSessionRegistry(), certificates, "localhost")
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
