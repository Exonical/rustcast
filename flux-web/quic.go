package main

import (
	"context"
	"crypto/tls"
	"encoding/binary"
	"fmt"
	"io"
	"log"
	"time"

	"github.com/quic-go/quic-go"
)

// connectQUIC connects to flux-server's QUIC frame endpoint (same port as the
// TCP frame server, over UDP). Each H.264 frame arrives on its own
// unidirectional stream — a lost packet only delays that frame, never the ones
// behind it — and upstream commands (IDR requests, input events) go over a
// bidirectional control stream using the same byte protocol as TCP.
// Returns an error if the connection can't be established (caller falls back
// to TCP); once connected it blocks until the connection dies.
func connectQUIC(addr string) error {
	dialCtx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()

	tlsConf := &tls.Config{
		InsecureSkipVerify: true, // flux-server uses a self-signed certificate
		NextProtos:         []string{"flux-frames"},
	}
	conn, err := quic.DialAddr(dialCtx, addr, tlsConf, &quic.Config{
		MaxIdleTimeout:  15 * time.Second,
		KeepAlivePeriod: 3 * time.Second,
	})
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	log.Printf("[frame] connected to quic %s", addr)

	ctx, connCancel := context.WithCancel(context.Background())
	defer connCancel()
	defer conn.CloseWithError(0, "done")

	// Control stream for upstream commands (IDR requests, input events).
	control, err := conn.OpenStreamSync(ctx)
	if err != nil {
		return fmt.Errorf("open control stream: %w", err)
	}
	go func() {
		defer connCancel()
		for {
			select {
			case cmd := <-upstreamCommandChan:
				if _, err := control.Write(cmd); err != nil {
					log.Printf("[frame] quic write command error: %v", err)
					return
				}
			case <-ctx.Done():
				return
			}
		}
	}()

	var frameCount uint64
	for {
		stream, err := conn.AcceptUniStream(ctx)
		if err != nil {
			return fmt.Errorf("accept stream: %w", err)
		}

		// Frame stream: [8-byte BE capture-ts µs][4-byte BE length][H.264 data]
		var hdr [12]byte
		if _, err := io.ReadFull(stream, hdr[:]); err != nil {
			log.Printf("[frame] quic read header error: %v", err)
			continue
		}
		tsMicros := binary.BigEndian.Uint64(hdr[0:8])
		frameLen := binary.BigEndian.Uint32(hdr[8:12])
		if frameLen == 0 || frameLen > 10*1024*1024 {
			log.Printf("[frame] quic invalid frame length: %d", frameLen)
			continue
		}
		data := make([]byte, frameLen)
		if _, err := io.ReadFull(stream, data); err != nil {
			log.Printf("[frame] quic read frame error: %v", err)
			continue
		}

		frameCount++
		if frameCount%300 == 0 {
			log.Printf("[frame] received %d frames via quic (last=%d bytes)", frameCount, frameLen)
		}

		msg := frameMsg{tsMicros: tsMicros, data: data}
		select {
		case frameChan <- msg:
		default:
			select {
			case <-frameChan:
			default:
			}
			frameChan <- msg
		}
	}
}
