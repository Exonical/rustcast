package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"errors"
	"log"
	"math/big"
	"net"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	selfSignedCertificateLifetime = 13 * 24 * time.Hour
	selfSignedCertificateRotation = 6 * 24 * time.Hour
	selfSignedCertificateOverlap  = 10 * time.Minute
)

type relayCertificateManager struct {
	configured      bool
	publicHost      string
	certificate     atomic.Pointer[tls.Certificate]
	mu              sync.Mutex
	next            *tls.Certificate
	nextRotation    time.Time
	onHashesChanged func()
}

func newRelayCertificateManager(certFile, keyFile, publicHost string) (*relayCertificateManager, error) {
	manager := &relayCertificateManager{
		configured: certFile != "" && keyFile != "",
		publicHost: strings.TrimSpace(publicHost),
	}
	var certificate tls.Certificate
	var err error
	if manager.configured {
		certificate, err = tls.LoadX509KeyPair(certFile, keyFile)
		if err != nil {
			return nil, err
		}
		if certificate.Leaf == nil {
			certificate.Leaf, err = x509.ParseCertificate(certificate.Certificate[0])
			if err != nil {
				return nil, err
			}
		}
	} else {
		certificate, err = generateRelayCertificate(time.Now(), manager.publicHost)
		if err != nil {
			return nil, err
		}
		next, nextErr := generateRelayCertificate(time.Now(), manager.publicHost)
		if nextErr != nil {
			return nil, nextErr
		}
		manager.next = &next
		manager.nextRotation = time.Now().Add(selfSignedCertificateRotation)
	}
	manager.certificate.Store(&certificate)
	return manager, nil
}

func generateRelayCertificate(now time.Time, publicHost string) (tls.Certificate, error) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return tls.Certificate{}, err
	}
	serial, err := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	if err != nil {
		return tls.Certificate{}, err
	}
	template := &x509.Certificate{
		SerialNumber:          serial,
		Subject:               pkix.Name{CommonName: "Flux Relay"},
		NotBefore:             now.Add(-time.Minute),
		NotAfter:              now.Add(selfSignedCertificateLifetime),
		KeyUsage:              x509.KeyUsageDigitalSignature,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		BasicConstraintsValid: true,
		DNSNames:              []string{"localhost"},
		IPAddresses:           []net.IP{net.ParseIP("127.0.0.1"), net.ParseIP("::1")},
	}
	if host := strings.TrimSpace(publicHost); host != "" {
		if ip := net.ParseIP(host); ip != nil {
			template.IPAddresses = append(template.IPAddresses, ip)
		} else {
			template.DNSNames = append(template.DNSNames, host)
		}
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		return tls.Certificate{}, err
	}
	certificate := tls.Certificate{
		Certificate: [][]byte{der},
		PrivateKey:  key,
	}
	certificate.Leaf, err = x509.ParseCertificate(der)
	return certificate, err
}

func (m *relayCertificateManager) getCertificate(*tls.ClientHelloInfo) (*tls.Certificate, error) {
	return m.certificate.Load(), nil
}

func (m *relayCertificateManager) certificateHashes() []string {
	if m.configured {
		return []string{}
	}
	m.mu.Lock()
	certificate := m.certificate.Load()
	next := m.next
	overlap := next != nil && time.Until(m.nextRotation) <= selfSignedCertificateOverlap
	m.mu.Unlock()
	if certificate == nil || len(certificate.Certificate) == 0 {
		return []string{}
	}
	hash := sha256.Sum256(certificate.Certificate[0])
	hashes := []string{base64.StdEncoding.EncodeToString(hash[:])}
	if overlap && len(next.Certificate) != 0 {
		nextHash := sha256.Sum256(next.Certificate[0])
		hashes = append(hashes, base64.StdEncoding.EncodeToString(nextHash[:]))
	}
	return hashes
}

func (m *relayCertificateManager) tlsConfig() *tls.Config {
	return &tls.Config{
		MinVersion:       tls.VersionTLS13,
		MaxVersion:       tls.VersionTLS13,
		NextProtos:       []string{"h3"},
		CurvePreferences: pqCurvePreferences(),
		GetCertificate:   m.getCertificate,
	}
}

func pqCurvePreferences() []tls.CurveID {
	return []tls.CurveID{
		tls.MLKEM1024,
		tls.SecP384r1MLKEM1024,
		tls.SecP256r1MLKEM768,
		tls.X25519MLKEM768,
		tls.CurveP384,
		tls.CurveP256,
		tls.X25519,
	}
}

func (m *relayCertificateManager) rotate() error {
	if m.configured {
		return nil
	}
	following, err := generateRelayCertificate(time.Now(), m.publicHost)
	if err != nil {
		return err
	}
	m.mu.Lock()
	next := m.next
	if next == nil {
		m.mu.Unlock()
		return errors.New("next self-signed certificate is unavailable")
	}
	m.certificate.Store(next)
	m.next = &following
	m.nextRotation = time.Now().Add(selfSignedCertificateRotation)
	onHashesChanged := m.onHashesChanged
	m.mu.Unlock()
	if onHashesChanged != nil {
		onHashesChanged()
	}
	return nil
}

func (m *relayCertificateManager) setOnHashesChanged(callback func()) {
	m.mu.Lock()
	m.onHashesChanged = callback
	m.mu.Unlock()
}

func (m *relayCertificateManager) notifyHashesChanged() {
	m.mu.Lock()
	callback := m.onHashesChanged
	m.mu.Unlock()
	if callback != nil {
		callback()
	}
}

func (m *relayCertificateManager) runRotation(ctx context.Context) {
	if m.configured {
		return
	}
	for {
		m.mu.Lock()
		rotationAt := m.nextRotation
		m.mu.Unlock()
		overlapStart := rotationAt.Add(-selfSignedCertificateOverlap)
		if !waitUntil(ctx, overlapStart) {
			return
		}
		m.notifyHashesChanged()
		if !waitUntil(ctx, rotationAt) {
			return
		}
		select {
		case <-ctx.Done():
			return
		default:
		}
		for {
			if err := m.rotate(); err != nil {
				log.Printf("[tls] certificate rotation failed: %v", err)
				timer := time.NewTimer(time.Minute)
				select {
				case <-ctx.Done():
					timer.Stop()
					return
				case <-timer.C:
				}
				continue
			}
			break
		}
	}
}

func waitUntil(ctx context.Context, at time.Time) bool {
	delay := time.Until(at)
	if delay <= 0 {
		return true
	}
	timer := time.NewTimer(delay)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}
