package main

import (
	"context"
	"crypto/tls"
	"fmt"
	"testing"
	"time"

	"github.com/quic-go/quic-go"
)

func TestPQCurvePreferencesOrder(t *testing.T) {
	want := []tls.CurveID{
		tls.MLKEM1024,
		tls.SecP384r1MLKEM1024,
		tls.SecP256r1MLKEM768,
		tls.X25519MLKEM768,
		tls.CurveP384,
		tls.CurveP256,
		tls.X25519,
	}
	got := pqCurvePreferences()
	if len(got) != len(want) {
		t.Fatalf("curve preference count = %d, want %d", len(got), len(want))
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("curve preference %d = %v, want %v", i, got[i], want[i])
		}
	}
}

func TestHostLinkTLSAcceptsPQCurveGroups(t *testing.T) {
	for _, curve := range []tls.CurveID{tls.X25519MLKEM768, tls.MLKEM1024} {
		t.Run(fmt.Sprintf("%d", curve), func(t *testing.T) {
			certificates, err := newRelayCertificateManager("", "", "localhost")
			if err != nil {
				t.Fatal(err)
			}
			serverTLS := certificates.tlsConfig().Clone()
			serverTLS.NextProtos = []string{"flux-frames"}
			listener, err := quic.ListenAddr("127.0.0.1:0", serverTLS, &quic.Config{
				MaxIdleTimeout: 15 * time.Second, KeepAlivePeriod: 3 * time.Second,
			})
			if err != nil {
				t.Fatal(err)
			}
			defer listener.Close()
			accepted := make(chan error, 1)
			go func() {
				conn, err := listener.Accept(context.Background())
				if err == nil {
					defer conn.CloseWithError(0, "test complete")
				}
				accepted <- err
			}()
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			conn, err := quic.DialAddr(ctx, listener.Addr().String(), &tls.Config{
				InsecureSkipVerify: true,
				NextProtos:         []string{"flux-frames"},
				CurvePreferences:   []tls.CurveID{curve},
			}, &quic.Config{MaxIdleTimeout: 15 * time.Second, KeepAlivePeriod: 3 * time.Second})
			if err != nil {
				t.Fatalf("QUIC handshake using %v failed: %v", curve, err)
			}
			defer conn.CloseWithError(0, "test complete")
			if err := <-accepted; err != nil {
				t.Fatal(err)
			}
			if got := conn.ConnectionState().TLS.CurveID; got != curve {
				t.Fatalf("negotiated curve = %v, want %v", got, curve)
			}
		})
	}
}
