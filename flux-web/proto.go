package main

import "encoding/binary"

// Host↔relay frame-server wire protocol. The bytes mirror
// flux/crates/flux-server/src/frame_protocol.rs (frames 0x01-0x05, commands
// 0x01-0x07) and flux/crates/flux-hostd/src/proto.rs (login 0x06, commands
// 0x08-0x0A). Frame types and commands share byte values, so keep the msg*
// and cmd* namespaces apart.

// Host→relay frame message types: [type u8][ts u64 BE][len u32 BE][payload].
const (
	msgVideo            byte = 0x01 // H.264 access unit
	msgCursor           byte = 0x02 // cursor JSON metadata
	msgResolutionStatus byte = 0x03 // resolution transition status JSON
	msgAudio            byte = 0x04 // [seq u32 BE][opus payload]
	msgClipboard        byte = 0x05 // JSON {"mime","data"}
	msgLogin            byte = 0x06 // hostd PAM login JSON
)

// Relay→host command types (first byte of each command).
const (
	cmdIDR           byte = 0x01 // no payload
	cmdInput         byte = 0x02 // [len u32 BE][JSON input event]
	cmdBitrate       byte = 0x03 // [kbps u32 BE]
	cmdViewers       byte = 0x04 // [count u32 BE]
	cmdQuality       byte = 0x05 // [quality level][FPS cap]
	cmdResolution    byte = 0x06 // [width u16 BE][height u16 BE]
	cmdClipboard     byte = 0x07 // [len u32 BE][JSON {"mime","data"}]
	cmdLoginResponse byte = 0x08 // [len u32 BE][JSON {"response"}]
	cmdLock          byte = 0x09 // [len u32 BE = 0]
	cmdHello         byte = 0x0A // [len u32 BE][relay token]
)

// Relay→browser WebTransport uni-stream kinds (first byte of each stream).
const (
	streamVideo            byte = 0x01
	streamCursor           byte = 0x02
	streamResolutionStatus byte = 0x03
	streamAudio            byte = 0x04
)

// commandFrame encodes a length-prefixed relay→host command:
// [cmdType][len u32 BE][payload].
func commandFrame(cmdType byte, payload []byte) []byte {
	cmd := make([]byte, 5+len(payload))
	cmd[0] = cmdType
	binary.BigEndian.PutUint32(cmd[1:5], uint32(len(payload)))
	copy(cmd[5:], payload)
	return cmd
}

// u32Command encodes a relay→host command carrying one value:
// [cmdType][value u32 BE].
func u32Command(cmdType byte, value uint32) []byte {
	cmd := make([]byte, 5)
	cmd[0] = cmdType
	binary.BigEndian.PutUint32(cmd[1:], value)
	return cmd
}
