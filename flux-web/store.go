package main

// Clipboard policy + audit persistence, backed by a per-relay SQLite database
// (pure-Go modernc.org/sqlite, no cgo). All clipboard policy and audit records
// are scoped per machine; machines without a row get the permissive default.

import (
	"context"
	"database/sql"
	"encoding/json"
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
	ConnectionPath string `json:"connection_path"`
	UpdatedAt      string `json:"updated_at,omitempty"`
	UpdatedBy      string `json:"updated_by,omitempty"`
}

func defaultMachinePolicy() machinePolicy {
	return machinePolicy{Mode: clipModeBoth, MaxBytes: defaultClipboardMaxBytes, CaptureContent: true, ConnectionPath: "auto"}
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
	EventID     string
	Timestamp   string
	SessionID   string
	UserSub     string
	UserName    string
	ClientIP    string
	Direction   string
	Mime        string
	Data        []byte // hashed always, stored only when policy captures content
	Size        int64
	SHA256      string
	Result      string
	OSUser      string
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
	OSUser      string `json:"os_user,omitempty"`
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
	st := &store{db: db}
	if err := st.migrate(); err != nil {
		db.Close()
		return nil, fmt.Errorf("migrate schema: %w", err)
	}
	return st, nil
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
    connection_path TEXT NOT NULL DEFAULT 'auto',
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
    content      BLOB,
    event_id     TEXT
);
CREATE INDEX IF NOT EXISTS clipboard_events_ts ON clipboard_events(ts);
CREATE INDEX IF NOT EXISTS clipboard_events_machine_ts ON clipboard_events(machine_id, ts);
CREATE TABLE IF NOT EXISTS host_logins (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           TEXT NOT NULL,
    machine_id   TEXT NOT NULL,
    machine_name TEXT,
    session_id   TEXT,
    user_sub     TEXT,
    user_name    TEXT,
    client_ip    TEXT,
    os_user      TEXT,
    result       TEXT NOT NULL,
    event_id     TEXT
);
CREATE INDEX IF NOT EXISTS host_logins_ts ON host_logins(ts);
CREATE INDEX IF NOT EXISTS host_logins_machine_ts ON host_logins(machine_id, ts);
CREATE TABLE IF NOT EXISTS audit_forward_queue (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id   TEXT NOT NULL UNIQUE,
    payload    TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT 0
);
`

func (s *store) Close() error { return s.db.Close() }

func (s *store) persistAuditForwardEvent(event forwardedAuditEvent) error {
	payload, err := json.Marshal(event)
	if err != nil {
		return err
	}
	_, err = s.db.Exec(
		`INSERT INTO audit_forward_queue (event_id, payload, created_at) VALUES (?, ?, ?)`,
		event.EventID, string(payload), time.Now().Unix())
	return err
}

func (s *store) loadAuditForwardQueue(limit int, retention time.Duration) ([]forwardedAuditEvent, int, error) {
	if limit <= 0 {
		limit = auditForwardQueueLimit
	}
	rows, err := s.db.Query(`SELECT event_id, payload, created_at FROM audit_forward_queue ORDER BY seq`)
	if err != nil {
		return nil, 0, err
	}
	type queueEntry struct {
		eventID string
		event   forwardedAuditEvent
	}
	entries := make([]queueEntry, 0)
	droppedIDs := make([]string, 0)
	cutoff := time.Now().Add(-retention)
	for rows.Next() {
		var eventID, payload string
		var createdAt int64
		if err := rows.Scan(&eventID, &payload, &createdAt); err != nil {
			rows.Close()
			return nil, 0, err
		}
		enqueuedAt := time.Unix(createdAt, 0)
		if retention > 0 && enqueuedAt.Before(cutoff) {
			droppedIDs = append(droppedIDs, eventID)
			continue
		}
		var event forwardedAuditEvent
		if err := json.Unmarshal([]byte(payload), &event); err != nil || event.EventID != eventID {
			droppedIDs = append(droppedIDs, eventID)
			continue
		}
		event.EnqueuedAt = enqueuedAt
		entries = append(entries, queueEntry{eventID: eventID, event: event})
	}
	if err := rows.Err(); err != nil {
		rows.Close()
		return nil, 0, err
	}
	if err := rows.Close(); err != nil {
		return nil, 0, err
	}
	if len(entries) > limit {
		dropCount := len(entries) - limit
		for _, entry := range entries[:dropCount] {
			droppedIDs = append(droppedIDs, entry.eventID)
		}
		entries = entries[dropCount:]
	}
	events := make([]forwardedAuditEvent, len(entries))
	for i, entry := range entries {
		events[i] = entry.event
	}
	return events, len(droppedIDs), s.deleteAuditForwardEvents(droppedIDs)
}

func (s *store) deleteAuditForwardEvents(eventIDs []string) error {
	if len(eventIDs) == 0 {
		return nil
	}
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	statement, err := tx.Prepare(`DELETE FROM audit_forward_queue WHERE event_id = ?`)
	if err != nil {
		_ = tx.Rollback()
		return err
	}
	for _, eventID := range eventIDs {
		if _, err := statement.Exec(eventID); err != nil {
			_ = statement.Close()
			_ = tx.Rollback()
			return err
		}
	}
	if err := statement.Close(); err != nil {
		_ = tx.Rollback()
		return err
	}
	return tx.Commit()
}

// migrate applies additive schema changes to existing databases.
func (s *store) migrate() error {
	has, err := s.hasColumn("machine_policy", "connection_path")
	if err != nil {
		return err
	}
	if !has {
		if _, err := s.db.Exec(`ALTER TABLE machine_policy ADD COLUMN connection_path TEXT NOT NULL DEFAULT 'auto'`); err != nil {
			return err
		}
	}
	has, err = s.hasColumn("audit_forward_queue", "created_at")
	if err != nil {
		return err
	}
	if !has {
		if _, err := s.db.Exec(`ALTER TABLE audit_forward_queue ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0`); err != nil {
			return err
		}
	}
	if _, err := s.db.Exec(`UPDATE audit_forward_queue
		SET created_at = CAST(strftime('%s','now') AS INTEGER)
		WHERE created_at = 0`); err != nil {
		return err
	}
	has, err = s.hasColumn("clipboard_events", "os_user")
	if err != nil {
		return err
	}
	if !has {
		if _, err := s.db.Exec(`ALTER TABLE clipboard_events ADD COLUMN os_user TEXT`); err != nil {
			return err
		}
	}
	for _, table := range []string{"clipboard_events", "host_logins"} {
		has, err := s.hasColumn(table, "event_id")
		if err != nil {
			return err
		}
		if !has {
			if _, err := s.db.Exec(`ALTER TABLE ` + table + ` ADD COLUMN event_id TEXT`); err != nil {
				return err
			}
		}
	}
	if _, err := s.db.Exec(`CREATE UNIQUE INDEX IF NOT EXISTS clipboard_events_event_id
		ON clipboard_events(event_id)`); err != nil {
		return err
	}
	if _, err := s.db.Exec(`CREATE UNIQUE INDEX IF NOT EXISTS host_logins_event_id
		ON host_logins(event_id)`); err != nil {
		return err
	}
	return nil
}

func (s *store) hasColumn(table, column string) (bool, error) {
	rows, err := s.db.Query(`SELECT name FROM pragma_table_info(?)`, table)
	if err != nil {
		return false, err
	}
	defer rows.Close()
	for rows.Next() {
		var name string
		if err := rows.Scan(&name); err != nil {
			return false, err
		}
		if name == column {
			return true, nil
		}
	}
	return false, rows.Err()
}

// getPolicy returns the stored policy for machineID, or the permissive
// default when no row exists.
func (s *store) getPolicy(machineID string) machinePolicy {
	var p machinePolicy
	var capture int
	err := s.db.QueryRow(
		`SELECT clipboard_mode, max_bytes, capture_content, connection_path, updated_at, updated_by
		 FROM machine_policy WHERE machine_id = ?`, machineID).
		Scan(&p.Mode, &p.MaxBytes, &capture, &p.ConnectionPath, &p.UpdatedAt, &p.UpdatedBy)
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
	if p.ConnectionPath == "" {
		p.ConnectionPath = "auto"
	}
	switch p.ConnectionPath {
	case "auto", "direct_only", "relay_only":
	default:
		return errors.New("connection_path must be one of auto|direct_only|relay_only")
	}
	if p.MaxBytes <= 0 || p.MaxBytes > 64<<20 {
		return errors.New("max_bytes must be between 1 and 67108864")
	}
	capture := 0
	if p.CaptureContent {
		capture = 1
	}
	_, err := s.db.Exec(
		`INSERT INTO machine_policy (machine_id, clipboard_mode, max_bytes, capture_content, connection_path, updated_at, updated_by)
		 VALUES (?, ?, ?, ?, ?, ?, ?)
		 ON CONFLICT(machine_id) DO UPDATE SET
		   clipboard_mode = excluded.clipboard_mode,
		   max_bytes = excluded.max_bytes,
		   capture_content = excluded.capture_content,
		   connection_path = excluded.connection_path,
		   updated_at = excluded.updated_at,
		   updated_by = excluded.updated_by`,
		machineID, p.Mode, p.MaxBytes, capture, p.ConnectionPath,
		time.Now().UTC().Format(time.RFC3339Nano), updatedBy)
	return err
}

type auditRowExecer interface {
	Exec(string, ...any) (sql.Result, error)
}

func insertAuditRow(execer auditRowExecer, ev auditEvent, capture bool) error {
	var content []byte
	if capture && len(ev.Data) > 0 {
		content = ev.Data
	}
	size, sum := ev.Size, ev.SHA256
	if ev.Data != nil {
		size, sum = int64(len(ev.Data)), sha256Hex(ev.Data)
	} else if sum == "" {
		size, sum = 0, sha256Hex(nil)
	}
	_, err := execer.Exec(
		`INSERT INTO clipboard_events
		   (ts, machine_id, machine_name, session_id, user_sub, user_name,
		    client_ip, direction, mime, size, sha256, result, content, os_user, event_id)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		 ON CONFLICT(event_id) DO NOTHING`,
		auditTimestamp(ev.Timestamp),
		ev.MachineID, nullIfEmpty(ev.MachineName), nullIfEmpty(ev.SessionID),
		nullIfEmpty(ev.UserSub), nullIfEmpty(ev.UserName), nullIfEmpty(ev.ClientIP),
		ev.Direction, nullIfEmpty(ev.Mime), size, sum, ev.Result, content,
		nullIfEmpty(ev.OSUser), nullIfEmpty(ev.EventID))
	return err
}

// insertAudit appends an audit row; content is stored only when capture is on.
func (s *store) insertAudit(ev auditEvent, capture bool) error {
	return insertAuditRow(s.db, ev, capture)
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
	                 COALESCE(os_user,''),
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
			&r.Direction, &r.Mime, &r.Size, &r.SHA256, &r.Result, &r.OSUser, &r.HasContent); err != nil {
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
		        direction, COALESCE(mime,''), size, sha256, result, COALESCE(os_user,''), content
		 FROM clipboard_events WHERE id = ?`, id).
		Scan(&r.ID, &r.Timestamp, &r.MachineID, &r.MachineName, &r.SessionID,
			&r.UserSub, &r.UserName, &r.ClientIP, &r.Direction, &r.Mime,
			&r.Size, &r.SHA256, &r.Result, &r.OSUser, &content)
	if err != nil {
		return auditRow{}, nil, err
	}
	r.HasContent = len(content) > 0
	return r, content, nil
}

// purge deletes audit rows older than the retention window.
func (s *store) purge(olderThan time.Time) (int64, error) {
	ts := olderThan.UTC().Format(time.RFC3339Nano)
	res, err := s.db.Exec(`DELETE FROM clipboard_events WHERE ts < ?`, ts)
	if err != nil {
		return 0, err
	}
	n, _ := res.RowsAffected()
	loginRes, err := s.db.Exec(`DELETE FROM host_logins WHERE ts < ?`, ts)
	if err != nil {
		return n, err
	}
	loginCount, _ := loginRes.RowsAffected()
	n += loginCount
	queueRes, err := s.db.Exec(`DELETE FROM audit_forward_queue WHERE created_at < ?`, olderThan.Unix())
	if err != nil {
		return n, err
	}
	queueCount, _ := queueRes.RowsAffected()
	n += queueCount
	return n, nil
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
					log.Printf("[store] purged %d audit and queue rows older than %d days", n, days)
				}
			}
		}
	}()
}

// hostLogin is one host OS-login attempt outcome.
type hostLogin struct {
	MachineID   string
	MachineName string
	EventID     string
	Timestamp   string
	SessionID   string
	UserSub     string
	UserName    string
	ClientIP    string
	OSUser      string
	Result      string
}

// loginRow is a stored host login audit entry.
type loginRow struct {
	ID          int64  `json:"id"`
	Timestamp   string `json:"ts"`
	MachineID   string `json:"machine_id"`
	MachineName string `json:"machine_name"`
	SessionID   string `json:"session_id"`
	UserSub     string `json:"user_sub"`
	UserName    string `json:"user_name"`
	ClientIP    string `json:"client_ip"`
	OSUser      string `json:"os_user"`
	Result      string `json:"result"`
}

func insertLoginRow(execer auditRowExecer, l hostLogin) error {
	_, err := execer.Exec(
		`INSERT INTO host_logins
		   (ts, machine_id, machine_name, session_id, user_sub, user_name,
		    client_ip, os_user, result, event_id)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		 ON CONFLICT(event_id) DO NOTHING`,
		auditTimestamp(l.Timestamp),
		l.MachineID, nullIfEmpty(l.MachineName), nullIfEmpty(l.SessionID),
		nullIfEmpty(l.UserSub), nullIfEmpty(l.UserName), nullIfEmpty(l.ClientIP),
		nullIfEmpty(l.OSUser), l.Result, nullIfEmpty(l.EventID))
	return err
}

func (s *store) insertLogin(l hostLogin) error {
	return insertLoginRow(s.db, l)
}

func (s *store) insertForwardedAudit(logins []hostLogin, clips []auditEvent) error {
	if s == nil || s.db == nil {
		return errors.New("audit store unavailable")
	}
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	for _, login := range logins {
		if err := insertLoginRow(tx, login); err != nil {
			_ = tx.Rollback()
			return err
		}
	}
	for _, clip := range clips {
		if err := insertAuditRow(tx, clip, true); err != nil {
			_ = tx.Rollback()
			return err
		}
	}
	return tx.Commit()
}

// listLogins returns the newest host login rows (id DESC).
func (s *store) listLogins(machine string, limit, before int64) ([]loginRow, error) {
	if limit <= 0 {
		limit = 100
	}
	if limit > 1000 {
		limit = 1000
	}
	query := `SELECT id, ts, machine_id, COALESCE(machine_name,''), COALESCE(session_id,''),
	                 COALESCE(user_sub,''), COALESCE(user_name,''), COALESCE(client_ip,''),
	                 COALESCE(os_user,''), result
	          FROM host_logins WHERE 1=1`
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
	var out []loginRow
	for rows.Next() {
		var r loginRow
		if err := rows.Scan(&r.ID, &r.Timestamp, &r.MachineID, &r.MachineName,
			&r.SessionID, &r.UserSub, &r.UserName, &r.ClientIP,
			&r.OSUser, &r.Result); err != nil {
			return nil, err
		}
		out = append(out, r)
	}
	return out, rows.Err()
}
