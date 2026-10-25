package main

import (
	"context"
	"crypto"
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
)

func passthroughAuth(c *gin.Context) { c.Next() }

func disabledAuth(t *testing.T) *authService {
	t.Helper()
	a, err := newAuthService(context.Background(), authConfig{})
	if err != nil {
		t.Fatal(err)
	}
	return a
}

func testIdentity() *identity {
	return &identity{
		Sub: "user-1", Email: "u@example.com", Name: "Test User",
		Admin: true, Expires: time.Now().Add(time.Hour),
	}
}

func sessionCookieValue(t *testing.T, a *authService, user *identity) string {
	t.Helper()
	payload, _ := json.Marshal(user)
	sealed, err := a.seal(payload)
	if err != nil {
		t.Fatal(err)
	}
	return sealed
}

func TestCookieSealRoundTripAndTamper(t *testing.T) {
	a := disabledAuth(t)
	payload := []byte(`{"sub":"x"}`)
	sealed, err := a.seal(payload)
	if err != nil {
		t.Fatal(err)
	}
	out, err := a.open(sealed)
	if err != nil {
		t.Fatal(err)
	}
	if string(out) != string(payload) {
		t.Fatalf("round trip mismatch: %q", out)
	}
	raw, _ := base64.RawURLEncoding.DecodeString(sealed)
	raw[len(raw)-1] ^= 0xff
	if _, err := a.open(base64.RawURLEncoding.EncodeToString(raw)); err == nil {
		t.Fatal("tampered cookie decrypted")
	}
	if _, err := a.open("not-a-cookie"); err == nil {
		t.Fatal("garbage cookie decrypted")
	}
}

func TestDisabledAuthGrantsAnonymousAdmin(t *testing.T) {
	gin.SetMode(gin.TestMode)
	a := disabledAuth(t)
	r := gin.New()
	r.GET("/api/me", a.requireUser(), func(c *gin.Context) {
		user := a.userFrom(c)
		c.JSON(http.StatusOK, gin.H{"sub": user.Sub, "admin": user.Admin})
	})
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/me", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("expected 200, got %d", w.Code)
	}
	var body struct {
		Sub   string `json:"sub"`
		Admin bool   `json:"admin"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if body.Sub != "anonymous" || !body.Admin {
		t.Fatalf("unexpected identity %+v", body)
	}
}

func TestRequireUserRejectsWithoutSession(t *testing.T) {
	gin.SetMode(gin.TestMode)
	a := &authService{enabled: true}
	block, err := newTestAEAD()
	if err != nil {
		t.Fatal(err)
	}
	a.aead = block
	r := gin.New()
	r.GET("/api/machines", a.requireUser(), func(c *gin.Context) { c.Status(200) })
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/api/machines", nil))
	if w.Code != http.StatusUnauthorized {
		t.Fatalf("expected 401, got %d", w.Code)
	}
	if !strings.Contains(w.Body.String(), `"login":"/auth/login"`) {
		t.Fatalf("missing login hint: %s", w.Body)
	}
}

func TestRequireAdminRejectsNonAdmin(t *testing.T) {
	gin.SetMode(gin.TestMode)
	a := &authService{enabled: true}
	block, err := newTestAEAD()
	if err != nil {
		t.Fatal(err)
	}
	a.aead = block
	r := gin.New()
	r.POST("/api/sessions/:id/disconnect", a.requireUser(), a.requireAdmin(), func(c *gin.Context) { c.Status(204) })

	nonAdmin := testIdentity()
	nonAdmin.Admin = false
	req := httptest.NewRequest(http.MethodPost, "/api/sessions/x/disconnect", nil)
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, nonAdmin)})
	w := httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusForbidden {
		t.Fatalf("expected 403 for non-admin, got %d", w.Code)
	}

	req = httptest.NewRequest(http.MethodPost, "/api/sessions/x/disconnect", nil)
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, testIdentity())})
	w = httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusNoContent {
		t.Fatalf("expected 204 for admin, got %d", w.Code)
	}
}

func TestExpiredSessionRejected(t *testing.T) {
	gin.SetMode(gin.TestMode)
	a := &authService{enabled: true}
	block, err := newTestAEAD()
	if err != nil {
		t.Fatal(err)
	}
	a.aead = block
	r := gin.New()
	r.GET("/api/me", a.requireUser(), func(c *gin.Context) { c.Status(200) })
	expired := testIdentity()
	expired.Expires = time.Now().Add(-time.Hour)
	req := httptest.NewRequest(http.MethodGet, "/api/me", nil)
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, expired)})
	w := httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusUnauthorized {
		t.Fatalf("expected 401 for expired session, got %d", w.Code)
	}
}

func TestMachineTokenBearerCheck(t *testing.T) {
	gin.SetMode(gin.TestMode)
	a := &authService{enabled: true, cfg: authConfig{relayToken: "secret-token"}}
	r := gin.New()
	r.POST("/api/machines/register", a.requireMachineToken(), func(c *gin.Context) { c.Status(200) })

	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodPost, "/api/machines/register", nil))
	if w.Code != http.StatusUnauthorized {
		t.Fatalf("expected 401 without token, got %d", w.Code)
	}

	req := httptest.NewRequest(http.MethodPost, "/api/machines/register", nil)
	req.Header.Set("Authorization", "Bearer wrong")
	w = httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusUnauthorized {
		t.Fatalf("expected 401 with wrong token, got %d", w.Code)
	}

	req = httptest.NewRequest(http.MethodPost, "/api/machines/register", nil)
	req.Header.Set("Authorization", "Bearer secret-token")
	w = httptest.NewRecorder()
	r.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("expected 200 with token, got %d", w.Code)
	}
}

func TestTicketSingleUseAndExpiry(t *testing.T) {
	a := &authService{enabled: true, tickets: make(map[string]*wtTicket)}
	user := testIdentity()
	ticket := a.issueTicket(user)
	if got := a.consumeTicket(ticket); got != user {
		t.Fatal("first consume should return identity")
	}
	if got := a.consumeTicket(ticket); got != nil {
		t.Fatal("second consume must fail")
	}
	a.tickets["expired"] = &wtTicket{user: user, expires: time.Now().Add(-time.Second)}
	if got := a.consumeTicket("expired"); got != nil {
		t.Fatal("expired ticket must fail")
	}
	if got := a.consumeTicket("unknown"); got != nil {
		t.Fatal("unknown ticket must fail")
	}
}

func TestLoginNextSanitization(t *testing.T) {
	for _, tc := range []struct{ in, want string }{
		{"/machines", "/machines"},
		{"", "/"},
		{"https://evil.example/", "/"},
		{"//evil.example/", "/"},
		{"javascript:alert(1)", "/"},
	} {
		if got := sanitizeNext(tc.in); got != tc.want {
			t.Fatalf("sanitizeNext(%q)=%q want %q", tc.in, got, tc.want)
		}
	}
}

// --- fake OIDC issuer ---

type fakeIssuer struct {
	server   *httptest.Server
	key      *rsa.PrivateKey
	lastCode string
	nonce    string
}

func b64url(b []byte) string { return base64.RawURLEncoding.EncodeToString(b) }

func newFakeIssuer(t *testing.T) *fakeIssuer {
	t.Helper()
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	f := &fakeIssuer{key: key}
	mux := http.NewServeMux()
	f.server = httptest.NewServer(mux)
	mux.HandleFunc("/.well-known/openid-configuration", func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{
			"issuer":                 f.server.URL,
			"authorization_endpoint": f.server.URL + "/authorize",
			"token_endpoint":         f.server.URL + "/token",
			"jwks_uri":               f.server.URL + "/jwks",
			"end_session_endpoint":   f.server.URL + "/endsession",
		})
	})
	mux.HandleFunc("/jwks", func(w http.ResponseWriter, r *http.Request) {
		n := key.PublicKey.N.Bytes()
		e := big.NewInt(int64(key.PublicKey.E)).Bytes()
		json.NewEncoder(w).Encode(map[string]any{
			"keys": []map[string]any{{
				"kty": "RSA", "kid": "test", "use": "sig", "alg": "RS256",
				"n": b64url(n), "e": b64url(e),
			}},
		})
	})
	mux.HandleFunc("/token", func(w http.ResponseWriter, r *http.Request) {
		if err := r.ParseForm(); err != nil {
			w.WriteHeader(400)
			return
		}
		if r.Form.Get("code_verifier") == "" {
			w.WriteHeader(400)
			json.NewEncoder(w).Encode(map[string]string{"error": "missing pkce verifier"})
			return
		}
		f.lastCode = r.Form.Get("code")
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{
			"access_token": "at", "token_type": "Bearer", "expires_in": 3600,
			"id_token": f.idToken(f.nonce),
		})
	})
	t.Cleanup(f.server.Close)
	return f
}

func (f *fakeIssuer) idToken(nonce string) string {
	header := b64url([]byte(`{"alg":"RS256","kid":"test","typ":"JWT"}`))
	claims, _ := json.Marshal(map[string]any{
		"iss":                f.server.URL,
		"sub":                "authentik-user-1",
		"aud":                "flux-relay",
		"exp":                time.Now().Add(time.Hour).Unix(),
		"iat":                time.Now().Unix(),
		"nonce":              nonce,
		"email":              "akadmin@example.com",
		"preferred_username": "akadmin",
		"groups":             []string{"flux-admins"},
	})
	unsigned := header + "." + b64url(claims)
	digest := sha256.Sum256([]byte(unsigned))
	sig, _ := rsa.SignPKCS1v15(rand.Reader, f.key, crypto.SHA256, digest[:])
	return unsigned + "." + b64url(sig)
}

func testAuthConfig(issuer string) authConfig {
	return authConfig{
		issuer: issuer, clientID: "flux-relay", clientSecret: "s3cret",
		adminGroup:    "flux-admins",
		scopes:        []string{"openid", "profile", "email", "groups"},
		sessionSecret: []byte("0123456789abcdef0123456789abcdef"),
	}
}

// Full auth-code + PKCE round trip against the fake issuer.
func TestOIDCFlowEndToEnd(t *testing.T) {
	fake := newFakeIssuer(t)
	gin.SetMode(gin.TestMode)
	a, err := newAuthService(context.Background(), testAuthConfig(fake.server.URL))
	if err != nil {
		t.Fatal(err)
	}
	r := gin.New()
	a.registerRoutes(r)
	r.GET("/api/me", a.requireUser(), func(c *gin.Context) {
		user := a.userFrom(c)
		c.JSON(200, gin.H{"sub": user.Sub, "name": user.Name, "admin": user.Admin})
	})
	server := httptest.NewServer(r)
	defer server.Close()
	client := &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error {
		return http.ErrUseLastResponse
	}}

	// /auth/login → 302 to the authorize endpoint with PKCE + state.
	resp, err := client.Get(server.URL + "/auth/login?next=/sessions/")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusFound {
		t.Fatalf("login expected 302, got %d", resp.StatusCode)
	}
	authorize, err := url.Parse(resp.Header.Get("Location"))
	if err != nil {
		t.Fatal(err)
	}
	if authorize.Path != "/authorize" {
		t.Fatalf("unexpected authorize URL %s", authorize)
	}
	q := authorize.Query()
	if q.Get("client_id") != "flux-relay" || q.Get("code_challenge") == "" || q.Get("state") == "" || q.Get("nonce") == "" {
		t.Fatalf("authorize query missing fields: %s", q)
	}
	state := q.Get("state")
	nonce := q.Get("nonce")
	fake.nonce = nonce
	var oauthCookie *http.Cookie
	for _, cookie := range resp.Cookies() {
		if cookie.Name == oauthCookieName {
			oauthCookie = cookie
		}
	}
	if oauthCookie == nil {
		t.Fatal("missing oauth state cookie")
	}

	// Simulate the IdP redirect back with a code.
	callbackReq, _ := http.NewRequest(http.MethodGet,
		server.URL+"/auth/callback?state="+url.QueryEscape(state)+"&code=fake-code&nonce="+url.QueryEscape(nonce), nil)
	callbackReq.AddCookie(oauthCookie)
	resp, err = client.Do(callbackReq)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusFound {
		t.Fatalf("callback expected 302, got %d", resp.StatusCode)
	}
	if loc := resp.Header.Get("Location"); loc != "/sessions/" {
		t.Fatalf("expected redirect to /sessions/, got %q", loc)
	}
	var session *http.Cookie
	for _, cookie := range resp.Cookies() {
		if cookie.Name == sessionCookieName {
			session = cookie
		}
	}
	if session == nil {
		t.Fatal("missing session cookie")
	}

	// The session cookie unlocks /api/me with admin identity.
	req, _ := http.NewRequest(http.MethodGet, server.URL+"/api/me", nil)
	req.AddCookie(session)
	resp, err = client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var me struct {
		Sub   string `json:"sub"`
		Name  string `json:"name"`
		Admin bool   `json:"admin"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&me); err != nil {
		t.Fatal(err)
	}
	if me.Sub != "authentik-user-1" || me.Name != "akadmin" || !me.Admin {
		t.Fatalf("unexpected identity %+v", me)
	}

	// State mismatch is rejected.
	badReq, _ := http.NewRequest(http.MethodGet, server.URL+"/auth/callback?state=wrong&code=x", nil)
	badReq.AddCookie(oauthCookie)
	resp, err = client.Do(badReq)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("state mismatch expected 400, got %d", resp.StatusCode)
	}

	// An id_token carrying a different nonce is rejected.
	fake.nonce = "not-the-login-nonce"
	nonceReq, _ := http.NewRequest(http.MethodGet,
		server.URL+"/auth/callback?state="+url.QueryEscape(state)+"&code=fake-code", nil)
	nonceReq.AddCookie(oauthCookie)
	resp, err = client.Do(nonceReq)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("nonce mismatch expected 401, got %d", resp.StatusCode)
	}
}

// The static guard sends unauthenticated HTML navigations to login and
// leaves asset requests alone.
func TestStaticGuardRedirectsHTML(t *testing.T) {
	a := &authService{enabled: true}
	block, err := newTestAEAD()
	if err != nil {
		t.Fatal(err)
	}
	a.aead = block
	inner := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(200) })
	guard := a.staticGuard(inner)

	w := httptest.NewRecorder()
	guard.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/", nil))
	if w.Code != http.StatusFound || w.Header().Get("Location") != "/auth/login?next=%2F" {
		t.Fatalf("expected login redirect, got %d %q", w.Code, w.Header().Get("Location"))
	}

	w = httptest.NewRecorder()
	guard.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/_next/static/chunk.js", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("asset must be public, got %d", w.Code)
	}
	w = httptest.NewRecorder()
	guard.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/favicon.ico", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("file-with-extension must be public, got %d", w.Code)
	}

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.AddCookie(&http.Cookie{Name: sessionCookieName, Value: sessionCookieValue(t, a, testIdentity())})
	w = httptest.NewRecorder()
	guard.ServeHTTP(w, req)
	if w.Code != http.StatusOK {
		t.Fatalf("authenticated navigation must pass, got %d", w.Code)
	}
}

func newTestAEAD() (cipher.AEAD, error) {
	key := deriveKey([]byte("test-secret-test-secret-test-secret"), "flux-session-cookie")
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, err
	}
	return cipher.NewGCM(block)
}
