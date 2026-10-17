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
	StartedAt   time.Time `json:"started_at"`
	Width       uint32    `json:"width"`
	Height      uint32    `json:"height"`
}

type sessionEntry struct {
	info    sessionInfo
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

func (r *sessionRegistry) add(info sessionInfo, end func(reason string)) {
	entry := &sessionEntry{info: info, end: end}
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

func registerSessionRoutes(r *gin.Engine, sessions *sessionRegistry) {
	// These endpoints are intentionally unauthenticated, like the machine routes.
	r.GET("/api/sessions", func(c *gin.Context) {
		c.JSON(http.StatusOK, sessions.list())
	})
	r.POST("/api/sessions/:id/disconnect", func(c *gin.Context) {
		if !sessions.disconnect(c.Param("id")) {
			c.JSON(http.StatusNotFound, gin.H{"error": "session not found"})
			return
		}
		c.Status(http.StatusNoContent)
	})
}
