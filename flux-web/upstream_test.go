package main

import (
	"testing"
	"time"
)

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
