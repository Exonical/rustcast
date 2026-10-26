package main

import (
	"context"
	"crypto/tls"
	"encoding/base64"
	"errors"
	"log"
	"net"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/gin-contrib/cors"
	"github.com/gin-gonic/gin"
	"github.com/quic-go/quic-go"
	"golang.org/x/net/ipv4"
	"golang.org/x/net/ipv6"
)

const (
	webTransportPort = 8443
	dscpAF41         = 34 << 2
)

func webTransportListenAddr() string {
	listenAddr := strings.TrimSpace(os.Getenv("FLUX_WT_ADDR"))
	if listenAddr == "" {
		listenAddr = ":8443"
	}
	return listenAddr
}

func listenWebTransportUDP() (*net.UDPConn, error) {
	listenAddr := webTransportListenAddr()
	udpAddr, err := net.ResolveUDPAddr("udp", listenAddr)
	if err != nil {
		return nil, err
	}
	conn, err := net.ListenUDP("udp", udpAddr)
	if err != nil {
		return nil, err
	}
	if err := ipv4.NewConn(conn).SetTOS(dscpAF41); err != nil {
		log.Printf("[webtransport] DSCP(IPv4) not set: %v", err)
	}
	if err := ipv6.NewConn(conn).SetTrafficClass(dscpAF41); err != nil {
		log.Printf("[webtransport] DSCP(IPv6) not set: %v", err)
	}
	log.Printf("[webtransport] HTTP/3 UDP listener on %s (DSCP AF41)", udpAddr)
	return conn, nil
}

func main() {
	log.SetFlags(log.Ltime | log.Lmicroseconds | log.Lshortfile)
	frameServerAddr := os.Getenv("FLUX_SERVER_ADDR")
	if frameServerAddr == "" {
		frameServerAddr = "127.0.0.1:8556"
	}
	registry := newMachineRegistry()
	sessions := newSessionRegistry()
	serveMetrics(os.Getenv("FLUX_METRICS_ADDR"), registry.metrics)
	registry.seedStatic("default", "Configured Flux Server", frameServerAddr)

	clipboardStore, err := openStore(storeDirFromEnv())
	if err != nil {
		log.Fatalf("open clipboard store: %v", err)
	}
	defer clipboardStore.Close()
	clipboardStore.startRetentionPurge(context.Background(), auditRetentionDays())
	registry.attachDeps(&relayDeps{
		store:    clipboardStore,
		sessions: sessions,
		name:     registry.displayName,
	})

	certificates, err := newRelayCertificateManager(
		os.Getenv("FLUX_TLS_CERT"),
		os.Getenv("FLUX_TLS_KEY"),
		os.Getenv("FLUX_WT_PUBLIC_HOST"),
	)
	if err != nil {
		log.Fatalf("load WebTransport TLS certificate: %v", err)
	}
	if strings.EqualFold(strings.TrimSpace(os.Getenv("FLUX_MODE")), "direct") {
		if err := runDirectMode(registry, sessions, certificates, clipboardStore, frameServerAddr); err != nil {
			log.Fatalf("direct mode: %v", err)
		}
		return
	}
	hostLinkAddr := strings.TrimSpace(os.Getenv("FLUX_HOST_LINK_ADDR"))
	if hostLinkAddr == "" {
		hostLinkAddr = ":8444"
	}
	if !strings.EqualFold(hostLinkAddr, "off") {
		relayToken := os.Getenv("FLUX_RELAY_TOKEN")
		if relayToken == "" {
			log.Printf("[host-link] WARN: FLUX_RELAY_TOKEN unset; host-link listener disabled")
		} else {
			tlsConfig := certificates.tlsConfig().Clone()
			tlsConfig.NextProtos = []string{"flux-frames"}
			listener, err := quic.ListenAddr(hostLinkAddr, tlsConfig, &quic.Config{
				MaxIdleTimeout:  15 * time.Second,
				KeepAlivePeriod: 3 * time.Second,
			})
			if err != nil {
				log.Printf("[host-link] listen on %s failed: %v", hostLinkAddr, err)
			} else {
				log.Printf("[host-link] listening on %s", hostLinkAddr)
				go serveHostLinks(listener, registry, relayToken)
			}
		}
	}
	authService, err := newAuthService(context.Background(), authConfigFromEnv())
	if err != nil {
		log.Fatalf("OIDC provider init: %v", err)
	}
	ticketSigner, err := loadTicketSigner(storeDirFromEnv())
	if err != nil {
		log.Fatalf("load direct ticket signer: %v", err)
	}
	wtServer := newWebTransportServer(registry, sessions, certificates, authService, clipboardStore, nil)
	_, err = startWebTransport(wtServer)
	if err != nil {
		log.Fatalf("listen for WebTransport: %v", err)
	}
	rotationCtx, stopRotation := context.WithCancel(context.Background())
	defer stopRotation()
	go certificates.runRotation(rotationCtx)

	webAddr := ":8080"
	r := newWebHandler(registry, sessions, certificates, os.Getenv("FLUX_WT_PUBLIC_HOST"), authService, clipboardStore, ticketSigner)
	httpHandler := http.Handler(r)
	httpsAddr := strings.TrimSpace(os.Getenv("FLUX_HTTPS_ADDR"))
	if httpsAddr == "" {
		httpsAddr = ":8443"
	}
	if certificates.configured && !strings.EqualFold(httpsAddr, "off") {
		httpsServer := &http.Server{
			Addr:      httpsAddr,
			Handler:   withAltSvc(r),
			TLSConfig: webHTTPSTLSConfig(certificates),
		}
		go func() {
			log.Printf("flux-web listening on https://localhost%s", httpsAddr)
			if err := httpsServer.ListenAndServeTLS("", ""); err != nil && !errors.Is(err, http.ErrServerClosed) {
				log.Printf("HTTPS server error: %v", err)
			}
		}()
		httpHandler = withHTTPSRedirect(r, httpsAddr)
	}
	log.Printf("flux-web listening on http://localhost%s", webAddr)
	if err := http.ListenAndServe(webAddr, httpHandler); err != nil {
		log.Fatalf("server error: %v", err)
	}
}

func newWebHandler(
	registry *machineRegistry,
	sessions *sessionRegistry,
	certificates *relayCertificateManager,
	publicHost string,
	authService *authService,
	clipboardStore *store,
	signer *ticketSigner,
) *gin.Engine {
	gin.SetMode(gin.ReleaseMode)
	r := gin.Default()
	r.Use(cors.Default())
	authService.registerRoutes(r)
	api := r.Group("/api", authService.requireUser())
	admin := r.Group("/api", authService.requireUser(), authService.requireAdmin())
	machineAPI := r.Group("/api", authService.requireMachineToken())
	registerMachineRoutes(api, machineAPI, registry)
	machineAPI.GET("/relay/ticket-key", func(c *gin.Context) {
		if signer == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "ticket signing key unavailable"})
			return
		}
		c.JSON(http.StatusOK, gin.H{
			"alg":        "ML-DSA-87",
			"public_key": base64.StdEncoding.EncodeToString(signer.publicKey()),
		})
	})
	registerSessionRoutes(api, admin, sessions)
	registerPolicyRoutes(admin, registry, authService)
	api.GET("/webtransport", func(c *gin.Context) {
		user := authService.userFrom(c)
		machineID := c.Query("machine")
		if machineID == "" {
			machineID = "default"
		}
		candidates := make([]gin.H, 0)
		if signer != nil {
			if machine, err := registry.get(machineID); err == nil {
				for _, direct := range machine.DirectCandidates {
					ticket, err := signer.sign(user, machineID, machine.ClipboardPolicy)
					if err != nil {
						log.Printf("[ticket] sign direct candidate for %s: %v", machineID, err)
						continue
					}
					candidates = append(candidates, gin.H{
						"path": "direct", "url": direct.URL, "certHashes": direct.CertHashes, "ticket": ticket,
					})
				}
			}
		}
		relayTicket := authService.issueTicket(user)
		candidates = append(candidates, gin.H{
			"path": "relay", "host": publicHost, "port": webTransportPort,
			"certHashes": certificates.certificateHashes(), "ticket": relayTicket,
		})
		c.JSON(http.StatusOK, gin.H{
			"host":       publicHost,
			"port":       webTransportPort,
			"certHashes": certificates.certificateHashes(),
			"ticket":     relayTicket,
			"candidates": candidates,
		})
	})
	api.GET("/me", func(c *gin.Context) {
		user := authService.userFrom(c)
		c.JSON(http.StatusOK, gin.H{
			"sub":          user.Sub,
			"email":        user.Email,
			"name":         user.Name,
			"username":     user.Username,
			"admin":        user.Admin,
			"auth_enabled": authService.enabled,
		})
	})
	r.NoRoute(gin.WrapH(authService.staticGuard(http.FileServer(http.Dir("./ui/out")))))
	return r
}

func withAltSvc(handler http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Alt-Svc", `h3=":8443"`)
		handler.ServeHTTP(w, r)
	})
}

func withHTTPSRedirect(handler http.Handler, httpsAddr string) http.Handler {
	httpsHost, httpsPort, err := net.SplitHostPort(httpsAddr)
	if err != nil || httpsPort == "" {
		return handler
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		isAPIPath := r.URL.Path == "/api" || strings.HasPrefix(r.URL.Path, "/api/")
		if (r.Method == http.MethodGet || r.Method == http.MethodHead) && !isAPIPath {
			host := httpsHost
			if host == "" || host == "0.0.0.0" || host == "::" {
				host = r.Host
				if requestHost, _, err := net.SplitHostPort(host); err == nil {
					host = requestHost
				} else {
					host = strings.Trim(host, "[]")
				}
			}
			location := "https://" + net.JoinHostPort(host, httpsPort) + r.URL.RequestURI()
			http.Redirect(w, r, location, http.StatusPermanentRedirect)
			return
		}
		handler.ServeHTTP(w, r)
	})
}

func webHTTPSTLSConfig(certificates *relayCertificateManager) *tls.Config {
	return &tls.Config{
		MinVersion:       tls.VersionTLS13,
		CurvePreferences: pqCurvePreferences(),
		GetCertificate:   certificates.getCertificate,
	}
}
