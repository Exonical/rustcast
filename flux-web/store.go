package main

// Clipboard policy + audit persistence, backed by a per-relay SQLite database
// (pure-Go modernc.org/sqlite, no cgo). All clipboard policy and audit records
// are scoped per machine; machines without a row get the permissive default.

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strconv"
	"time"

	_ "modernc.org/sqlite"
)

// Clipboard policy modes stored in machine_policy.clipboard_mode.
const (
	clipModeBoth         = "both"
	clipModeOff          = "off"
	clipModeHostToClient = "host_to_client"
	clipModeClientToHost = "client_to_host"
)

// Clipboard directions recorded in clipboard_events.direction and used in
// clipboard-status control messages.
const (
	clipDirHostToClient = "host_to_client"
	clipDirClientToHost = "client_to_host"
)

// Audit results recorded in clipboard_events.result.
const (
	clipResultOK       = "ok"
	clipResultBlocked  = "blocked"
	clipResultTooLarge = "too_large"
	clipResultInvalid  = "invalid"
)

const (
	defaultClipboardMaxBytes = 1 << 20 // 1 MiB
	defaultAuditRetention    = 30      // days; 0 keeps forever
)

var errInvalidClipMode = errors.New("clipboard_mode must be one of both|off|host_to_client|client_to_host")

// machinePolicy is the effective clipboard policy for a machine.
type machinePolicy struct {
	Mode           string `json:"clipboard_mode"`
	MaxBytes       int64  `json:"max_bytes"`
	CaptureContent bool   `json:"capture_content"`
	UpdatedAt      string `json:"updated_at,omitempty"`
	UpdatedBy      string `json:"updated_by,omitempty"`
}

func defaultMachinePolicy() machinePolicy {
	return machinePolicy{Mode: clipModeBoth, MaxBytes: defaultClipboardMaxBytes, CaptureContent: true}
}

// allows reports whether the policy permits a transfer in direction.
func (p machinePolicy) allows(direction string) bool {
	switch p.Mode {
	case clipModeBoth:
		return true
	case clipModeOff:
		return false
	case clipModeHostToClient:
		return direction == clipDirHostToClient
	case clipModeClientToHost:
		return direction == clipDirClientToHost
	default:
		return false
	}
}

// auditEvent is one clipboard transfer (or attempted transfer) record.
type auditEvent struct {
	MachineID   string
	MachineName string
	SessionID   string
	UserSub     string
	UserName    string
	ClientIP    string
	Direction   string
	Mime        string
	Data        []byte // hashed always, stored only when policy captures content
	Result      string
}

// auditRow is a stored clipboard event. Content is loaded separately.
type auditRow struct {
	ID          int64  `json:"id"`
	Timestamp   string `json:"ts"`
	MachineID   string `json:"machine_id"`
	MachineName string `json:"machine_name"`
	SessionID   string `json:"session_id"`
	UserSub     string `json:"user_sub"`
	UserName    string `json:"user_name"`
	ClientIP    string `json:"client_ip"`
	Direction   string `json:"direction"`
	Mime        string `json:"mime"`
	Size        int64  `json:"size"`
	SHA256      string `json:"sha256"`
	Result      string `json:"result"`
	HasContent  bool   `json:"has_content"`
}

type store struct {
	db *sql.DB
}

// openStore opens (creating if needed) the relay SQLite database under dir.
// The caller is responsible for the directory; storeDirFromEnv resolves it.
func openStore(dir string) (*store, error) {
	if dir == "" {
		dir = "./data"
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, fmt.Errorf("create data dir %s: %w", dir, err)
	}
	dsn := fmt.Sprintf("file:%s?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)",
		filepath.Join(dir, "flux-relay.db"))
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1) // SQLite serializes writers anyway; keep it predictable.
	if _, err := db.Exec(schemaSQL); err != nil {
		db.Close()
		return nil, fmt.Errorf("apply schema: %w", err)
	}
	return &store{db: db}, nil
}

// storeDirFromEnv resolves FLUX_DATA_DIR (default ./data).
func storeDirFromEnv() string {
	if dir := os.Getenv("FLUX_DATA_DIR"); dir != "" {
		return dir
	}
	return "./data"
}

const schemaSQL = `
CREATE TABLE IF NOT EXISTS machine_policy (
    machine_id      TEXT PRIMARY KEY,
    clipboard_mode  TEXT NOT NULL,
    max_bytes       INTEGER NOT NULL,
    capture_content INTEGER NOT NULL,
    updated_at      TEXT NOT NULL,
    updated_by      TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS clipboard_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           TEXT NOT NULL,
    machine_id   TEXT NOT NULL,
    machine_name TEXT,
    session_id   TEXT,
    user_sub     TEXT,
    user_name    TEXT,
    client_ip    TEXT,
    direction    TEXT NOT NULL,
    mime         TEXT,
    size         INTEGER NOT NULL,
    sha256       TEXT NOT NULL,
    result       TEXT NOT NULL,
    content      BLOB
);
CREATE INDEX IF NOT EXISTS clipboard_events_ts ON clipboard_events(ts);
CREATE INDEX IF NOT EXISTS clipboard_events_machine_ts ON clipboard_events(machine_id, ts);
`

func (s *store) Close() error { return s.db.Close() }

// getPolicy returns the stored policy for machineID, or the permissive
// default when no row exists.
func (s *store) getPolicy(machineID string) machinePolicy {
	var p machinePolicy
	var capture int
	err := s.db.QueryRow(
		`SELECT clipboard_mode, max_bytes, capture_content, updated_at, updated_by
		 FROM machine_policy WHERE machine_id = ?`, machineID).
		Scan(&p.Mode, &p.MaxBytes, &capture, &p.UpdatedAt, &p.UpdatedBy)
	if err != nil {
		return defaultMachinePolicy()
	}
	p.CaptureContent = capture != 0
	return p
}

// putPolicy validates and upserts a machine policy.
func (s *store) putPolicy(machineID string, p machinePolicy, updatedBy string) error {
	switch p.Mode {
	case clipModeBoth, clipModeOff, clipModeHostToClient, clipModeClientToHost:
	default:
		return errInvalidClipMode
	}
	if p.MaxBytes <= 0 || p.MaxBytes > 64<<20 {
		return errors.New("max_bytes must be between 1 and 67108864")
	}
	capture := 0
	if p.CaptureContent {
		capture = 1
	}
	_, err := s.db.Exec(
		`INSERT INTO machine_policy (machine_id, clipboard_mode, max_bytes, capture_content, updated_at, updated_by)
		 VALUES (?, ?, ?, ?, ?, ?)
		 ON CONFLICT(machine_id) DO UPDATE SET
		   clipboard_mode = excluded.clipboard_mode,
		   max_bytes = excluded.max_bytes,
		   capture_content = excluded.capture_content,
		   updated_at = excluded.updated_at,
		   updated_by = excluded.updated_by`,
		machineID, p.Mode, p.MaxBytes, capture,
		time.Now().UTC().Format(time.RFC3339Nano), updatedBy)
	return err
}

// insertAudit appends an audit row; content is stored only when capture is on.
func (s *store) insertAudit(ev auditEvent, capture bool) error {
	var content []byte
	if capture && len(ev.Data) > 0 {
		content = ev.Data
	}
	sum := sha256Hex(ev.Data)
	_, err := s.db.Exec(
		`INSERT INTO clipboard_events
		   (ts, machine_id, machine_name, session_id, user_sub, user_name,
		    client_ip, direction, mime, size, sha256, result, content)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		time.Now().UTC().Format(time.RFC3339Nano),
		ev.MachineID, nullIfEmpty(ev.MachineName), nullIfEmpty(ev.SessionID),
		nullIfEmpty(ev.UserSub), nullIfEmpty(ev.UserName), nullIfEmpty(ev.ClientIP),
		ev.Direction, nullIfEmpty(ev.Mime), int64(len(ev.Data)), sum, ev.Result, content)
	return err
}

func nullIfEmpty(v string) any {
	if v == "" {
		return nil
	}
	return v
}

// listAudit returns the newest rows (id DESC) without content, honoring
// machine/limit/before filters.
func (s *store) listAudit(machine string, limit, before int64) ([]auditRow, error) {
	if limit <= 0 {
		limit = 100
	}
	if limit > 1000 {
		limit = 1000
	}
	query := `SELECT id, ts, machine_id, COALESCE(machine_name,''), COALESCE(session_id,''),
	                 COALESCE(user_sub,''), COALESCE(user_name,''), COALESCE(client_ip,''),
	                 direction, COALESCE(mime,''), size, sha256, result,
	                 content IS NOT NULL AND length(content) > 0
	          FROM clipboard_events WHERE 1=1`
	var args []any
	if machine != "" {
		query += ` AND machine_id = ?`
		args = append(args, machine)
	}
	if before > 0 {
		query += ` AND id < ?`
		args = append(args, before)
	}
	query += ` ORDER BY id DESC LIMIT ?`
	args = append(args, limit)

	rows, err := s.db.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []auditRow
	for rows.Next() {
		var r auditRow
		if err := rows.Scan(&r.ID, &r.Timestamp, &r.MachineID, &r.MachineName,
			&r.SessionID, &r.UserSub, &r.UserName, &r.ClientIP,
			&r.Direction, &r.Mime, &r.Size, &r.SHA256, &r.Result, &r.HasContent); err != nil {
			return nil, err
		}
		out = append(out, r)
	}
	return out, rows.Err()
}

// getAuditContent returns the stored content blob for a row (nil if absent).
func (s *store) getAuditContent(id int64) (auditRow, []byte, error) {
	var r auditRow
	var content []byte
	err := s.db.QueryRow(
		`SELECT id, ts, machine_id, COALESCE(machine_name,''), COALESCE(session_id,''),
		        COALESCE(user_sub,''), COALESCE(user_name,''), COALESCE(client_ip,''),
		        direction, COALESCE(mime,''), size, sha256, result, content
		 FROM clipboard_events WHERE id = ?`, id).
		Scan(&r.ID, &r.Timestamp, &r.MachineID, &r.MachineName, &r.SessionID,
			&r.UserSub, &r.UserName, &r.ClientIP, &r.Direction, &r.Mime,
			&r.Size, &r.SHA256, &r.Result, &content)
	if err != nil {
		return auditRow{}, nil, err
	}
	r.HasContent = len(content) > 0
	return r, content, nil
}

// purge deletes audit rows older than the retention window.
func (s *store) purge(olderThan time.Time) (int64, error) {
	res, err := s.db.Exec(`DELETE FROM clipboard_events WHERE ts < ?`,
		olderThan.UTC().Format(time.RFC3339Nano))
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// auditRetentionDays reads FLUX_AUDIT_RETENTION_DAYS (default 30; 0 = keep).
func auditRetentionDays() int {
	if v := os.Getenv("FLUX_AUDIT_RETENTION_DAYS"); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n >= 0 {
			return n
		}
	}
	return defaultAuditRetention
}

// startRetentionPurge purges expired audit rows hourly until ctx ends. A
// retention of 0 keeps everything.
func (s *store) startRetentionPurge(ctx context.Context, days int) {
	if days <= 0 {
		return
	}
	go func() {
		ticker := time.NewTicker(time.Hour)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				cutoff := time.Now().AddDate(0, 0, -days)
				if n, err := s.purge(cutoff); err != nil {
					log.Printf("[store] audit purge failed: %v", err)
				} else if n > 0 {
					log.Printf("[store] purged %d clipboard audit rows older than %d days", n, days)
				}
			}
		}
	}()
}
