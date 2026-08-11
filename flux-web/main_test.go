package main

import (
	"sync"
	"testing"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/webrtc/v4"
)

func TestSessionMachineAccessIsSynchronized(t *testing.T) {
	session := &Session{}
	machine := &machineUpstream{
		abr: &abrState{commandChan: make(chan []byte, 256)},
	}

	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		for i := 0; i < 1000; i++ {
			session.setMachine(machine)
		}
	}()
	go func() {
		defer wg.Done()
		for i := 0; i < 1000; i++ {
			session.onGCCEstimate(5_000 + uint32(i))
		}
	}()
	wg.Wait()
}

func TestMediaEngineInterceptorsSendTWCCLoopback(t *testing.T) {
	mediaEngine, interceptors, _, err := newMediaEngineAndInterceptors()
	if err != nil {
		t.Fatalf("create media engine and interceptors: %v", err)
	}

	senderAPI := webrtc.NewAPI(
		webrtc.WithMediaEngine(mediaEngine),
		webrtc.WithInterceptorRegistry(interceptors),
	)
	sender, err := senderAPI.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		t.Fatalf("create sender peer connection: %v", err)
	}
	defer sender.Close()

	receiver, err := webrtc.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		t.Fatalf("create receiver peer connection: %v", err)
	}
	defer receiver.Close()

	track, err := webrtc.NewTrackLocalStaticRTP(
		webrtc.RTPCodecCapability{
			MimeType:  webrtc.MimeTypeH264,
			ClockRate: 90000,
		},
		"video",
		"loopback",
	)
	if err != nil {
		t.Fatalf("create loopback track: %v", err)
	}
	if _, err := sender.AddTrack(track); err != nil {
		t.Fatalf("add loopback track: %v", err)
	}

	connected := make(chan struct{})
	sender.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateConnected {
			select {
			case <-connected:
			default:
				close(connected)
			}
		}
	})
	received := make(chan struct{}, 4)
	receiver.OnTrack(func(remote *webrtc.TrackRemote, _ *webrtc.RTPReceiver) {
		for range 4 {
			if _, _, err := remote.ReadRTP(); err != nil {
				return
			}
			received <- struct{}{}
		}
	})

	offer, err := sender.CreateOffer(nil)
	if err != nil {
		t.Fatalf("create offer: %v", err)
	}
	senderGatherComplete := webrtc.GatheringCompletePromise(sender)
	if err := sender.SetLocalDescription(offer); err != nil {
		t.Fatalf("set sender local description: %v", err)
	}
	select {
	case <-senderGatherComplete:
	case <-time.After(5 * time.Second):
		t.Fatal("timed out gathering sender candidates")
	}

	if err := receiver.SetRemoteDescription(*sender.LocalDescription()); err != nil {
		t.Fatalf("set receiver remote description: %v", err)
	}
	answer, err := receiver.CreateAnswer(nil)
	if err != nil {
		t.Fatalf("create answer: %v", err)
	}
	receiverGatherComplete := webrtc.GatheringCompletePromise(receiver)
	if err := receiver.SetLocalDescription(answer); err != nil {
		t.Fatalf("set receiver local description: %v", err)
	}
	select {
	case <-receiverGatherComplete:
	case <-time.After(5 * time.Second):
		t.Fatal("timed out gathering receiver candidates")
	}
	if err := sender.SetRemoteDescription(*receiver.LocalDescription()); err != nil {
		t.Fatalf("set sender remote description: %v", err)
	}

	select {
	case <-connected:
	case <-time.After(5 * time.Second):
		t.Fatal("timed out connecting loopback peer connections")
	}

	for sequence := uint16(0); sequence < 4; sequence++ {
		if err := track.WriteRTP(&rtp.Packet{
			Header: rtp.Header{
				Version:        2,
				PayloadType:    102,
				SequenceNumber: sequence,
				Timestamp:      uint32(sequence) * 3000,
				SSRC:           1,
				Marker:         true,
			},
			Payload: []byte{0x65, 0x01, 0x02},
		}); err != nil {
			t.Fatalf("write RTP packet %d: %v", sequence, err)
		}
	}

	for packet := 0; packet < 4; packet++ {
		select {
		case <-received:
		case <-time.After(5 * time.Second):
			t.Fatalf("timed out receiving RTP packet %d", packet)
		}
	}
}
