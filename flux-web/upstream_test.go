package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"reflect"
	"sync"
	"testing"
	"time"

	"github.com/pion/rtp"
	"github.com/pion/rtp/codecs"
	"github.com/pion/webrtc/v4"
)

func pooledFrame(pool *frameBufferPool, frame frameMsg) frameMsg {
	data := pool.get(len(frame.data))
	copy(data, frame.data)
	frame.data, frame.buffers = data, pool
	return frame
}

func cachedFrameBuffers(pool *frameBufferPool) int {
	pool.mu.Lock()
	defer pool.mu.Unlock()
	return pool.count
}

func TestFrameBufferPoolReuseAndBound(t *testing.T) {
	var pool frameBufferPool
	frame := pooledFrame(&pool, pFrame(1))
	original := &frame.data[0]
	frame.release()
	frame.release()
	if pool.count != 1 {
		t.Fatalf("release cached %d buffers, want exactly 1", pool.count)
	}
	smaller := pool.get(3)
	if len(smaller) != 3 || &smaller[0] != original {
		t.Fatal("smaller frame did not reuse the same backing array")
	}
	other := pool.get(3)
	if &other[0] == original {
		t.Fatal("in-flight frames shared a buffer")
	}
	pool.put(smaller)
	pool.put(other)
	large := pool.get(1024)
	pool.put(large)
	if got := pool.get(1024); &got[0] != &large[0] {
		t.Fatal("larger buffer was not reused after growing")
	}
	for i := 0; i < maxCachedFrameBuffers+2; i++ {
		pool.put(make([]byte, 5))
	}
	if pool.count != maxCachedFrameBuffers {
		t.Fatalf("cache size = %d, want %d", pool.count, maxCachedFrameBuffers)
	}
}

func TestLatestFrameReleasesOnlyDiscardedPayloads(t *testing.T) {
	tests := []struct {
		name    string
		current frameMsg
		queued  []frameMsg
		wantTs  uint64
	}{
		{"replace P-frame", pFrame(1), []frameMsg{pFrame(2)}, 2},
		{"preserve IDR", idrFrame(1), []frameMsg{pFrame(2), pFrame(3)}, 1},
		{"replace IDR", idrFrame(1), []frameMsg{pFrame(2), idrFrame(3), pFrame(4)}, 3},
		{"no backlog", pFrame(1), nil, 1},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			var pool frameBufferPool
			current := pooledFrame(&pool, test.current)
			ch := make(chan frameMsg, len(test.queued))
			for _, frame := range test.queued {
				ch <- pooledFrame(&pool, frame)
			}
			latest, _ := latestFrame(ch, current)
			if latest.tsMicros != test.wantTs || len(latest.data) == 0 {
				t.Fatalf("selected frame = %+v, want ts=%d", latest, test.wantTs)
			}
			if pool.count != len(test.queued) {
				t.Fatalf("returned %d buffers, want %d discarded buffers", pool.count, len(test.queued))
			}
			for i := 0; i < pool.count; i++ {
				if &pool.buffers[i][:1][0] == &latest.data[0] {
					t.Fatal("selected buffer was returned before consumption")
				}
				for j := 0; j < i; j++ {
					if &pool.buffers[i][:1][0] == &pool.buffers[j][:1][0] {
						t.Fatal("discarded buffer returned twice")
					}
				}
			}
			latest.release()
			if pool.count != len(test.queued)+1 {
				t.Fatal("selected buffer was not returned after consumption")
			}
		})
	}
}

func TestReadPayloadReturnsTruncatedVideoBuffer(t *testing.T) {
	u := newMachineUpstream("", "", func(string) {})
	frame, err := u.readPayload(bytes.NewReader([]byte{1, 2}), 0x01, 5)
	if !errors.Is(err, io.ErrUnexpectedEOF) || frame.data != nil || u.frameBuffers.count != 1 {
		t.Fatalf("readPayload = (%+v, %v), cached=%d", frame, err, u.frameBuffers.count)
	}
	for _, messageType := range []byte{0x02, 0x03, 0xff} {
		frame, err := u.readPayload(bytes.NewReader([]byte("{}")), messageType, 2)
		if err != nil || frame.buffers != nil || string(frame.data) != "{}" {
			t.Fatalf("metadata payload pooled or changed: (%+v, %v)", frame, err)
		}
		frame.release()
	}
	if u.frameBuffers.count != 1 {
		t.Fatal("non-video metadata entered the video buffer pool")
	}
}

func TestReadFramesPoolsVideoButRetainsCursor(t *testing.T) {
	u := newMachineUpstream("", "", func(string) {})
	reader, writer := net.Pipe()
	defer reader.Close()
	go func() {
		defer writer.Close()
		for _, message := range []struct {
			kind byte
			data []byte
		}{{0x02, []byte(`{"visible":true}`)}, {0x01, pFrame(1).data}} {
			var header [13]byte
			header[0] = message.kind
			binary.BigEndian.PutUint64(header[1:9], 42)
			binary.BigEndian.PutUint32(header[9:13], uint32(len(message.data)))
			if _, err := writer.Write(append(header[:], message.data...)); err != nil {
				return
			}
		}
	}()
	if err := u.readFrames(reader); !errors.Is(err, io.EOF) {
		t.Fatalf("readFrames = %v, want EOF", err)
	}
	frame := <-u.frameChan
	if frame.buffers != &u.frameBuffers || frame.tsMicros != 42 || !bytes.Equal(frame.data, pFrame(1).data) {
		t.Fatalf("video payload or ownership changed: %+v", frame)
	}
	frame.release()
	reused := u.frameBuffers.get(5)
	clear(reused)
	u.frameBuffers.put(reused)
	if string(u.lastCursor.data) != `{"visible":true}` {
		t.Fatal("retained cursor data was overwritten by buffer reuse")
	}
}

func TestQueueFrameReleasesEvictedAndStoppedFrames(t *testing.T) {
	u := newMachineUpstream("", "", func(string) {})
	u.frameChan = make(chan frameMsg, 1)
	first := pooledFrame(&u.frameBuffers, pFrame(1))
	second := pooledFrame(&u.frameBuffers, pFrame(2))
	third := pooledFrame(&u.frameBuffers, pFrame(3))
	if u.queueFrame(first) || !u.queueFrame(second) {
		t.Fatal("queue did not report eviction")
	}
	if u.frameBuffers.count != 1 {
		t.Fatal("evicted buffer was not returned exactly once")
	}
	u.stop()
	u.queueFrame(third)
	if len(u.frameChan) != 0 || u.frameBuffers.count != 3 {
		t.Fatalf("shutdown retained frames: queue=%d cached=%d", len(u.frameChan), u.frameBuffers.count)
	}
}

func TestConcurrentQueueAndStop(t *testing.T) {
	u := newMachineUpstream("", "", func(string) {})
	u.frameChan = make(chan frameMsg, 1)
	var producers sync.WaitGroup
	started := make(chan struct{}, 2)
	for i := 0; i < 2; i++ {
		producers.Add(1)
		go func() {
			defer producers.Done()
			u.queueFrame(pooledFrame(&u.frameBuffers, pFrame(0)))
			started <- struct{}{}
			for j := 0; j < 1000; j++ {
				u.queueFrame(pooledFrame(&u.frameBuffers, pFrame(uint64(j))))
			}
		}()
	}
	<-started
	<-started
	u.stop()
	done := make(chan struct{})
	go func() { producers.Wait(); close(done) }()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("concurrent reader/requeue producers blocked after shutdown")
	}
	if len(u.frameChan) != 0 {
		t.Fatal("producer queued a frame after shutdown drain")
	}
}

func TestPacingRequeueTransfersBufferOwnership(t *testing.T) {
	u := newMachineUpstream("", "", func(string) {})
	u.frameChan = make(chan frameMsg, 1)
	track, err := webrtc.NewTrackLocalStaticRTP(webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeH264}, "video", "test")
	if err != nil {
		t.Fatal(err)
	}
	session := &Session{VideoTrack: track}
	u.queueFrame(pooledFrame(&u.frameBuffers, pFrame(1)))
	next, sent := u.writePacedPackets(session, []*rtp.Packet{{}}, []time.Duration{time.Hour}, false)
	if next == nil || sent != 0 || u.frameBuffers.count != 0 {
		t.Fatalf("pacing released the selected frame: next=%+v sent=%d cached=%d", next, sent, u.frameBuffers.count)
	}
	u.queueFrame(pooledFrame(&u.frameBuffers, pFrame(2)))
	if !u.queueFrame(*next) || u.frameBuffers.count != 1 {
		t.Fatal("requeue did not return the evicted buffer exactly once")
	}
	selected := <-u.frameChan
	if selected.tsMicros != 1 || u.frameBuffers.count != 1 {
		t.Fatal("requeued frame was released before consumption")
	}
	selected.release()
	if u.frameBuffers.count != 2 {
		t.Fatal("consumed requeued frame was not returned")
	}
}

func TestFramePusherReleasesConsumedAndSkippedFrames(t *testing.T) {
	for _, scenario := range []string{"no session", "no video track", "awaiting IDR", "packetized"} {
		t.Run(scenario, func(t *testing.T) {
			u := newMachineUpstream("", "", func(string) {})
			if scenario != "no session" {
				session := &Session{needsIDR: scenario == "awaiting IDR"}
				if scenario != "no video track" {
					track, err := webrtc.NewTrackLocalStaticRTP(webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeH264}, "video", "test")
					if err != nil {
						t.Fatal(err)
					}
					session.VideoTrack = track
					session.Packetizer = rtp.NewPacketizer(1200, 96, 1, &framePayloader{}, rtp.NewFixedSequencer(0), 90000)
				}
				u.bindSession(session)
			}
			u.queueFrame(pooledFrame(&u.frameBuffers, pFrame(1)))
			done := make(chan struct{})
			go func() { u.framePusher(); close(done) }()
			defer func() { u.stop(); <-done }()
			deadline := time.NewTimer(time.Second)
			defer deadline.Stop()
			ticker := time.NewTicker(time.Millisecond)
			defer ticker.Stop()
			for cachedFrameBuffers(&u.frameBuffers) != 1 {
				select {
				case <-deadline.C:
					t.Fatal("pusher did not release frame")
				case <-ticker.C:
				}
			}
		})
	}
}

func TestFramePayloaderMatchesPion(t *testing.T) {
	for _, mtu := range []uint16{0, 2, 16, 1200} {
		var safe framePayloader
		var original codecs.H264Payloader
		for _, data := range [][]byte{
			nil, {0x41, 1, 2}, {0, 0, 1},
			{0, 0, 0, 1, 0x67, 3, 0, 0, 1, 0x68, 4, 0, 0, 0, 1, 0x65, 5},
			{0, 0, 1, 0x67, 8}, {0x68, 9}, {0, 0, 1, 0x09},
			append([]byte{0, 0, 1, 0x65}, bytes.Repeat([]byte{3}, 4096)...),
			{0, 0, 1, 0, 0, 0, 1, 0x41, 4},
		} {
			got, want := safe.Payload(mtu, data), original.Payload(mtu, data)
			if !reflect.DeepEqual(got, want) {
				t.Fatalf("mtu=%d data=%x: got %x, want %x", mtu, data, got, want)
			}
		}
	}
}

func TestPacketizerDoesNotRetainPooledPayload(t *testing.T) {
	var pool frameBufferPool
	packetizer := rtp.NewPacketizer(1200, 96, 1, &framePayloader{}, rtp.NewFixedSequencer(0), 90000)
	var reference codecs.H264Payloader
	// Parameter sets may arrive separately and survive multiple Packetize calls.
	for _, data := range [][]byte{{0, 0, 1, 0x67, 1}, {0, 0, 1, 0x68, 2}, {0, 0, 1, 0x65, 3}} {
		frame := pooledFrame(&pool, frameMsg{data: data})
		packets := packetizer.Packetize(frame.data, 1440)
		want := reference.Payload(1188, data)
		frame.release()
		reused := pool.get(len(data))
		clear(reused)
		pool.put(reused)
		if len(packets) != len(want) {
			t.Fatalf("packets=%d, want %d", len(packets), len(want))
		}
		for i, packet := range packets {
			if !bytes.Equal(packet.Payload, want[i]) {
				t.Fatalf("packet payload corrupted after reuse: got %x, want %x", packet.Payload, want[i])
			}
		}
	}
}

func FuzzFramePayloaderMatchesPion(f *testing.F) {
	f.Add([]byte{0, 0, 0, 1, 0x67, 3, 0, 0, 1, 0x68, 4, 0, 0, 0, 1, 0x65, 5}, uint16(1200))
	f.Add([]byte{0x41, 1, 2}, uint16(16))
	f.Fuzz(func(t *testing.T, data []byte, mtu uint16) {
		var safe framePayloader
		var original codecs.H264Payloader
		for _, part := range [][]byte{data[:len(data)/2], data[len(data)/2:]} {
			got, want := safe.Payload(mtu, part), original.Payload(mtu, part)
			if !reflect.DeepEqual(got, want) {
				t.Fatalf("mtu=%d data=%x: got %x, want %x", mtu, part, got, want)
			}
		}
	})
}

var benchmarkFrameData []byte

func BenchmarkFramePayloadBuffer(b *testing.B) {
	for _, size := range []int{256 * 1024, 1024 * 1024, maxFramePayloadSize} {
		b.Run(stringSize(size), func(b *testing.B) {
			b.Run("allocate", func(b *testing.B) {
				b.ReportAllocs()
				for i := 0; i < b.N; i++ {
					benchmarkFrameData = make([]byte, size)
				}
			})
			b.Run("reuse", func(b *testing.B) {
				var pool frameBufferPool
				pool.put(pool.get(size))
				b.ReportAllocs()
				b.ResetTimer()
				for i := 0; i < b.N; i++ {
					benchmarkFrameData = pool.get(size)
					pool.put(benchmarkFrameData)
				}
			})
		})
	}
}

func BenchmarkFrameReadAndPacketize(b *testing.B) {
	data := bytes.Repeat([]byte{3}, 1024*1024)
	copy(data, []byte{0, 0, 0, 1, 0x65})
	for _, reuse := range []bool{false, true} {
		name := "allocate"
		var payloader rtp.Payloader = &codecs.H264Payloader{}
		if reuse {
			name = "reuse"
			payloader = &framePayloader{}
		}
		b.Run(name, func(b *testing.B) {
			u := newMachineUpstream("", "", func(string) {})
			packetizer := rtp.NewPacketizer(1200, 96, 1, payloader, rtp.NewFixedSequencer(0), 90000)
			reader := bytes.NewReader(data)
			if reuse {
				u.frameBuffers.put(u.frameBuffers.get(len(data)))
			}
			b.ReportAllocs()
			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				reader.Reset(data)
				var frame frameMsg
				if reuse {
					var err error
					frame, err = u.readPayload(reader, 0x01, len(data))
					if err != nil {
						b.Fatal(err)
					}
				} else {
					frame.data = make([]byte, len(data))
					if _, err := io.ReadFull(reader, frame.data); err != nil {
						b.Fatal(err)
					}
				}
				packets := packetizer.Packetize(frame.data, 1440)
				if len(packets) == 0 {
					b.Fatal("no packets")
				}
				benchmarkFrameData = packets[0].Payload
				frame.release()
			}
		})
	}
}

func stringSize(size int) string {
	switch size {
	case 256 * 1024:
		return "256KiB"
	case 1024 * 1024:
		return "1MiB"
	default:
		return "10MiB"
	}
}

func TestCaptureFrameDuration(t *testing.T) {
	tests := []struct {
		name       string
		lastTs     uint64
		currentTs  uint64
		haveLastTs bool
		want       time.Duration
	}{
		{
			name:       "normal frame interval",
			lastTs:     1_000_000,
			currentTs:  1_016_000,
			haveLastTs: true,
			want:       16 * time.Millisecond,
		},
		{
			name:       "multi-second idle gap is preserved",
			lastTs:     1_000_000,
			currentTs:  4_000_000,
			haveLastTs: true,
			want:       3 * time.Second,
		},
		{
			name:       "backwards timestamp uses nominal duration",
			lastTs:     4_000_000,
			currentTs:  3_000_000,
			haveLastTs: true,
			want:       defaultFrameDuration,
		},
		{
			name:       "timestamp reset uses nominal duration",
			lastTs:     9_000_000,
			currentTs:  0,
			haveLastTs: true,
			want:       defaultFrameDuration,
		},
		{
			name:       "first sample uses nominal duration",
			lastTs:     0,
			currentTs:  2_000_000,
			haveLastTs: false,
			want:       defaultFrameDuration,
		},
		{
			name:       "absurd timestamp gap is capped",
			lastTs:     1,
			currentTs:  31_000_000,
			haveLastTs: true,
			want:       maxSaneFrameDuration,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := captureFrameDuration(test.lastTs, test.currentTs, test.haveLastTs); got != test.want {
				t.Fatalf("captureFrameDuration(%d, %d, %t) = %s, want %s",
					test.lastTs, test.currentTs, test.haveLastTs, got, test.want)
			}
		})
	}
}

func TestPacingScheduleUsesTwiceTargetBitrate(t *testing.T) {
	schedule := pacingSchedule([]int{1000, 1000, 500}, 1000, 16*time.Millisecond)
	want := []time.Duration{0, 4 * time.Millisecond, 8 * time.Millisecond}
	for i := range want {
		if schedule[i] != want[i] {
			t.Fatalf("schedule[%d] = %s, want %s", i, schedule[i], want[i])
		}
	}
}

func TestPacingScheduleUsesFrameRateFloor(t *testing.T) {
	schedule := pacingSchedule([]int{1200, 800}, 0, 16*time.Millisecond)
	if schedule[0] != 0 || schedule[1] != 9600*time.Microsecond {
		t.Fatalf("frame-rate floor schedule = %v, want [0 9.6ms]", schedule)
	}
}

func TestPacingScheduleLargeFrameIsBoundedByTargetMultiple(t *testing.T) {
	packetSizes := []int{10_000, 10_000}
	schedule := pacingSchedule(packetSizes, 100, 16*time.Millisecond)
	if got, want := schedule[len(schedule)-1], 200*time.Millisecond; got != want {
		t.Fatalf("large frame's final packet starts at %s, want %s", got, want)
	}
}

func TestPacingScheduleIDRUsesEmissionTargetAfterLongGap(t *testing.T) {
	schedule := pacingScheduleForFrame([]int{50_000, 30_000}, 2_100, time.Second, true)
	if got, want := schedule[1], 40*time.Millisecond; got != want {
		t.Fatalf("IDR final packet starts at %s, want %s", got, want)
	}
	if got, want := schedule[1]+30_000*8*time.Second/time.Duration(pacingIDRMinCeiling), 64*time.Millisecond; got != want {
		t.Fatalf("IDR emission time = %s, want %s", got, want)
	}
}

func TestPacingScheduleIDRCeilingScalesWithTarget(t *testing.T) {
	schedule := pacingScheduleForFrame([]int{200_000, 200_000}, 12_000, 16*time.Millisecond, true)
	bitsPerSecond := float64(48_000_000)
	if got, want := schedule[1], time.Duration(float64(200_000*8)/bitsPerSecond*float64(time.Second)); got != want {
		t.Fatalf("IDR final packet starts at %s, want %s", got, want)
	}
}

func TestPacingScheduleIDRRespectsAbsoluteRateCeiling(t *testing.T) {
	schedule := pacingScheduleForFrame([]int{1_000_000, 1}, 1_000, time.Second, true)
	if got, want := schedule[1], 800*time.Millisecond; got != want {
		t.Fatalf("large IDR final packet starts at %s, want %s", got, want)
	}
}

func TestLatestFrameReportsDiscardedFrames(t *testing.T) {
	tests := []struct {
		name     string
		current  frameMsg
		queued   []frameMsg
		wantTs   uint64
		wantIDR  bool
		wantDrop bool
	}{
		{
			name:     "keyframe outranks newer P-frame",
			current:  pFrame(1),
			queued:   []frameMsg{idrFrame(2), pFrame(3)},
			wantTs:   2,
			wantIDR:  true,
			wantDrop: true,
		},
		{
			name:     "newest keyframe wins among multiple keyframes",
			current:  pFrame(1),
			queued:   []frameMsg{idrFrame(2), pFrame(3), idrFrame(4), pFrame(5)},
			wantTs:   4,
			wantIDR:  true,
			wantDrop: true,
		},
		{
			name:     "newest P-frame wins without a keyframe",
			current:  pFrame(1),
			queued:   []frameMsg{pFrame(2), pFrame(3)},
			wantTs:   3,
			wantIDR:  false,
			wantDrop: true,
		},
		{
			name:     "empty queue keeps current keyframe",
			current:  idrFrame(1),
			wantTs:   1,
			wantIDR:  true,
			wantDrop: false,
		},
		{
			name:     "empty queue keeps current P-frame",
			current:  pFrame(1),
			wantTs:   1,
			wantIDR:  false,
			wantDrop: false,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			ch := make(chan frameMsg, len(test.queued))
			for _, frame := range test.queued {
				ch <- frame
			}
			latest, idr, dropped := latestFrame(ch, test.current)
			if latest.tsMicros != test.wantTs || idr != test.wantIDR || dropped != test.wantDrop {
				t.Fatalf("latestFrame() = (%+v, %t, %t), want ts=%d idr=%t dropped=%t",
					latest, idr, dropped, test.wantTs, test.wantIDR, test.wantDrop)
			}
			if idr != isIDRFrame(latest.data) {
				t.Fatalf("latestFrame() idr=%t disagrees with isIDRFrame()=%t", idr, isIDRFrame(latest.data))
			}
		})
	}
}

func TestGatedSkipRequestsRecoveryIDRThroughGate(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })
	session := &Session{needsIDR: true}

	if !u.skipFrameUntilIDR(session, false) {
		t.Fatal("gated P-frame was not skipped")
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("first gated skip queued %d IDR requests, want 1", got)
	}
	if !u.skipFrameUntilIDR(session, false) {
		t.Fatal("second gated P-frame was not skipped")
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("same-window gated skip queued %d IDR requests, want 1", got)
	}

	now = now.Add(idrRequestInterval)
	if !u.skipFrameUntilIDR(session, false) {
		t.Fatal("next-window gated P-frame was not skipped")
	}
	if got := len(u.commandChan); got != 2 {
		t.Fatalf("next-window gated skip queued %d IDR requests, want 2 total", got)
	}
}

func TestGatedSkipDoesNotSkipIDR(t *testing.T) {
	u := newMachineUpstream("", "", nil)
	session := &Session{needsIDR: true}
	if u.skipFrameUntilIDR(session, true) {
		t.Fatal("IDR was incorrectly skipped")
	}
	if got := len(u.commandChan); got != 0 {
		t.Fatalf("IDR path queued %d recovery requests, want 0", got)
	}
}

func pFrame(ts uint64) frameMsg {
	return frameMsg{tsMicros: ts, data: []byte{0, 0, 0, 1, 0x41}}
}

func idrFrame(ts uint64) frameMsg {
	return frameMsg{tsMicros: ts, data: []byte{0, 0, 0, 1, 0x65}}
}

func TestIDRRequestGateBoundsBurstAcrossDropPaths(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })

	for range 6 {
		u.requestIDR(idrReasonStaleQueue)
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("burst queued %d IDR requests, want exactly 1", got)
	}
	if got := <-u.commandChan; len(got) != 1 || got[0] != 0x01 {
		t.Fatalf("burst queued command %#v, want IDR command", got)
	}

	now = now.Add(idrRequestInterval)
	if !u.requestIDR(idrReasonStaleQueue) {
		t.Fatal("request at the next gate window was rejected")
	}
	if got := len(u.commandChan); got != 1 {
		t.Fatalf("second gate window queued %d IDR requests, want 1", got)
	}
	if got := <-u.commandChan; len(got) != 1 || got[0] != 0x01 {
		t.Fatalf("next-window queued command %#v, want IDR command", got)
	}
}

func TestInitialIDRRequestBypassesGateOncePerSession(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })
	session := &Session{}

	if !u.requestIDR(idrReasonViewerPLI) {
		t.Fatal("regular IDR request was rejected")
	}
	if !u.requestInitialIDR(session) {
		t.Fatal("initial session IDR request was delayed by the shared gate")
	}
	if u.requestInitialIDR(session) {
		t.Fatal("initial-session exemption was reused")
	}
	if got := len(u.commandChan); got != 2 {
		t.Fatalf("queued %d IDR requests, want regular plus one initial request", got)
	}
}

func TestEmissionSequenceNumbersRemainContiguousAcrossAbandonment(t *testing.T) {
	next := uint16(65534)
	var emitted []uint16
	for range 2 {
		packet := &rtp.Packet{}
		assignSequenceNumber(packet, &next)
		emitted = append(emitted, packet.Header.SequenceNumber)
		commitSequenceNumber(&next, true)
	}
	// The remainder of this frame is abandoned. The next frame starts with
	// the next emitted sequence number rather than the packetizer's counter.
	for range 3 {
		packet := &rtp.Packet{}
		assignSequenceNumber(packet, &next)
		emitted = append(emitted, packet.Header.SequenceNumber)
		commitSequenceNumber(&next, true)
	}
	want := []uint16{65534, 65535, 0, 1, 2}
	for i := range want {
		if emitted[i] != want[i] {
			t.Fatalf("emitted sequence[%d] = %d, want %d", i, emitted[i], want[i])
		}
	}
}

func TestSequenceNumberDoesNotAdvanceOnWriteError(t *testing.T) {
	next := uint16(41)
	packet := &rtp.Packet{}
	assignSequenceNumber(packet, &next)
	if packet.Header.SequenceNumber != next {
		t.Fatalf("assigned sequence = %d, want %d", packet.Header.SequenceNumber, next)
	}
	if next != 41 {
		t.Fatalf("sequence advanced before write: %d", next)
	}

	commitSequenceNumber(&next, false)
	if next != 41 {
		t.Fatalf("failed write sequence = %d, want 41", next)
	}
	commitSequenceNumber(&next, true)
	if next != 42 {
		t.Fatalf("successful write sequence = %d, want 42", next)
	}
}

func TestAbandonedFrameDoesNotOveradvanceRTPClock(t *testing.T) {
	firstTicks, remainder := consumeRTPDuration(16*time.Millisecond, 0)
	secondTicks, remainder := consumeRTPDuration(16*time.Millisecond, remainder)
	combinedTicks, combinedRemainder := consumeRTPDuration(32*time.Millisecond, 0)
	if firstTicks+secondTicks != combinedTicks || remainder != combinedRemainder {
		t.Fatalf(
			"abandoned-frame timeline = (%d, %v), direct timeline = (%d, %v)",
			firstTicks+secondTicks,
			remainder,
			combinedTicks,
			combinedRemainder,
		)
	}
}

func TestIDRStatsSeparatesGrantedFromSuppressedByReason(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })

	u.requestIDR(idrReasonViewerPLI)  // granted
	u.requestIDR(idrReasonAbandoned)  // gated
	u.requestIDR(idrReasonStaleQueue) // gated
	u.requestIDR(idrReasonStaleQueue) // gated

	summary := u.idrStats.drain()
	want := "viewer-pli=1(+0 suppressed) stale-queue-discard=0(+2 suppressed) abandoned-packets=0(+1 suppressed)"
	if summary != want {
		t.Fatalf("summary = %q, want %q", summary, want)
	}
	if again := u.idrStats.drain(); again != "" {
		t.Fatalf("counts survived a drain: %q", again)
	}
}

func TestStageStatsReportsWorstCaseSeparatelyFromAverage(t *testing.T) {
	var stats stageStats
	stats.observe(1, 2*time.Millisecond, 4*time.Millisecond, false, 10_000)
	stats.observe(9, 20*time.Millisecond, 40*time.Millisecond, true, 700_000)

	summary := stats.summary()
	want := "frames=2 queue avg=5.0 max=9 | relay wait avg=11.0ms max=20.0ms | " +
		"pacing avg=22.0ms max=40.0ms | idr n=1 max=700000 bytes max pacing=40.0ms"
	if summary != want {
		t.Fatalf("summary = %q, want %q", summary, want)
	}
	if empty := (&stageStats{}).summary(); empty != "" {
		t.Fatalf("idle window summary = %q, want empty", empty)
	}
}
