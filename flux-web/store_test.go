package main

import (
	"testing"
	"time"
)

func testStore(t *testing.T) *store {
	t.Helper()
	st, err := openStore(t.TempDir())
	if err != nil {
		t.Fatalf("openStore: %v", err)
	}
	t.Cleanup(func() { st.Close() })
	return st
}

func TestStoreDefaultPolicy(t *testing.T) {
	st := testStore(t)
	p := st.getPolicy("never-seen")
	if p.Mode != clipModeBoth || p.MaxBytes != defaultClipboardMaxBytes || !p.CaptureContent {
		t.Fatalf("default policy = %+v, want {both 1MiB true}", p)
	}
}

func TestStorePutGetAndReload(t *testing.T) {
	dir := t.TempDir()
	st, err := openStore(dir)
	if err != nil {
		t.Fatalf("openStore: %v", err)
	}
	want := machinePolicy{Mode: clipModeClientToHost, MaxBytes: 4096, CaptureContent: false}
	if err := st.putPolicy("m1", want, "sub-1 (Alice)"); err != nil {
		t.Fatalf("putPolicy: %v", err)
	}
	got := st.getPolicy("m1")
	if got.Mode != want.Mode || got.MaxBytes != want.MaxBytes || got.CaptureContent != want.CaptureContent || got.UpdatedBy != "sub-1 (Alice)" {
		t.Fatalf("policy = %+v, want %+v", got, want)
	}

	// Update in place.
	want.Mode, want.MaxBytes = clipModeOff, 128
	if err := st.putPolicy("m1", want, "sub-2"); err != nil {
		t.Fatalf("putPolicy update: %v", err)
	}
	st.Close()

	// Reopen the same file: the row must survive.
	st2, err := openStore(dir)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer st2.Close()
	got = st2.getPolicy("m1")
	if got.Mode != clipModeOff || got.MaxBytes != 128 || got.CaptureContent || got.UpdatedBy != "sub-2" {
		t.Fatalf("reloaded policy = %+v, want {off 128 false sub-2}", got)
	}
}

func TestStoreRejectsInvalidPolicy(t *testing.T) {
	st := testStore(t)
	if err := st.putPolicy("m1", machinePolicy{Mode: "sideways", MaxBytes: 1}, "u"); err == nil {
		t.Fatal("expected invalid mode error")
	}
	if err := st.putPolicy("m1", machinePolicy{Mode: clipModeBoth, MaxBytes: 0}, "u"); err == nil {
		t.Fatal("expected invalid max_bytes error")
	}
}

func TestStoreAuditCaptureContent(t *testing.T) {
	st := testStore(t)
	ev := auditEvent{MachineID: "m1", Direction: clipDirClientToHost, Mime: clipWireMime,
		Data: []byte("secret"), Result: clipResultOK}
	if err := st.insertAudit(ev, true); err != nil {
		t.Fatal(err)
	}
	if err := st.insertAudit(ev, false); err != nil {
		t.Fatal(err)
	}
	rows, err := st.listAudit("m1", 10, 0)
	if err != nil || len(rows) != 2 {
		t.Fatalf("listAudit rows=%v err=%v", rows, err)
	}
	// Newest first: rows[0] is the uncaptured insert, rows[1] the captured one.
	if rows[0].HasContent {
		t.Fatalf("uncaptured row has_content=true: %+v", rows[0])
	}
	if !rows[1].HasContent {
		t.Fatalf("captured row has_content=false: %+v", rows[1])
	}
	_, content, err := st.getAuditContent(rows[1].ID)
	if err != nil || string(content) != "secret" {
		t.Fatalf("content=%q err=%v", content, err)
	}
	_, content, _ = st.getAuditContent(rows[0].ID)
	if len(content) != 0 {
		t.Fatalf("uncaptured row content=%q, want empty", content)
	}
	if rows[1].SHA256 == "" || len(rows[1].SHA256) != 64 {
		t.Fatalf("sha256 not populated: %+v", rows[1])
	}
}

func TestStorePurge(t *testing.T) {
	st := testStore(t)
	if _, err := st.db.Exec(
		`INSERT INTO clipboard_events (ts, machine_id, direction, size, sha256, result)
		 VALUES (?, 'm1', 'client_to_host', 1, 'x', 'ok')`,
		time.Now().AddDate(0, 0, -60).UTC().Format(time.RFC3339Nano)); err != nil {
		t.Fatal(err)
	}
	ev := auditEvent{MachineID: "m1", Direction: clipDirHostToClient, Data: []byte("a"), Result: clipResultOK}
	if err := st.insertAudit(ev, false); err != nil {
		t.Fatal(err)
	}
	n, err := st.purge(time.Now().AddDate(0, 0, -30))
	if err != nil || n != 1 {
		t.Fatalf("purge removed %d rows, want 1 (err=%v)", n, err)
	}
	rows, _ := st.listAudit("m1", 10, 0)
	if len(rows) != 1 {
		t.Fatalf("expected 1 row after purge, got %d", len(rows))
	}
}

func TestListAuditFilters(t *testing.T) {
	st := testStore(t)
	for i := 0; i < 5; i++ {
		st.insertAudit(auditEvent{MachineID: "a", Direction: clipDirHostToClient, Data: []byte("x"), Result: clipResultOK}, false)
	}
	st.insertAudit(auditEvent{MachineID: "b", Direction: clipDirHostToClient, Data: []byte("y"), Result: clipResultBlocked}, false)

	rows, _ := st.listAudit("a", 0, 0)
	if len(rows) != 5 {
		t.Fatalf("machine filter returned %d rows, want 5", len(rows))
	}
	rows, _ = st.listAudit("", 2, 0)
	if len(rows) != 2 {
		t.Fatalf("limit returned %d rows, want 2", len(rows))
	}
	rows, _ = st.listAudit("a", 10, rows[1].ID)
	if len(rows) != 4 {
		t.Fatalf("before filter returned %d rows, want 4", len(rows))
	}
	if rows[0].Mime != "" || rows[0].HasContent {
		t.Fatalf("content must not appear in list: %+v", rows[0])
	}
}

func TestPolicyAllowsEachMode(t *testing.T) {
	cases := []struct {
		mode         string
		hostToClient bool
		clientToHost bool
	}{
		{clipModeBoth, true, true},
		{clipModeOff, false, false},
		{clipModeHostToClient, true, false},
		{clipModeClientToHost, false, true},
		{"bogus", false, false},
	}
	for _, c := range cases {
		p := machinePolicy{Mode: c.mode}
		if got := p.allows(clipDirHostToClient); got != c.hostToClient {
			t.Errorf("mode %s allows(host_to_client) = %v, want %v", c.mode, got, c.hostToClient)
		}
		if got := p.allows(clipDirClientToHost); got != c.clientToHost {
			t.Errorf("mode %s allows(client_to_host) = %v, want %v", c.mode, got, c.clientToHost)
		}
	}
}
