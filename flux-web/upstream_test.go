package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"sync"
	"testing"
	"time"

	"github.com/quic-go/quic-go"
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
			latest, _, _ := latestFrame(ch, current)
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
	u := newMachineUpstream("", "", nil, func(string) {})
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
	u := newMachineUpstream("", "", nil, func(string) {})
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

func TestReadFramesDispatchesAudio(t *testing.T) {
	u := newMachineUpstream("", "", nil, func(string) {})
	reader, writer := net.Pipe()
	defer reader.Close()
	payload := []byte{0, 0, 0, 7, 0xaa, 0xbb, 0xcc}
	go func() {
		defer writer.Close()
		var header [13]byte
		header[0] = 0x04
		binary.BigEndian.PutUint64(header[1:9], 4242)
		binary.BigEndian.PutUint32(header[9:13], uint32(len(payload)))
		_, _ = writer.Write(append(header[:], payload...))
	}()
	if err := u.readFrames(reader); !errors.Is(err, io.EOF) {
		t.Fatalf("readFrames = %v, want EOF", err)
	}
	select {
	case msg := <-u.audioChan:
		if msg.tsMicros != 4242 || !bytes.Equal(msg.data, payload) {
			t.Fatalf("audio message = %+v, want ts=4242 data=%x", msg, payload)
		}
	default:
		t.Fatal("audio message not dispatched to audioChan")
	}
	if u.frameBuffers.count != 0 {
		t.Fatal("audio payload entered the video buffer pool")
	}
}

func TestSendAudioDropsOldest(t *testing.T) {
	u := newMachineUpstream("", "", nil, func(string) {})
	u.audioChan = make(chan audioMsg, 2)
	u.sendAudio(audioMsg{tsMicros: 1})
	u.sendAudio(audioMsg{tsMicros: 2})
	u.sendAudio(audioMsg{tsMicros: 3})
	first := <-u.audioChan
	if first.tsMicros != 2 {
		t.Fatalf("first queued audio ts = %d, want 2 (oldest dropped)", first.tsMicros)
	}
}

func TestQueueFrameReleasesEvictedAndStoppedFrames(t *testing.T) {
	u := newMachineUpstream("", "", nil, func(string) {})
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
	u := newMachineUpstream("", "", nil, func(string) {})
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

func TestNextFrameSendsShortBacklogInOrder(t *testing.T) {
	now := time.Now()
	current := pFrame(1)
	current.receivedAt = now.Add(-maxRelayBacklog / 2)
	ch := make(chan frameMsg, 2)
	ch <- pFrame(2)
	ch <- idrFrame(3)
	got, idr, dropped := nextFrame(ch, current, now)
	if got.tsMicros != 1 || idr || dropped || len(ch) != 2 {
		t.Fatalf("nextFrame = (ts=%d, %t, %t) queue=%d; want the oldest frame and nothing dropped", got.tsMicros, idr, dropped, len(ch))
	}
}

func TestNextFrameSkipsStaleBacklogToNewestKeyframe(t *testing.T) {
	now := time.Now()
	stale := pFrame(1)
	stale.receivedAt = now.Add(-2 * maxRelayBacklog)
	ch := make(chan frameMsg, 3)
	ch <- pFrame(2)
	ch <- idrFrame(3)
	ch <- pFrame(4)
	got, idr, dropped := nextFrame(ch, stale, now)
	if got.tsMicros != 3 || !idr || !dropped || len(ch) != 0 {
		t.Fatalf("nextFrame = (ts=%d, %t, %t) queue=%d; want the newest keyframe", got.tsMicros, idr, dropped, len(ch))
	}
	got, _, dropped = nextFrame(make(chan frameMsg), stale, now)
	if got.tsMicros != 1 || dropped {
		t.Fatal("a stale frame with nothing newer queued was dropped")
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
	u := newMachineUpstream("", "", nil, nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })
	session := &Session{}
	session.needsIDR.Store(true)

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
	u := newMachineUpstream("", "", nil, nil)
	session := &Session{}
	session.needsIDR.Store(true)
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
	u := newMachineUpstream("", "", nil, nil)
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
	u := newMachineUpstream("", "", nil, nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })
	session := &Session{}

	if !u.requestIDR(idrReasonViewerRequest) {
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

func TestIDRStatsSeparatesGrantedFromSuppressedByReason(t *testing.T) {
	now := time.Date(2026, time.January, 1, 0, 0, 0, 0, time.UTC)
	u := newMachineUpstream("", "", nil, nil)
	u.idrGate = newIDRRequestGate(func() time.Time { return now })

	u.requestIDR(idrReasonViewerRequest) // granted
	u.requestIDR(idrReasonAwaitingIDR)   // gated
	u.requestIDR(idrReasonStaleQueue)    // gated
	u.requestIDR(idrReasonStaleQueue)    // gated

	summary := u.idrStats.drain()
	want := "viewer-request=1(+0 suppressed) stale-queue-discard=0(+2 suppressed) awaiting-idr=0(+1 suppressed)"
	if summary != want {
		t.Fatalf("summary = %q, want %q", summary, want)
	}
	if again := u.idrStats.drain(); again != "" {
		t.Fatalf("counts survived a drain: %q", again)
	}
}

func TestStageStatsReportsWorstCaseSeparatelyFromAverage(t *testing.T) {
	var stats stageStats
	stats.observe(1, 2*time.Millisecond, 4*time.Millisecond, false, 10_000, false)
	stats.observe(9, 20*time.Millisecond, 40*time.Millisecond, true, 700_000, true)

	summary := stats.summary()
	want := "frames=2 queue avg=5.0 max=9 | relay wait avg=11.0ms max=20.0ms | " +
		"send avg=22.0ms max=40.0ms | idr n=1 max=700000 bytes max send=40.0ms | resets=1"
	if summary != want {
		t.Fatalf("summary = %q, want %q", summary, want)
	}
	if empty := (&stageStats{}).summary(); empty != "" {
		t.Fatalf("idle window summary = %q, want empty", empty)
	}
}

func encodeHostFrame(kind byte, ts uint64, data []byte) []byte {
	var header [13]byte
	header[0] = kind
	binary.BigEndian.PutUint64(header[1:9], ts)
	binary.BigEndian.PutUint32(header[9:13], uint32(len(data)))
	return append(header[:], data...)
}

type resetReader struct {
	data []byte
	err  error
}

func (r *resetReader) Read(p []byte) (int, error) {
	if len(r.data) == 0 {
		return 0, r.err
	}
	n := copy(p, r.data)
	r.data = r.data[n:]
	return n, nil
}

func TestReadFramesWarnsAndSkipsInvalidCursorJSON(t *testing.T) {
	u := newMachineUpstream("", "", nil, func(string) {})
	reader, writer := net.Pipe()
	defer reader.Close()
	go func() {
		defer writer.Close()
		_, _ = writer.Write(encodeHostFrame(0x02, 1, []byte("{not json")))
		_, _ = writer.Write(encodeHostFrame(0x02, 2, []byte(`{"visible":false}`)))
	}()
	if err := u.readFrames(reader); !errors.Is(err, io.EOF) {
		t.Fatalf("readFrames = %v, want EOF", err)
	}
	if u.lastCursor.tsMicros != 2 || string(u.lastCursor.data) != `{"visible":false}` {
		t.Fatalf("lastCursor = %+v, want only the valid cursor", u.lastCursor)
	}
}

func TestReadFramesRejectsInvalidLength(t *testing.T) {
	u := newMachineUpstream("", "", nil, func(string) {})
	reader, writer := net.Pipe()
	defer reader.Close()
	go func() {
		defer writer.Close()
		var header [13]byte
		header[0] = 0x01
		_, _ = writer.Write(header[:])
	}()
	if err := u.readFrames(reader); err == nil || err.Error() != "invalid frame length: 0" {
		t.Fatalf("readFrames = %v, want invalid frame length", err)
	}
}

func TestReadQUICStreamSkipsOnlyPeerResets(t *testing.T) {
	u := newMachineUpstream("", "", nil, func(string) {})
	var frameCount uint64
	reset := &quic.StreamError{StreamID: 3, Remote: true}
	video := encodeHostFrame(0x01, 7, pFrame(1).data)
	for name, data := range map[string][]byte{"before header": nil, "mid header": video[:5], "mid payload": video[:15]} {
		if err := u.readQUICStream(&resetReader{data: data, err: reset}, &frameCount); err != nil {
			t.Fatalf("%s: reset stream = %v, want skipped", name, err)
		}
	}
	if len(u.frameChan) != 0 || frameCount != 0 || cachedFrameBuffers(&u.frameBuffers) != 1 {
		t.Fatalf("reset streams leaked frames: queued=%d count=%d cached=%d", len(u.frameChan), frameCount, cachedFrameBuffers(&u.frameBuffers))
	}

	for name, stream := range map[string]io.Reader{
		"truncated":      bytes.NewReader(video[:15]),
		"invalid length": bytes.NewReader(make([]byte, 13)),
		"local cancel":   &resetReader{err: &quic.StreamError{StreamID: 3}},
		"unknown type":   bytes.NewReader(encodeHostFrame(0x7f, 0, []byte{1})),
	} {
		if err := u.readQUICStream(stream, &frameCount); err == nil {
			t.Fatalf("%s: readQUICStream = nil, want fatal error", name)
		}
	}

	if err := u.readQUICStream(bytes.NewReader(video), &frameCount); err != nil {
		t.Fatalf("complete stream = %v", err)
	}
	frame := <-u.frameChan
	if frameCount != 1 || frame.tsMicros != 7 || !bytes.Equal(frame.data, pFrame(1).data) {
		t.Fatalf("video frame = %+v (count=%d)", frame, frameCount)
	}
	frame.release()
}
