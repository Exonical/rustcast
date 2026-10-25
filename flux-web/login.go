package main

// Per-machine host login state machine (flux-hostd wire login). The host
// daemon gates the frame stream behind a PAM conversation carried in 0x06
// frames (prompts/results out) and 0x08 commands (responses in). Exactly one
// viewer performs the login at a time; while active the machine is bound to
// that viewer's relay identity.

import (
	"encoding/binary"
	"encoding/json"
	"log"
	"os"
)

type loginState int

const (
	loginIdle loginState = iota
	loginAuthenticating
	loginActive
)

func (s loginState) String() string {
	switch s {
	case loginAuthenticating:
		return "authenticating"
	case loginActive:
		return "active"
	default:
		return "idle"
	}
}

// loginInfo describes who is authenticating / which OS login is active.
type loginInfo struct {
	state     loginState
	sessionID string // authenticating or owning viewer session
	authSess  *Session
	osUser    string
	osUID     int64
	relaySub  string
	relayName string
}

// loginFrameJSON is the hostd 0x06 frame payload.
type loginFrameJSON struct {
	State      string `json:"state"`
	Style      string `json:"style,omitempty"`
	Message    string `json:"message,omitempty"`
	User       string `json:"user,omitempty"`
	UID        int64  `json:"uid,omitempty"`
	Reattached bool   `json:"reattached,omitempty"`
}

// loginPrefill controls whether the relay answers the host's "Username"
// prompt itself from the viewer's OIDC preferred_username. FLUX_LOGIN_PREFILL=false
// disables it (used to test the devuser local-PAM path).
var loginPrefill = os.Getenv("FLUX_LOGIN_PREFILL") != "false"

const usernamePrompt = "Username"

// gateViewer decides whether a new WebTransport session may attach.
// Returns role "auth" (this viewer performs the login), "viewer" (already
// logged in, same relay identity), or a non-empty reject reason.
func (u *machineUpstream) gateViewer(sess *Session) (role, reason string) {
	u.loginMu.Lock()
	defer u.loginMu.Unlock()
	sub := ""
	if sess.user != nil {
		sub = sess.user.Sub
	}
	if !u.supportsLogin && !u.loginSpoken {
		// Upstream is an old-style agent without the login protocol; no gate.
		return "viewer", ""
	}
	switch u.login.state {
	case loginAuthenticating:
		return "", "login in progress"
	case loginActive:
		if u.login.relaySub == "" || sub != u.login.relaySub {
			return "", "in use by " + u.login.osUser
		}
		return "viewer", ""
	default:
		name := ""
		if sess.user != nil {
			name = sess.user.Name
		}
		u.login = loginInfo{
			state:     loginAuthenticating,
			sessionID: sess.id,
			authSess:  sess,
			relaySub:  sub,
			relayName: name,
		}
		return "auth", ""
	}
}

// loginSnapshot returns a copy of the current login state for APIs.
func (u *machineUpstream) loginSnapshot() loginInfo {
	u.loginMu.Lock()
	defer u.loginMu.Unlock()
	l := u.login
	l.authSess = nil
	return l
}

func (u *machineUpstream) loginOSUser() string {
	u.loginMu.Lock()
	defer u.loginMu.Unlock()
	if u.login.state == loginActive {
		return u.login.osUser
	}
	return ""
}

func (u *machineUpstream) sendLoginResponse(resp string) {
	data, _ := json.Marshal(map[string]string{"response": resp})
	cmd := make([]byte, 5+len(data))
	cmd[0] = 0x08
	binary.BigEndian.PutUint32(cmd[1:5], uint32(len(data)))
	copy(cmd[5:], data)
	u.send(cmd)
}

// sendLoginResponseIfAuth forwards a viewer's login answer only when that
// viewer owns the login exchange.
func (u *machineUpstream) sendLoginResponseIfAuth(sess *Session, resp string) {
	u.loginMu.Lock()
	allowed := u.login.state == loginAuthenticating && u.login.sessionID == sess.id
	u.loginMu.Unlock()
	if allowed {
		u.sendLoginResponse(resp)
	}
}

func (u *machineUpstream) sendLock() {
	u.send([]byte{0x09, 0, 0, 0, 0})
}

// viewerLock handles a viewer {"type":"lock"}: only the logged-in relay
// identity may lock; the locking viewer becomes the next authenticating
// viewer so it sees the new Username prompt on the same connection.
func (u *machineUpstream) viewerLock(sess *Session) {
	u.loginMu.Lock()
	if u.login.state != loginActive {
		u.loginMu.Unlock()
		return
	}
	sub := ""
	if sess.user != nil {
		sub = sess.user.Sub
	}
	if sub == "" || sub != u.login.relaySub {
		u.loginMu.Unlock()
		return
	}
	u.login = loginInfo{
		state:     loginAuthenticating,
		sessionID: sess.id,
		authSess:  sess,
		relaySub:  sub,
		relayName: u.login.relayName,
	}
	u.loginMu.Unlock()
	u.sendLock()
}

// onSessionLeft is called when a session is released. An authenticating
// viewer leaving frees the login slot; the last viewer of an active login
// locks the host.
func (u *machineUpstream) onSessionLeft(sess *Session) {
	u.loginMu.Lock()
	switch u.login.state {
	case loginAuthenticating:
		if u.login.sessionID == sess.id {
			u.login = loginInfo{}
		}
	case loginActive:
		left := true
		if u.deps != nil && u.deps.sessions != nil {
			for _, v := range u.deps.sessions.sessionsFor(u.id) {
				if v.info.ID != sess.id {
					left = false
				}
			}
		}
		if left {
			u.login = loginInfo{state: loginIdle}
			u.loginMu.Unlock()
			u.sendLock()
			return
		}
	}
	u.loginMu.Unlock()
}

// onUpstreamDisconnected resets login state and ends attached viewers.
func (u *machineUpstream) onUpstreamDisconnected() {
	u.loginMu.Lock()
	u.login = loginInfo{}
	u.lastPrompt = nil
	u.loginSpoken = false
	u.loginMu.Unlock()
	if u.deps != nil && u.deps.sessions != nil {
		for _, v := range u.deps.sessions.sessionsFor(u.id) {
			u.deps.sessions.endByID(v.info.ID, "host connection lost")
		}
	}
}

// insertHostLogin writes a host_logins audit row for the authenticating
// viewer (or an empty viewer context when none).
func (u *machineUpstream) insertHostLogin(sess *Session, osUser, result string) {
	if u.deps == nil || u.deps.store == nil {
		return
	}
	ev := hostLogin{
		MachineID: u.id,
		OSUser:    osUser,
		Result:    result,
	}
	if u.deps.name != nil {
		ev.MachineName = u.deps.name(u.id)
	}
	if sess != nil {
		ev.SessionID, ev.UserSub, ev.UserName, ev.ClientIP = sess.auditFields()
	}
	if err := u.deps.store.insertLogin(ev); err != nil {
		log.Printf("[login] audit insert failed machine=%s: %v", u.id, err)
	}
}

// handleLoginFrame processes a hostd 0x06 frame.
func (u *machineUpstream) handleLoginFrame(data []byte) {
	var f loginFrameJSON
	if err := json.Unmarshal(data, &f); err != nil {
		log.Printf("[frame:%s] invalid login frame: %v", u.id, err)
		return
	}
	u.loginMu.Lock()
	u.loginSpoken = true
	u.loginMu.Unlock()
	switch f.State {
	case "prompt":
		u.loginMu.Lock()
		p := f
		u.lastPrompt = &p
		// A viewer that attached before the first prompt arrived (or after a
		// lock) is promoted to the authenticating viewer.
		if u.login.authSess == nil && u.login.state == loginIdle {
			if cur := u.currentSession(); cur != nil {
				sub, name := "", ""
				if cur.user != nil {
					sub, name = cur.user.Sub, cur.user.Name
				}
				u.login = loginInfo{
					state: loginAuthenticating, sessionID: cur.id,
					authSess: cur, relaySub: sub, relayName: name,
				}
			}
		}
		sess := u.login.authSess
		u.loginMu.Unlock()
		if sess == nil {
			return
		}
		// Username prefill: answer from the viewer's OIDC preferred_username
		// instead of asking (falls back to the viewer when empty).
		if f.Message == usernamePrompt && loginPrefill {
			if sess.user != nil && sess.user.Username != "" {
				log.Printf("[frame:%s] auto-answering Username prompt as %q", u.id, sess.user.Username)
				u.sendLoginResponse(sess.user.Username)
				return
			}
		}
		u.forwardLogin(sess, "prompt", data)
	case "ok":
		u.loginMu.Lock()
		sess := u.login.authSess
		prev := u.login
		u.login = loginInfo{
			state:     loginActive,
			sessionID: prev.sessionID,
			osUser:    f.User,
			osUID:     f.UID,
			relaySub:  prev.relaySub,
			relayName: prev.relayName,
		}
		u.loginMu.Unlock()
		if u.deps != nil && u.deps.sessions != nil && sess != nil {
			u.deps.sessions.setOSInfo(sess.id, f.User, f.UID)
		}
		u.insertHostLogin(sess, f.User, "ok")
		log.Printf("[frame:%s] os login ok user=%s uid=%d reattached=%v", u.id, f.User, f.UID, f.Reattached)
		if sess == nil {
			return
		}
		u.forwardLogin(sess, "ok", data)
		// The deferred frame-flow start for the authenticating viewer.
		sess.deferredStart(u)
	case "failed":
		u.loginMu.Lock()
		sess := u.login.authSess
		u.loginMu.Unlock()
		u.insertHostLogin(sess, f.User, "failed")
		if sess != nil {
			u.forwardLogin(sess, "failed", data)
		}
	case "busy":
		u.loginMu.Lock()
		sess := u.login.authSess
		u.login = loginInfo{state: loginIdle}
		u.loginMu.Unlock()
		u.insertHostLogin(sess, f.User, "busy")
		if sess != nil {
			u.forwardLogin(sess, "busy", data)
			if u.deps != nil && u.deps.sessions != nil {
				u.deps.sessions.endByID(sess.id, "host in use by "+f.User)
			}
		}
	default:
		log.Printf("[frame:%s] unknown login state %q", u.id, f.State)
	}
}

// replayPrompt re-sends the last seen login prompt to a new authenticating
// viewer (e.g. hostd sent "Username" before any viewer connected).
func (u *machineUpstream) replayPrompt(sess *Session) {
	u.loginMu.Lock()
	p := u.lastPrompt
	u.loginMu.Unlock()
	if p == nil {
		return
	}
	data, err := json.Marshal(p)
	if err != nil {
		return
	}
	if p.Message == usernamePrompt && loginPrefill {
		if sess.user != nil && sess.user.Username != "" {
			u.sendLoginResponse(sess.user.Username)
			return
		}
	}
	u.forwardLogin(sess, "prompt replay", data)
}

// forwardLogin sends a hostd login frame to the viewer as a "login" control
// message, logging (not failing) when the control stream write fails.
func (u *machineUpstream) forwardLogin(sess *Session, what string, data []byte) {
	if err := sess.writeControl(controlMessage{Type: "login", Data: data}); err != nil {
		log.Printf("[frame:%s] login %s forward failed: %v", u.id, what, err)
	}
}
