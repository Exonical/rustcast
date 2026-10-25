package main

import (
	"bytes"
	"testing"
)

// The relay's protocol names must keep the bytes the Rust host speaks
// (flux-server frame_protocol.rs, flux-hostd proto.rs).
func TestProtocolByteValues(t *testing.T) {
	for name, tc := range map[string]struct{ got, want byte }{
		"msgVideo":               {msgVideo, 0x01},
		"msgCursor":              {msgCursor, 0x02},
		"msgResolutionStatus":    {msgResolutionStatus, 0x03},
		"msgAudio":               {msgAudio, 0x04},
		"msgClipboard":           {msgClipboard, 0x05},
		"msgLogin":               {msgLogin, 0x06},
		"cmdIDR":                 {cmdIDR, 0x01},
		"cmdInput":               {cmdInput, 0x02},
		"cmdBitrate":             {cmdBitrate, 0x03},
		"cmdViewers":             {cmdViewers, 0x04},
		"cmdQuality":             {cmdQuality, 0x05},
		"cmdResolution":          {cmdResolution, 0x06},
		"cmdClipboard":           {cmdClipboard, 0x07},
		"cmdLoginResponse":       {cmdLoginResponse, 0x08},
		"cmdLock":                {cmdLock, 0x09},
		"cmdHello":               {cmdHello, 0x0A},
		"streamVideo":            {streamVideo, 0x01},
		"streamCursor":           {streamCursor, 0x02},
		"streamResolutionStatus": {streamResolutionStatus, 0x03},
		"streamAudio":            {streamAudio, 0x04},
	} {
		if tc.got != tc.want {
			t.Errorf("%s = 0x%02x, want 0x%02x", name, tc.got, tc.want)
		}
	}
}

func TestCommandEncoders(t *testing.T) {
	for name, tc := range map[string]struct{ got, want []byte }{
		"login response": {commandFrame(cmdLoginResponse, []byte(`{}`)), []byte{0x08, 0, 0, 0, 2, '{', '}'}},
		"empty lock":     {commandFrame(cmdLock, nil), []byte{0x09, 0, 0, 0, 0}},
		"hello":          {commandFrame(cmdHello, []byte("tok")), []byte{0x0A, 0, 0, 0, 3, 't', 'o', 'k'}},
		"bitrate":        {u32Command(cmdBitrate, 0x01020304), []byte{0x03, 1, 2, 3, 4}},
		"viewers":        {u32Command(cmdViewers, 2), []byte{0x04, 0, 0, 0, 2}},
	} {
		if !bytes.Equal(tc.got, tc.want) {
			t.Errorf("%s = %x, want %x", name, tc.got, tc.want)
		}
	}
}
