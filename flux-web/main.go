package main

import (
	"context"
	"crypto/tls"
	"errors"
	"log"
	"net"
	"net/http"
	"os"
	"strings"

	"github.com/gin-contrib/cors"
	"github.com/gin-gonic/gin"
	"golang.org/x/net/ipv4"
	"golang.org/x/net/ipv6"
)

const (
	webTransportPort = 8443
	dscpAF41         = 34 << 2
)

func listenWebTransportUDP() (*net.UDPConn, error) {
	conn, err := net.ListenUDP("udp", &net.UDPAddr{Port: webTransportPort})
	if err != nil {
		return nil, err
	}
	if err := ipv4.NewConn(conn).SetTOS(dscpAF41); err != nil {
		log.Printf("[webtransport] DSCP(IPv4) not set: %v", err)
	}
	if err := ipv6.NewConn(conn).SetTrafficClass(dscpAF41); err != nil {
		log.Printf("[webtransport] DSCP(IPv6) not set: %v", err)
	}
	log.Printf("[webtransport] HTTP/3 UDP listener on :%d (DSCP AF41)", webTransportPort)
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

	certificates, err := newRelayCertificateManager(
		os.Getenv("FLUX_TLS_CERT"),
		os.Getenv("FLUX_TLS_KEY"),
		os.Getenv("FLUX_WT_PUBLIC_HOST"),
	)
	if err != nil {
		log.Fatalf("load WebTransport TLS certificate: %v", err)
	}
	wtServer := newWebTransportServer(registry, sessions, certificates)
	udpConn, err := listenWebTransportUDP()
	if err != nil {
		log.Fatalf("listen for WebTransport: %v", err)
	}
	go func() {
		if err := wtServer.Serve(udpConn); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Printf("[webtransport] HTTP/3 server stopped: %v", err)
		}
	}()
	rotationCtx, stopRotation := context.WithCancel(context.Background())
	defer stopRotation()
	go certificates.runRotation(rotationCtx)

	webAddr := ":8080"
	r := newWebHandler(registry, sessions, certificates, os.Getenv("FLUX_WT_PUBLIC_HOST"))
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
) *gin.Engine {
	gin.SetMode(gin.ReleaseMode)
	r := gin.Default()
	r.Use(cors.Default())
	registerMachineRoutes(r, registry)
	registerSessionRoutes(r, sessions)
	r.GET("/api/webtransport", func(c *gin.Context) {
		c.JSON(http.StatusOK, gin.H{
			"host":       publicHost,
			"port":       webTransportPort,
			"certHashes": certificates.certificateHashes(),
		})
	})
	r.NoRoute(gin.WrapH(http.FileServer(http.Dir("./ui/out"))))
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
		MinVersion:     tls.VersionTLS13,
		GetCertificate: certificates.getCertificate,
	}
}
