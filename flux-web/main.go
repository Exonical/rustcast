package main

import (
	"context"
	"errors"
	"log"
	"net"
	"net/http"
	"os"

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
	gin.SetMode(gin.ReleaseMode)
	r := gin.Default()
	r.Use(cors.Default())
	registerMachineRoutes(r, registry)
	registerSessionRoutes(r, sessions)
	r.GET("/api/webtransport", func(c *gin.Context) {
		c.JSON(http.StatusOK, gin.H{
			"host":       os.Getenv("FLUX_WT_PUBLIC_HOST"),
			"port":       webTransportPort,
			"certHashes": certificates.certificateHashes(),
		})
	})
	r.NoRoute(gin.WrapH(http.FileServer(http.Dir("./ui/out"))))
	log.Printf("flux-web listening on http://localhost%s", webAddr)
	if err := r.Run(webAddr); err != nil {
		log.Fatalf("server error: %v", err)
	}
}
