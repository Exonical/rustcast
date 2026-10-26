package main

import "testing"

func TestTicketKeyURLAllowed(t *testing.T) {
	tests := []struct {
		name string
		raw  string
		want bool
	}{
		{name: "https relay", raw: "https://relay.example.test", want: true},
		{name: "http IPv4 loopback", raw: "http://127.0.0.1:8080", want: true},
		{name: "http IPv6 loopback", raw: "http://[::1]:8080", want: true},
		{name: "http localhost", raw: "http://localhost:8080", want: true},
		{name: "http external host", raw: "http://relay.example.test", want: false},
		{name: "garbage", raw: "not a URL", want: false},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := ticketKeyURLAllowed(test.raw); got != test.want {
				t.Fatalf("ticketKeyURLAllowed(%q) = %v, want %v", test.raw, got, test.want)
			}
		})
	}
}
