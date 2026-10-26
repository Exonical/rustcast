package main

// Clipboard stage 1: text-only clipboard sync between browser and host.
//
// Wire shapes:
//   browser→relay control:  {"type":"clipboard","data":{"mime":..,"data":..}}
//   relay→browser control:  same "clipboard" message, plus "clipboard-status"
//                           {"direction","result","policy"} after each send
//                           and on connect.
//   relay→host cmdClipboard (0x07): [0x07][len u32 BE][JSON {"mime","data"}]
//   host→relay msgClipboard (0x05): [type][ts][len][JSON {"mime","data"}]

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"log"
	"sync"
	"time"
	"unicode/utf8"
)

// clipWireMime is the only MIME type accepted in v1 (the host also advertises
// legacy text aliases internally, but the wire normalizes to this).
const clipWireMime = "text/plain;charset=utf-8"

// clipboardPayload is the JSON body of clipboard frames/commands.
type clipboardPayload struct {
	Mime string `json:"mime"`
	Data string `json:"data"`
}

func sha256Hex(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

// clipboardRate limits host→relay clipboard events to limit per second.
type clipboardRate struct {
	mu          sync.Mutex
	windowStart time.Time
	count       int
}

const clipboardRateLimit = 10 // events per second per upstream

func (r *clipboardRate) allow() bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	if now.Sub(r.windowStart) >= time.Second {
		r.windowStart = now
		r.count = 0
	}
	r.count++
	return r.count <= clipboardRateLimit
}

// clipStatusMessage builds the `clipboard-status` control message the relay
// sends after every client send and on session connect.
func clipStatusMessage(direction, result, policy string) controlMessage {
	return controlMessage{
		Type: "clipboard-status",
		Data: mustJSON(map[string]string{
			"direction": direction,
			"result":    result,
			"policy":    policy,
		}),
	}
}

// clipboardPayloadMessage builds the `clipboard` control message sent to
// viewers carrying host→client clipboard text.
func clipboardPayloadMessage(msg clipboardPayload) controlMessage {
	return controlMessage{
		Type: "clipboard",
		Data: mustJSON(msg),
	}
}

func (s *Session) machineName() string {
	if machine := s.getMachine(); machine != nil && machine.deps != nil && machine.deps.name != nil {
		return machine.deps.name(machine.id)
	}
	return ""
}

func (s *Session) auditFields() (sessionID, userSub, userName, clientIP string) {
	sessionID = s.id
	if s.user != nil {
		userSub = s.user.Sub
		userName = s.user.Name
		if userName == "" {
			userName = s.user.Email
		}
	}
	clientIP = s.clientIP
	return
}

func (s *Session) auditClipboard(direction, mime string, data []byte, result string, capture bool) {
	sessionID, userSub, userName, clientIP := s.auditFields()
	machine := s.getMachine()
	machineID, machineName := "", ""
	if machine != nil {
		machineID = machine.id
		machineName = s.machineName()
	}
	osUser := ""
	if machine != nil {
		osUser = machine.loginOSUser()
	}
	ev := auditEvent{
		MachineID: machineID, MachineName: machineName,
		SessionID: sessionID, UserSub: userSub, UserName: userName,
		ClientIP: clientIP, Direction: direction,
		Mime: mime, Data: data, Result: result, OSUser: osUser,
	}
	if machine != nil && machine.deps != nil && machine.deps.audit != nil {
		machine.deps.audit.recordClipboard(ev, capture)
	} else {
		storeAuditSink{store: s.store}.recordClipboard(ev, capture)
	}
	if machine != nil && machine.metrics != nil && machine.metrics.owner != nil {
		machine.metrics.owner.clipboardEvents.WithLabelValues(machineID, direction, result).Inc()
	}
}

func machinePolicyForClipboardMode(mode string) machinePolicy {
	policy := defaultMachinePolicy()
	policy.Mode = ticketClipboardPolicy(mode)
	return policy
}

func (s *Session) sessionClipboardPolicy(store *store) machinePolicy {
	if s != nil && s.clipboardPolicyOverride != nil {
		return *s.clipboardPolicyOverride
	}
	if s != nil && store != nil {
		if machine := s.getMachine(); machine != nil {
			return store.getPolicy(machine.id)
		}
	}
	return defaultMachinePolicy()
}

// handleClipboard processes a viewer `{"type":"clipboard"}` control message:
// validate, enforce per-machine policy, audit, forward cmdClipboard upstream, and
// always answer with `clipboard-status`.
func (s *Session) handleClipboard(raw json.RawMessage, store *store) error {
	machine := s.getMachine()
	if machine == nil {
		return errors.New("no active machine session")
	}
	policy := s.sessionClipboardPolicy(store)
	status := func(result string) error {
		return s.writeControl(clipStatusMessage(clipDirClientToHost, result, policy.Mode))
	}

	var msg clipboardPayload
	if err := json.Unmarshal(raw, &msg); err != nil || msg.Mime != clipWireMime || !utf8.ValidString(msg.Data) {
		s.auditClipboard(clipDirClientToHost, "", nil, clipResultInvalid, policy.CaptureContent)
		return status(clipResultInvalid)
	}
	data := []byte(msg.Data)
	if int64(len(data)) > policy.MaxBytes {
		s.auditClipboard(clipDirClientToHost, msg.Mime, data, clipResultTooLarge, policy.CaptureContent)
		return status(clipResultTooLarge)
	}
	if !policy.allows(clipDirClientToHost) {
		s.auditClipboard(clipDirClientToHost, msg.Mime, data, clipResultBlocked, policy.CaptureContent)
		return status(clipResultBlocked)
	}
	payload, err := json.Marshal(msg)
	if err != nil {
		return status(clipResultInvalid)
	}
	if !machine.send(commandFrame(cmdClipboard, payload)) {
		log.Printf("[control] upstream command channel full, dropped clipboard set")
	}
	s.auditClipboard(clipDirClientToHost, msg.Mime, data, clipResultOK, policy.CaptureContent)
	return status(clipResultOK)
}

// handleClipboardFrame processes a host→relay msgClipboard frame: rate-limit, parse,
// enforce host_to_client policy, fan out to the machine's viewers as a
// `clipboard` control message, and audit once per delivered viewer (one row
// when there are no viewers or the event is rejected).
func (u *machineUpstream) handleClipboardFrame(data []byte) {
	if u.deps == nil || !u.clipRate.allow() {
		return
	}
	store := u.deps.store
	policy := defaultMachinePolicy()
	if store != nil {
		policy = store.getPolicy(u.id)
	}
	machineName := ""
	if u.deps.name != nil {
		machineName = u.deps.name(u.id)
	}
	osUser := u.loginOSUser()

	// record counts the event once per upstream event and writes one audit
	// row per listed viewer (or a single viewer-less row when empty).
	record := func(direction, result, mime string, payload []byte, viewers []auditEvent) {
		u.metrics.owner.clipboardEvents.WithLabelValues(u.id, direction, result).Inc()
		if len(viewers) == 0 {
			viewers = []auditEvent{{}}
		}
		for _, base := range viewers {
			ev := base
			ev.MachineID, ev.MachineName = u.id, machineName
			ev.OSUser = osUser
			ev.Direction, ev.Mime, ev.Data, ev.Result = direction, mime, payload, result
			if u.deps.audit != nil {
				u.deps.audit.recordClipboard(ev, policy.CaptureContent)
			}
		}
	}

	var msg clipboardPayload
	if err := json.Unmarshal(data, &msg); err != nil || msg.Mime == "" || !utf8.ValidString(msg.Data) {
		record(clipDirHostToClient, clipResultInvalid, "", nil, nil)
		return
	}
	payload := []byte(msg.Data)
	viewers := u.deps.sessions.sessionsFor(u.id)
	if len(viewers) == 0 {
		result := clipResultOK
		switch {
		case int64(len(payload)) > policy.MaxBytes:
			result = clipResultTooLarge
		case !policy.allows(clipDirHostToClient):
			result = clipResultBlocked
		}
		record(clipDirHostToClient, result, msg.Mime, payload, nil)
		return
	}

	control := clipboardPayloadMessage(msg)
	for _, viewer := range viewers {
		sessionPolicy := viewer.sess.sessionClipboardPolicy(store)
		result := clipResultOK
		switch {
		case int64(len(payload)) > sessionPolicy.MaxBytes:
			result = clipResultTooLarge
		case !sessionPolicy.allows(clipDirHostToClient):
			result = clipResultBlocked
		}
		if result == clipResultOK {
			if err := viewer.sess.writeControl(control); err != nil {
				log.Printf("[clipboard] forward to session %s failed: %v", viewer.info.ID, err)
				continue
			}
		}
		u.metrics.owner.clipboardEvents.WithLabelValues(u.id, clipDirHostToClient, result).Inc()
		ev := auditEvent{
			MachineID: u.id, MachineName: machineName, OSUser: osUser,
			SessionID: viewer.info.ID, UserSub: viewer.info.UserSub,
			UserName: viewer.info.User, ClientIP: viewer.info.ClientIP,
			Direction: clipDirHostToClient, Mime: msg.Mime, Data: payload, Result: result,
		}
		if u.deps.audit != nil {
			u.deps.audit.recordClipboard(ev, sessionPolicy.CaptureContent)
		}
	}
}
