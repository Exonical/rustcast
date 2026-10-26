package main

import (
	"strings"
	"testing"
)

func TestMetricsIncludeProcessPathLabelInRelayAndDirectModes(t *testing.T) {
	for _, mode := range []string{"relay", "direct"} {
		t.Run(mode, func(t *testing.T) {
			t.Setenv("FLUX_MODE", mode)
			metrics := newRelayMetrics()
			metrics.machine("host").sessionsStarted.Inc()
			if body := scrapeMetrics(metrics); !strings.Contains(body, `path="`+mode+`"`) {
				t.Fatalf("metrics missing path=%q: %s", mode, body)
			}
		})
	}
}
