package main

const viewerHTML = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Flux Stream Viewer</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    background: #0a0a0a;
    color: #e0e0e0;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    min-height: 100vh;
  }
  #status {
    position: fixed;
    top: 16px;
    left: 50%;
    transform: translateX(-50%);
    padding: 8px 20px;
    border-radius: 8px;
    font-size: 14px;
    font-weight: 500;
    z-index: 100;
    transition: all 0.3s ease;
  }
  .status-connecting { background: #1a1a2e; color: #ffd93d; border: 1px solid #ffd93d33; }
  .status-connected  { background: #0a2a0a; color: #4ade80; border: 1px solid #4ade8033; }
  .status-error      { background: #2a0a0a; color: #f87171; border: 1px solid #f8717133; }
  #video-container {
    position: relative;
    max-width: 95vw;
    max-height: 90vh;
    background: #111;
    border-radius: 12px;
    overflow: hidden;
    box-shadow: 0 20px 60px rgba(0,0,0,0.5);
  }
  video {
    display: block;
    max-width: 95vw;
    max-height: 85vh;
    background: #000;
  }
  #controls {
    position: fixed;
    bottom: 16px;
    display: flex;
    gap: 12px;
  }
  button {
    padding: 8px 20px;
    border: 1px solid #333;
    border-radius: 6px;
    background: #1a1a1a;
    color: #e0e0e0;
    font-size: 14px;
    cursor: pointer;
    transition: all 0.2s;
  }
  button:hover { background: #2a2a2a; border-color: #555; }
  #stats {
    position: fixed;
    bottom: 60px;
    font-size: 12px;
    color: #666;
    font-family: monospace;
  }
</style>
</head>
<body>

<div id="status" class="status-connecting">Connecting...</div>

<div id="video-container">
  <video id="video" autoplay playsinline muted></video>
</div>

<div id="stats"></div>

<div id="controls">
  <button onclick="connect()">Reconnect</button>
  <button onclick="toggleFullscreen()">Fullscreen</button>
</div>

<script>
const videoEl = document.getElementById('video');
const statusEl = document.getElementById('status');
const statsEl = document.getElementById('stats');

let ws = null;
let pc = null;
let pendingCandidates = [];
let remoteDescSet = false;

function setStatus(text, cls) {
  statusEl.textContent = text;
  statusEl.className = 'status-' + cls;
}

async function connect() {
  // Cleanup previous
  if (pc) { pc.close(); pc = null; }
  if (ws) { ws.close(); ws = null; }

  setStatus('Connecting...', 'connecting');
  pendingCandidates = [];
  remoteDescSet = false;

  // Create peer connection
  pc = new RTCPeerConnection({
    iceServers: [{ urls: 'stun:stun.l.google.com:19302' }]
  });

  // We need a transceiver for receiving video
  pc.addTransceiver('video', { direction: 'recvonly' });

  pc.ontrack = (e) => {
    console.log('[webrtc] got track:', e.track.kind);
    if (e.streams && e.streams[0]) {
      videoEl.srcObject = e.streams[0];
    }
  };

  pc.oniceconnectionstatechange = () => {
    console.log('[webrtc] ICE state:', pc.iceConnectionState);
    if (pc.iceConnectionState === 'connected' || pc.iceConnectionState === 'completed') {
      setStatus('Connected', 'connected');
    } else if (pc.iceConnectionState === 'failed') {
      setStatus('Connection failed', 'error');
    } else if (pc.iceConnectionState === 'disconnected') {
      setStatus('Disconnected', 'error');
      setTimeout(connect, 2000);
    }
  };

  // Open WebSocket for signaling
  const wsProto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  ws = new WebSocket(wsProto + '//' + location.host + '/ws/signaling');

  ws.onopen = async () => {
    console.log('[ws] connected');

    // Create and send offer
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);

    ws.send(JSON.stringify({
      type: 'offer',
      data: { sd: offer.sdp }
    }));
    console.log('[ws] sent offer');
  };

  ws.onmessage = async (evt) => {
    const msg = JSON.parse(evt.data);
    console.log('[ws] received:', msg.type);

    if (msg.type === 'answer') {
      const answerData = JSON.parse(typeof msg.data === 'string' ? msg.data : JSON.stringify(msg.data));
      await pc.setRemoteDescription({
        type: 'answer',
        sdp: answerData.sd
      });
      remoteDescSet = true;
      console.log('[ws] set remote description (answer)');

      // Drain queued ICE candidates
      for (const c of pendingCandidates) {
        await pc.addIceCandidate(c);
      }
      console.log('[ws] drained ' + pendingCandidates.length + ' queued ICE candidates');
      pendingCandidates = [];
    } else if (msg.type === 'new-ice-candidate') {
      const candidate = JSON.parse(typeof msg.data === 'string' ? msg.data : JSON.stringify(msg.data));
      if (candidate.candidate) {
        if (remoteDescSet) {
          await pc.addIceCandidate(candidate);
          console.log('[ws] added ICE candidate');
        } else {
          pendingCandidates.push(candidate);
          console.log('[ws] queued ICE candidate (remote desc not set yet)');
        }
      }
    } else if (msg.type === 'error') {
      console.error('[ws] server error:', msg.data);
      setStatus('Server error', 'error');
    }
  };

  // Send our ICE candidates to the server
  pc.onicecandidate = (e) => {
    if (e.candidate && ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({
        type: 'new-ice-candidate',
        data: e.candidate.toJSON()
      }));
    }
  };

  ws.onerror = (e) => {
    console.error('[ws] error:', e);
    setStatus('WebSocket error', 'error');
  };

  ws.onclose = () => {
    console.log('[ws] closed');
  };
}

function toggleFullscreen() {
  if (!document.fullscreenElement) {
    document.getElementById('video-container').requestFullscreen();
  } else {
    document.exitFullscreen();
  }
}

// Update stats periodically
setInterval(async () => {
  if (!pc) return;
  try {
    const stats = await pc.getStats();
    stats.forEach(report => {
      if (report.type === 'inbound-rtp' && report.kind === 'video') {
        const fps = report.framesPerSecond || 0;
        const width = report.frameWidth || '?';
        const height = report.frameHeight || '?';
        const bytesReceived = report.bytesReceived || 0;
        const kbps = report.timestamp ? Math.round(bytesReceived * 8 / (report.timestamp / 1000) / 1000) : 0;
        statsEl.textContent = width + 'x' + height + ' @ ' + fps + ' fps | ' + kbps + ' kbps';
      }
    });
  } catch(e) {}
}, 1000);

// Auto-connect on load
connect();
</script>
</body>
</html>`
