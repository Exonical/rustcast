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
	"log"
	"math/big"
	"net"
	"strings"
	"sync/atomic"
	"time"
)

const (
	selfSignedCertificateLifetime = 13 * 24 * time.Hour
	selfSignedCertificateRotation = 6 * 24 * time.Hour
)

type relayCertificateManager struct {
	configured  bool
	publicHost  string
	certificate atomic.Pointer[tls.Certificate]
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
	certificate := m.certificate.Load()
	if certificate == nil || len(certificate.Certificate) == 0 {
		return []string{}
	}
	hash := sha256.Sum256(certificate.Certificate[0])
	return []string{base64.StdEncoding.EncodeToString(hash[:])}
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
	certificate, err := generateRelayCertificate(time.Now(), m.publicHost)
	if err != nil {
		return err
	}
	m.certificate.Store(&certificate)
	return nil
}

func (m *relayCertificateManager) runRotation(ctx context.Context) {
	if m.configured {
		return
	}
	ticker := time.NewTicker(selfSignedCertificateRotation)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			if err := m.rotate(); err != nil {
				log.Printf("[tls] certificate rotation failed: %v", err)
			}
		}
	}
}
