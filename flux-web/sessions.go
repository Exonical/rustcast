package main

import (
	"crypto/rand"
	"encoding/hex"
	"net/http"
	"sort"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
)

type sessionInfo struct {
	ID          string    `json:"id"`
	MachineID   string    `json:"machine_id"`
	MachineName string    `json:"machine_name"`
	ClientIP    string    `json:"client_ip"`
	UserAgent   string    `json:"user_agent"`
	User        string    `json:"user,omitempty"`
	UserSub     string    `json:"user_sub,omitempty"`
	OSUser      string    `json:"os_user,omitempty"`
	OSUID       int64     `json:"os_uid,omitempty"`
	StartedAt   time.Time `json:"started_at"`
	Width       uint32    `json:"width"`
	Height      uint32    `json:"height"`
}

type sessionEntry struct {
	info    sessionInfo
	sess    *Session
	end     func(reason string)
	endOnce sync.Once
}

type sessionRegistry struct {
	mu sync.Mutex
	m  map[string]*sessionEntry
}

func newSessionRegistry() *sessionRegistry {
	return &sessionRegistry{m: make(map[string]*sessionEntry)}
}

func (r *sessionRegistry) newID() (string, error) {
	var bytes [16]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(bytes[:]), nil
}

func (r *sessionRegistry) add(info sessionInfo, sess *Session, end func(reason string)) {
	entry := &sessionEntry{info: info, sess: sess, end: end}
	r.mu.Lock()
	r.m[info.ID] = entry
	r.mu.Unlock()
}

func (r *sessionRegistry) remove(id string) {
	r.mu.Lock()
	delete(r.m, id)
	r.mu.Unlock()
}

func (r *sessionRegistry) list() []sessionInfo {
	r.mu.Lock()
	result := make([]sessionInfo, 0, len(r.m))
	for _, entry := range r.m {
		result = append(result, entry.info)
	}
	r.mu.Unlock()
	sort.Slice(result, func(i, j int) bool {
		return result[i].StartedAt.Before(result[j].StartedAt)
	})
	return result
}

// sessionViewer pairs a live session with its audit metadata.
type sessionViewer struct {
	info sessionInfo
	sess *Session
}

// sessionsFor returns the active sessions bound to a machine.
func (r *sessionRegistry) sessionsFor(machineID string) []sessionViewer {
	r.mu.Lock()
	defer r.mu.Unlock()
	var out []sessionViewer
	for _, entry := range r.m {
		if entry.info.MachineID == machineID && entry.sess != nil {
			out = append(out, sessionViewer{info: entry.info, sess: entry.sess})
		}
	}
	return out
}

// setOSInfo records the OS login bound to a session.
func (r *sessionRegistry) setOSInfo(id, osUser string, osUID int64) {
	r.mu.Lock()
	if entry, ok := r.m[id]; ok {
		entry.info.OSUser = osUser
		entry.info.OSUID = osUID
	}
	r.mu.Unlock()
}

// endByID ends a session with a specific reason.
func (r *sessionRegistry) endByID(id, reason string) {
	r.mu.Lock()
	entry := r.m[id]
	r.mu.Unlock()
	if entry == nil {
		return
	}
	entry.endOnce.Do(func() {
		if entry.end != nil {
			entry.end(reason)
		}
	})
}

func (r *sessionRegistry) disconnect(id string) bool {
	r.mu.Lock()
	entry := r.m[id]
	r.mu.Unlock()
	if entry == nil {
		return false
	}
	entry.endOnce.Do(func() {
		if entry.end != nil {
			entry.end("disconnected by operator")
		}
	})
	return true
}

func registerSessionRoutes(api, admin *gin.RouterGroup, sessions *sessionRegistry) {
	api.GET("/sessions", func(c *gin.Context) {
		c.JSON(http.StatusOK, sessions.list())
	})
	admin.POST("/sessions/:id/disconnect", func(c *gin.Context) {
		if !sessions.disconnect(c.Param("id")) {
			c.JSON(http.StatusNotFound, gin.H{"error": "session not found"})
			return
		}
		c.Status(http.StatusNoContent)
	})
}
