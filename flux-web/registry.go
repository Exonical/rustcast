package main

import (
	"log"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
)

const machineExpiry = 15 * time.Second

type machineInfo struct {
	ID             string    `json:"id"`
	Name           string    `json:"name"`
	DisplayName    string    `json:"display_name,omitempty"`
	FrameEndpoint  string    `json:"frame_endpoint"`
	OS             string    `json:"os,omitempty"`
	GPUVendor      string    `json:"gpu_vendor,omitempty"`
	EncoderBackend string    `json:"encoder_backend,omitempty"`
	VirtualDisplay bool      `json:"virtual_display"`
	Width          uint32    `json:"width,omitempty"`
	Height         uint32    `json:"height,omitempty"`
	TargetFPS      uint32    `json:"target_fps,omitempty"`
	LastSeen       time.Time `json:"last_seen"`
	Status         string    `json:"status"`
	UpstreamStatus string    `json:"upstream_status"`
}

type machineRegistration struct {
	ID             string `json:"id" binding:"required"`
	Name           string `json:"name" binding:"required"`
	DisplayName    string `json:"display_name"`
	FrameEndpoint  string `json:"frame_endpoint" binding:"required"`
	OS             string `json:"os"`
	GPUVendor      string `json:"gpu_vendor"`
	EncoderBackend string `json:"encoder_backend"`
	VirtualDisplay bool   `json:"virtual_display"`
	Width          uint32 `json:"width"`
	Height         uint32 `json:"height"`
	TargetFPS      uint32 `json:"target_fps"`
}

type machineRecord struct {
	machineInfo
	static   bool
	upstream *machineUpstream
}

type machineRegistry struct {
	mu       sync.Mutex
	machines map[string]*machineRecord
}

func newMachineRegistry() *machineRegistry {
	return &machineRegistry{machines: make(map[string]*machineRecord)}
}

func (r *machineRegistry) seedStatic(id, name, endpoint string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.machines[id] = &machineRecord{
		machineInfo: machineInfo{
			ID:             id,
			Name:           name,
			FrameEndpoint:  endpoint,
			LastSeen:       time.Now(),
			Status:         "online",
			UpstreamStatus: "idle",
		},
		static: true,
	}
}

func (r *machineRegistry) upsert(input machineRegistration, static bool) *machineRecord {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	m := r.machines[input.ID]
	if m == nil {
		m = &machineRecord{}
		r.machines[input.ID] = m
	}
	upstreamStatus := m.UpstreamStatus
	if upstreamStatus == "" {
		upstreamStatus = "idle"
	}
	m.machineInfo = machineInfo{
		ID: input.ID, Name: input.Name, FrameEndpoint: input.FrameEndpoint,
		DisplayName: input.DisplayName,
		OS:          input.OS, GPUVendor: input.GPUVendor, EncoderBackend: input.EncoderBackend,
		VirtualDisplay: input.VirtualDisplay, Width: input.Width, Height: input.Height,
		TargetFPS: input.TargetFPS, LastSeen: now, Status: "online",
		UpstreamStatus: upstreamStatus,
	}
	m.static = static
	return m
}

func (r *machineRegistry) get(id string) (*machineRecord, error) {
	r.mu.Lock()
	m := r.machines[id]
	if m == nil {
		r.mu.Unlock()
		return nil, errMachineNotFound
	}
	var stale *machineUpstream
	if !m.static && time.Since(m.LastSeen) > machineExpiry {
		m.Status = "offline"
		stale = m.upstream
		if stale != nil {
			m.upstream = nil
		}
	}
	if m.Status != "online" {
		r.mu.Unlock()
		if stale != nil {
			stale.stop()
		}
		return nil, errMachineOffline
	}
	r.mu.Unlock()
	return m, nil
}

func (r *machineRegistry) list() []machineInfo {
	r.mu.Lock()
	result := make([]machineInfo, 0, len(r.machines))
	var stale []*machineUpstream
	for _, m := range r.machines {
		if !m.static && time.Since(m.LastSeen) > machineExpiry {
			m.Status = "offline"
			if m.upstream != nil {
				stale = append(stale, m.upstream)
				m.upstream = nil
			}
		}
		result = append(result, m.machineInfo)
	}
	r.mu.Unlock()
	for _, upstream := range stale {
		upstream.stop()
	}
	return result
}

func (r *machineRegistry) deregister(id string) bool {
	r.mu.Lock()
	m := r.machines[id]
	if m == nil || m.static {
		r.mu.Unlock()
		return false
	}
	upstream := m.upstream
	delete(r.machines, id)
	r.mu.Unlock()
	if upstream != nil {
		upstream.stop()
	}
	return true
}

func (r *machineRegistry) acquire(id string) (*machineUpstream, error) {
	m, err := r.get(id)
	if err != nil {
		return nil, err
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	if m.upstream == nil {
		m.upstream = newMachineUpstream(m.FrameEndpoint, m.ID, func(status string) {
			r.setUpstreamStatus(m.ID, status)
		})
		go m.upstream.run()
	}
	m.upstream.viewers++
	return m.upstream, nil
}

func (r *machineRegistry) setUpstreamStatus(id, status string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if m := r.machines[id]; m != nil {
		m.UpstreamStatus = status
	}
}

func (r *machineRegistry) release(m *machineUpstream) {
	r.mu.Lock()
	if m.viewers > 0 {
		m.viewers--
	}
	stop := false
	if m.viewers == 0 {
		stop = true
		for _, record := range r.machines {
			if record.upstream == m {
				record.upstream = nil
				break
			}
		}
	}
	r.mu.Unlock()
	if stop {
		m.stop()
	}
}

var (
	errMachineNotFound = &machineError{"machine not found"}
	errMachineOffline  = &machineError{"machine is offline"}
)

type machineError struct{ message string }

func (e *machineError) Error() string { return e.message }

func registerMachineRoutes(r *gin.Engine, registry *machineRegistry) {
	r.POST("/api/machines/register", func(c *gin.Context) {
		var input machineRegistration
		if err := c.ShouldBindJSON(&input); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "invalid registration: " + err.Error()})
			return
		}
		input.FrameEndpoint = rewriteFrameEndpoint(c, input.FrameEndpoint)
		// These endpoints are intentionally unauthenticated in this PR.
		m := registry.upsert(input, false)
		c.JSON(http.StatusOK, m.machineInfo)
	})
	r.POST("/api/machines/:id/heartbeat", func(c *gin.Context) {
		var input machineRegistration
		if err := c.ShouldBindJSON(&input); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "invalid heartbeat: " + err.Error()})
			return
		}
		if input.ID != c.Param("id") {
			c.JSON(http.StatusBadRequest, gin.H{"error": "heartbeat ID does not match path"})
			return
		}
		input.FrameEndpoint = rewriteFrameEndpoint(c, input.FrameEndpoint)
		m := registry.upsert(input, false)
		c.JSON(http.StatusOK, m.machineInfo)
	})
	r.POST("/api/machines/:id/deregister", func(c *gin.Context) {
		if !registry.deregister(c.Param("id")) {
			c.JSON(http.StatusNotFound, gin.H{"error": "machine not found"})
			return
		}
		c.Status(http.StatusNoContent)
	})
	r.GET("/api/machines", func(c *gin.Context) {
		c.JSON(http.StatusOK, registry.list())
	})
}

func rewriteFrameEndpoint(c *gin.Context, endpoint string) string {
	host, port, err := net.SplitHostPort(endpoint)
	if err != nil {
		return endpoint
	}
	ip := net.ParseIP(host)
	if ip == nil || (!ip.IsUnspecified() && !ip.IsLoopback()) {
		return endpoint
	}
	clientIP := net.ParseIP(c.ClientIP())
	if clientIP == nil {
		log.Printf(
			"[registry] cannot rewrite frame endpoint %q: registration client IP %q is invalid",
			endpoint,
			c.ClientIP(),
		)
		return endpoint
	}
	rewritten := net.JoinHostPort(clientIP.String(), port)
	log.Printf(
		"[registry] rewriting frame endpoint %q to %q using registration client IP",
		endpoint,
		rewritten,
	)
	return rewritten
}
