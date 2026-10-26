package main

import (
	"encoding/base64"
	"fmt"
	"log"
	"net"
	"net/http"
	"strconv"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/gin-gonic/gin"
	"github.com/quic-go/quic-go"
)

var errMachineInUse = &machineError{"machine already in use"}

const machineExpiry = 15 * time.Second

type machineInfo struct {
	ID              string    `json:"id"`
	Name            string    `json:"name"`
	DisplayName     string    `json:"display_name,omitempty"`
	FrameEndpoint   string    `json:"frame_endpoint"`
	OS              string    `json:"os,omitempty"`
	GPUVendor       string    `json:"gpu_vendor,omitempty"`
	EncoderBackend  string    `json:"encoder_backend,omitempty"`
	VirtualDisplay  bool      `json:"virtual_display"`
	Width           uint32    `json:"width,omitempty"`
	Height          uint32    `json:"height,omitempty"`
	TargetFPS       uint32    `json:"target_fps,omitempty"`
	DefaultFPS      uint32    `json:"default_fps,omitempty"`
	TargetBitrate   uint32    `json:"target_bitrate_kbps,omitempty"`
	Viewers         int       `json:"viewers"`
	ClipboardPolicy string    `json:"clipboard_policy"`
	LoggedInUser    string    `json:"logged_in_user"`
	LoginState      string    `json:"login_state"`
	QuicCertSHA256  string    `json:"quic_cert_sha256,omitempty"`
	HostDialsOut    bool      `json:"host_dials_out,omitempty"`
	SupportsLogin   bool      `json:"supports_login"`
	LastSeen        time.Time `json:"last_seen"`
	Status          string    `json:"status"`
	UpstreamStatus  string    `json:"upstream_status"`
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
	DefaultFPS     uint32 `json:"default_fps"`
	TargetBitrate  uint32 `json:"target_bitrate_kbps"`
	SupportsLogin  bool   `json:"supports_login"`
	LoginState     string `json:"login_state"`
	LoggedInUser   string `json:"logged_in_user"`
	QuicCertSHA256 string `json:"quic_cert_sha256"`
	HostDialsOut   bool   `json:"host_dials_out"`
}

type machineRecord struct {
	machineInfo
	static   bool
	upstream *machineUpstream
}

// relayDeps carries the shared clipboard dependencies (policy/audit store and
// the session registry for viewer fan-out) into lazily created upstreams.
type relayDeps struct {
	store    *store
	sessions *sessionRegistry
	name     func(machineID string) string
}

type machineRegistry struct {
	mu        sync.Mutex
	machines  map[string]*machineRecord
	metrics   *relayMetrics
	deps      *relayDeps
	hostLinks *hostLinkHub
}

// attachDeps wires clipboard persistence/fan-out dependencies; called once
// from main (and from tests that exercise clipboard paths).
func (r *machineRegistry) attachDeps(d *relayDeps) {
	r.mu.Lock()
	r.deps = d
	r.mu.Unlock()
}

func newMachineRegistry() *machineRegistry {
	metrics := newRelayMetrics()
	return &machineRegistry{
		machines:  make(map[string]*machineRecord),
		metrics:   metrics,
		hostLinks: newHostLinkHub(metrics.hostLinks),
	}
}

func (r *machineRegistry) metricMachineID(id string) string {
	r.mu.Lock()
	defer r.mu.Unlock()
	if _, ok := r.machines[id]; ok {
		return id
	}
	return "unknown"
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
	now := time.Now()
	m := r.machines[input.ID]
	if m == nil {
		m = &machineRecord{}
		r.machines[input.ID] = m
	}
	dropHostLink := m.HostDialsOut && !input.HostDialsOut
	var hostLink *quic.Conn
	if dropHostLink && r.hostLinks != nil {
		hostLink = r.hostLinks.peek(input.ID)
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
		TargetFPS: input.TargetFPS, DefaultFPS: input.DefaultFPS, LastSeen: now, Status: "online",
		UpstreamStatus: upstreamStatus,
		SupportsLogin:  input.SupportsLogin, LoginState: input.LoginState,
		LoggedInUser: input.LoggedInUser, QuicCertSHA256: input.QuicCertSHA256,
		HostDialsOut: input.HostDialsOut,
	}
	m.static = static
	if m.upstream != nil {
		m.upstream.setQuicCertSHA256(input.QuicCertSHA256)
	}
	if m.upstream != nil && input.TargetBitrate != 0 {
		m.upstream.abr.setSenderTarget(input.TargetBitrate)
	}
	r.mu.Unlock()
	if dropHostLink && r.hostLinks != nil {
		r.hostLinks.dropIf(input.ID, hostLink)
	}
	return m
}

func (r *machineRegistry) get(id string) (*machineRecord, error) {
	r.mu.Lock()
	m := r.machines[id]
	if m == nil {
		r.mu.Unlock()
		return nil, errMachineNotFound
	}
	expired := !m.static && time.Since(m.LastSeen) > machineExpiry
	var stale *machineUpstream
	var hostLink *quic.Conn
	if expired {
		m.Status = "offline"
		stale = m.upstream
		if stale != nil {
			m.upstream = nil
		}
		if r.hostLinks != nil {
			hostLink = r.hostLinks.peek(id)
		}
	}
	if m.Status != "online" {
		r.mu.Unlock()
		if stale != nil {
			stale.stop()
		}
		if expired && r.hostLinks != nil {
			r.hostLinks.dropIf(id, hostLink)
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
	expiredHostLinks := make(map[string]*quic.Conn)
	for _, m := range r.machines {
		if !m.static && time.Since(m.LastSeen) > machineExpiry {
			m.Status = "offline"
			if m.upstream != nil {
				stale = append(stale, m.upstream)
				m.upstream = nil
			}
			if r.hostLinks != nil {
				expiredHostLinks[m.ID] = r.hostLinks.peek(m.ID)
			}
		}
		info := m.machineInfo
		if m.upstream != nil {
			info.Viewers = m.upstream.viewers
			ls := m.upstream.loginSnapshot()
			info.LoginState = ls.state.String()
			info.LoggedInUser = ls.osUser
		} else {
			// No live upstream: report what the last registration carried.
			info.LoginState = m.LoginState
			if info.LoginState == "" {
				info.LoginState = loginIdle.String()
			}
			info.LoggedInUser = m.LoggedInUser
		}
		if r.deps != nil && r.deps.store != nil {
			info.ClipboardPolicy = r.deps.store.getPolicy(info.ID).Mode
		} else {
			info.ClipboardPolicy = clipModeBoth
		}
		result = append(result, info)
	}
	r.mu.Unlock()
	for _, upstream := range stale {
		upstream.stop()
	}
	if r.hostLinks != nil {
		for id, conn := range expiredHostLinks {
			r.hostLinks.dropIf(id, conn)
		}
	}
	return result
}

func (r *machineRegistry) displayName(id string) string {
	r.mu.Lock()
	defer r.mu.Unlock()
	m := r.machines[id]
	if m == nil {
		return ""
	}
	if m.DisplayName != "" {
		return m.DisplayName
	}
	return m.Name
}

func (r *machineRegistry) deregister(id string) bool {
	r.mu.Lock()
	m := r.machines[id]
	if m == nil || m.static {
		r.mu.Unlock()
		return false
	}
	upstream := m.upstream
	var hostLink *quic.Conn
	if r.hostLinks != nil {
		hostLink = r.hostLinks.peek(id)
	}
	delete(r.machines, id)
	r.mu.Unlock()
	if r.hostLinks != nil {
		r.hostLinks.dropIf(id, hostLink)
	}
	if upstream != nil {
		upstream.stop()
	} else {
		r.metrics.deleteMachine(id)
	}
	return true
}

func (r *machineRegistry) acquire(id string) (*machineUpstream, error) {
	m, err := r.get(id)
	if err != nil {
		return nil, err
	}
	r.mu.Lock()
	if m.upstream != nil && m.upstream.viewers > 0 {
		r.mu.Unlock()
		return nil, errMachineInUse
	}
	if m.upstream == nil {
		machineMetrics := r.metrics.machine(m.ID)
		m.upstream = newMachineUpstream(m.FrameEndpoint, m.ID, r.deps, func(status string) {
			r.setUpstreamStatus(m.ID, status)
		}, machineMetrics)
		m.upstream.supportsLogin = m.SupportsLogin
		m.upstream.quicCertSHA256 = m.QuicCertSHA256
		if m.HostDialsOut {
			m.upstream.hostLinks = r.hostLinks
		}
		if m.TargetBitrate != 0 {
			m.upstream.abr.setSenderTarget(m.TargetBitrate)
		}
		go m.upstream.run()
	}
	m.upstream.viewers++
	upstream := m.upstream
	viewerCount := m.upstream.viewers
	r.mu.Unlock()
	if !upstream.sendViewerCount(viewerCount) {
		log.Printf("[frame:%s] upstream command channel full, dropped viewer count %d", upstream.id, viewerCount)
	}
	return upstream, nil
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
	viewerCount := m.viewers
	r.mu.Unlock()
	if !m.sendViewerCount(viewerCount) {
		log.Printf("[frame:%s] upstream command channel full, dropped viewer count %d", m.id, viewerCount)
	}
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

// registerMachineRoutes mounts viewer APIs under api (user-gated) and the
// host-facing registration APIs under machines (bearer-token gated) so the
// two auth schemes don't stack.
func registerMachineRoutes(api, machines *gin.RouterGroup, registry *machineRegistry) {
	machines.POST("/machines/register", func(c *gin.Context) {
		var input machineRegistration
		if err := c.ShouldBindJSON(&input); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "invalid registration: " + err.Error()})
			return
		}
		input.FrameEndpoint = rewriteFrameEndpoint(c, input.FrameEndpoint)
		m := registry.upsert(input, false)
		c.JSON(http.StatusOK, m.machineInfo)
	})
	machines.POST("/machines/:id/heartbeat", func(c *gin.Context) {
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
	machines.POST("/machines/:id/deregister", func(c *gin.Context) {
		if !registry.deregister(c.Param("id")) {
			c.JSON(http.StatusNotFound, gin.H{"error": "machine not found"})
			return
		}
		c.Status(http.StatusNoContent)
	})
	api.GET("/machines", func(c *gin.Context) {
		c.JSON(http.StatusOK, registry.list())
	})
}

// registerPolicyRoutes exposes the admin-only clipboard policy + audit API.
// Every handler is a 503 when the store was never attached (misdeployment).
func registerPolicyRoutes(admin *gin.RouterGroup, registry *machineRegistry, auth *authService) {
	admin.GET("/machines/:id/policy", func(c *gin.Context) {
		st := registry.depsStore()
		if st == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "clipboard store unavailable"})
			return
		}
		c.JSON(http.StatusOK, st.getPolicy(c.Param("id")))
	})

	admin.PUT("/machines/:id/policy", func(c *gin.Context) {
		st := registry.depsStore()
		if st == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "clipboard store unavailable"})
			return
		}
		var p machinePolicy
		if err := c.ShouldBindJSON(&p); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "invalid policy body"})
			return
		}
		updatedBy := "unknown"
		if user := auth.userFrom(c); user != nil {
			updatedBy = user.Sub
			if user.Name != "" {
				updatedBy = fmt.Sprintf("%s (%s)", user.Sub, user.Name)
			}
		}
		if err := st.putPolicy(c.Param("id"), p, updatedBy); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}
		c.JSON(http.StatusOK, st.getPolicy(c.Param("id")))
	})

	admin.GET("/audit/clipboard", func(c *gin.Context) {
		st := registry.depsStore()
		if st == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "clipboard store unavailable"})
			return
		}
		limit, _ := strconv.ParseInt(c.DefaultQuery("limit", "100"), 10, 64)
		before, _ := strconv.ParseInt(c.Query("before"), 10, 64)
		rows, err := st.listAudit(c.Query("machine"), limit, before)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "audit query failed"})
			return
		}
		c.JSON(http.StatusOK, rows)
	})

	admin.GET("/audit/logins", func(c *gin.Context) {
		st := registry.depsStore()
		if st == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "audit store unavailable"})
			return
		}
		limit, _ := strconv.ParseInt(c.DefaultQuery("limit", "100"), 10, 64)
		before, _ := strconv.ParseInt(c.Query("before"), 10, 64)
		rows, err := st.listLogins(c.Query("machine"), limit, before)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "audit query failed"})
			return
		}
		c.JSON(http.StatusOK, rows)
	})

	admin.GET("/audit/clipboard/:id", func(c *gin.Context) {
		st := registry.depsStore()
		if st == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "clipboard store unavailable"})
			return
		}
		id, err := strconv.ParseInt(c.Param("id"), 10, 64)
		if err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": "invalid audit id"})
			return
		}
		row, content, err := st.getAuditContent(id)
		if err != nil {
			c.JSON(http.StatusNotFound, gin.H{"error": "audit row not found"})
			return
		}
		resp := gin.H{"row": row}
		switch {
		case len(content) == 0:
			resp["content"] = nil
		case utf8.Valid(content):
			resp["content"] = string(content)
			resp["encoding"] = "utf-8"
		default:
			resp["content"] = base64.StdEncoding.EncodeToString(content)
			resp["encoding"] = "base64"
		}
		c.JSON(http.StatusOK, resp)
	})
}

func (r *machineRegistry) depsStore() *store {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.deps == nil {
		return nil
	}
	return r.deps.store
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
