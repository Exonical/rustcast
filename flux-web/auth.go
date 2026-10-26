package main

import (
	"context"
	"crypto/aes"
	"crypto/cipher"
	"crypto/hkdf"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"log"
	"net/http"
	"net/url"
	"os"
	"path"
	"strings"
	"sync"
	"time"

	"github.com/coreos/go-oidc/v3/oidc"
	"github.com/gin-gonic/gin"
	"golang.org/x/oauth2"
)

const (
	sessionCookieName = "flux_session"
	oauthCookieName   = "flux_oauth"
	sessionCookieMax  = 12 * time.Hour
	oauthCookieMax    = 10 * time.Minute
	wtTicketTTL       = 60 * time.Second
	maxCookieSize     = 3800
)

type authConfig struct {
	issuer        string
	clientID      string
	clientSecret  string
	redirectURL   string
	adminGroup    string
	scopes        []string
	sessionSecret []byte
	relayToken    string
}

func authConfigFromEnv() authConfig {
	scopesEnv := os.Getenv("FLUX_OIDC_SCOPES")
	if scopesEnv == "" {
		scopesEnv = "openid profile email groups"
	}
	scopes := strings.Fields(scopesEnv)
	secret := os.Getenv("FLUX_SESSION_SECRET")
	var secretBytes []byte
	if secret == "" {
		secretBytes = make([]byte, 32)
		if _, err := rand.Read(secretBytes); err != nil {
			log.Fatalf("generate session secret: %v", err)
		}
		log.Printf("[auth] WARN: FLUX_SESSION_SECRET unset; generated ephemeral secret (sessions will not survive restart)")
	} else {
		secretBytes = []byte(secret)
		if len(secretBytes) < 32 {
			log.Printf("[auth] WARN: FLUX_SESSION_SECRET shorter than 32 bytes")
		}
	}
	adminGroup := os.Getenv("FLUX_OIDC_ADMIN_GROUP")
	if adminGroup == "" {
		adminGroup = "flux-admins"
	}
	return authConfig{
		issuer:        strings.TrimSpace(os.Getenv("FLUX_OIDC_ISSUER")),
		clientID:      os.Getenv("FLUX_OIDC_CLIENT_ID"),
		clientSecret:  os.Getenv("FLUX_OIDC_CLIENT_SECRET"),
		redirectURL:   strings.TrimSpace(os.Getenv("FLUX_OIDC_REDIRECT_URL")),
		adminGroup:    adminGroup,
		scopes:        scopes,
		sessionSecret: secretBytes,
		relayToken:    os.Getenv("FLUX_RELAY_TOKEN"),
	}
}

type identity struct {
	Sub   string `json:"sub"`
	Email string `json:"email,omitempty"`
	Name  string `json:"name,omitempty"`
	// Username is the OIDC preferred_username, used to pre-answer the host's
	// login "Username" prompt.
	Username          string    `json:"username,omitempty"`
	ClipboardPolicy   string    `json:"clipboard_policy,omitempty"`
	ClipboardCapture  bool      `json:"clipboard_capture,omitempty"`
	ClipboardMaxBytes int64     `json:"clipboard_max_bytes,omitempty"`
	TicketPath        string    `json:"-"`
	Groups            []string  `json:"groups,omitempty"`
	Admin             bool      `json:"admin"`
	Expires           time.Time `json:"exp"`
	IDToken           string    `json:"id_token,omitempty"`
}

var anonymousAdmin = &identity{Sub: "anonymous", Name: "Anonymous", Admin: true}

type wtTicket struct {
	user    *identity
	expires time.Time
}

type authService struct {
	cfg        authConfig
	enabled    bool
	aead       cipher.AEAD
	tokenWarn  sync.Once
	endSession string

	providerMu sync.Mutex
	provider   *oidc.Provider
	verifier   *oidc.IDTokenVerifier
	oauth      oauth2.Config

	mu      sync.Mutex
	tickets map[string]*wtTicket
}

func newAuthService(ctx context.Context, cfg authConfig) (*authService, error) {
	a := &authService{cfg: cfg, tickets: make(map[string]*wtTicket)}
	block, err := aes.NewCipher(deriveKey(cfg.sessionSecret, "flux-session-cookie"))
	if err != nil {
		return nil, err
	}
	a.aead, err = cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	if cfg.issuer == "" {
		log.Printf("[auth] WARN: authentication disabled (FLUX_OIDC_ISSUER unset)")
		return a, nil
	}
	a.enabled = true
	// Discovery runs lazily: the IdP (e.g. Authentik in the same pod) can take
	// minutes to boot, and a missing provider must not kill the relay.
	go a.warmProvider(context.Background())
	return a, nil
}

// warmProvider retries OIDC discovery in the background until it succeeds.
func (a *authService) warmProvider(ctx context.Context) {
	for attempt := 1; ; attempt++ {
		if err := a.initProvider(ctx); err == nil {
			log.Printf("[auth] OIDC provider ready: %s", a.cfg.issuer)
			return
		} else if attempt%10 == 1 {
			log.Printf("[auth] OIDC discovery not ready yet: %v", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-time.After(5 * time.Second):
		}
	}
}

func (a *authService) initProvider(ctx context.Context) error {
	a.providerMu.Lock()
	defer a.providerMu.Unlock()
	if a.provider != nil {
		return nil
	}
	provider, err := oidc.NewProvider(ctx, a.cfg.issuer)
	if err != nil {
		return err
	}
	var claims struct {
		EndSession string `json:"end_session_endpoint"`
	}
	if err := provider.Claims(&claims); err == nil {
		a.endSession = claims.EndSession
	}
	a.oauth = oauth2.Config{
		ClientID:     a.cfg.clientID,
		ClientSecret: a.cfg.clientSecret,
		Endpoint:     provider.Endpoint(),
		Scopes:       a.cfg.scopes,
	}
	a.verifier = provider.Verifier(&oidc.Config{ClientID: a.cfg.clientID})
	a.provider = provider
	return nil
}

// providerReady returns false while OIDC discovery hasn't succeeded.
func (a *authService) providerReady(ctx context.Context) bool {
	if !a.enabled {
		return true
	}
	return a.initProvider(ctx) == nil
}

func deriveKey(secret []byte, purpose string) []byte {
	k, err := hkdf.Key(sha256.New, secret, []byte("flux-web:"+purpose), "", 32)
	if err != nil {
		panic(err)
	}
	return k
}

func (a *authService) seal(payload []byte) (string, error) {
	nonce := make([]byte, a.aead.NonceSize())
	if _, err := io.ReadFull(rand.Reader, nonce); err != nil {
		return "", err
	}
	sealed := a.aead.Seal(nonce, nonce, payload, nil)
	return base64.RawURLEncoding.EncodeToString(sealed), nil
}

func (a *authService) open(value string) ([]byte, error) {
	raw, err := base64.RawURLEncoding.DecodeString(value)
	if err != nil {
		return nil, err
	}
	if len(raw) < a.aead.NonceSize() {
		return nil, errors.New("cookie too short")
	}
	return a.aead.Open(nil, raw[:a.aead.NonceSize()], raw[a.aead.NonceSize():], nil)
}

func requestScheme(c *gin.Context) string {
	if proto := c.GetHeader("X-Forwarded-Proto"); proto != "" {
		return strings.Split(proto, ",")[0]
	}
	if c.Request.TLS != nil {
		return "https"
	}
	return "http"
}

func secureCookie(c *gin.Context) bool {
	return requestScheme(c) == "https"
}

func (a *authService) redirectURL(c *gin.Context) string {
	if a.cfg.redirectURL != "" {
		return a.cfg.redirectURL
	}
	return requestScheme(c) + "://" + c.Request.Host + "/auth/callback"
}

func sanitizeNext(next string) string {
	if next == "" || !strings.HasPrefix(next, "/") || strings.HasPrefix(next, "//") {
		return "/"
	}
	return next
}

type oauthCookie struct {
	State    string    `json:"s"`
	Nonce    string    `json:"n"`
	Verifier string    `json:"v"`
	Next     string    `json:"next"`
	Expires  time.Time `json:"exp"`
}

func randomToken(n int) string {
	b := make([]byte, n)
	if _, err := io.ReadFull(rand.Reader, b); err != nil {
		panic(err)
	}
	return base64.RawURLEncoding.EncodeToString(b)
}

func (a *authService) login(c *gin.Context) {
	if !a.enabled {
		c.Redirect(http.StatusFound, sanitizeNext(c.Query("next")))
		return
	}
	if !a.providerReady(c.Request.Context()) {
		c.String(http.StatusServiceUnavailable, "identity provider is still starting; retry shortly")
		return
	}
	cookie := oauthCookie{
		State:    randomToken(16),
		Nonce:    randomToken(16),
		Verifier: oauth2.GenerateVerifier(),
		Next:     sanitizeNext(c.Query("next")),
		Expires:  time.Now().Add(oauthCookieMax),
	}
	payload, _ := json.Marshal(cookie)
	sealed, err := a.seal(payload)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "cookie seal failed"})
		return
	}
	http.SetCookie(c.Writer, &http.Cookie{
		Name: oauthCookieName, Value: sealed, Path: "/auth",
		HttpOnly: true, SameSite: http.SameSiteLaxMode, Secure: secureCookie(c),
		MaxAge: int(oauthCookieMax.Seconds()),
	})
	conf := a.oauth
	conf.RedirectURL = a.redirectURL(c)
	c.Redirect(http.StatusFound, conf.AuthCodeURL(
		cookie.State,
		oauth2.S256ChallengeOption(cookie.Verifier),
		oauth2.SetAuthURLParam("nonce", cookie.Nonce),
	))
}

type oidcClaims struct {
	Email             string   `json:"email"`
	Name              string   `json:"name"`
	PreferredUsername string   `json:"preferred_username"`
	Groups            []string `json:"groups"`
	Nonce             string   `json:"nonce"`
}

func (a *authService) callback(c *gin.Context) {
	if !a.providerReady(c.Request.Context()) {
		c.String(http.StatusServiceUnavailable, "identity provider is still starting; retry shortly")
		return
	}
	rawCookie, err := c.Cookie(oauthCookieName)
	if err != nil {
		c.String(http.StatusBadRequest, "missing oauth state cookie")
		return
	}
	payload, err := a.open(rawCookie)
	if err != nil {
		c.String(http.StatusBadRequest, "invalid oauth state cookie")
		return
	}
	var pending oauthCookie
	if err := json.Unmarshal(payload, &pending); err != nil || time.Now().After(pending.Expires) {
		c.String(http.StatusBadRequest, "expired oauth state cookie")
		return
	}
	if subtle.ConstantTimeCompare([]byte(pending.State), []byte(c.Query("state"))) != 1 {
		c.String(http.StatusBadRequest, "state mismatch")
		return
	}
	conf := a.oauth
	conf.RedirectURL = a.redirectURL(c)
	token, err := conf.Exchange(c.Request.Context(), c.Query("code"), oauth2.VerifierOption(pending.Verifier))
	if err != nil {
		log.Printf("[auth] code exchange failed: %v", err)
		c.String(http.StatusBadGateway, "token exchange failed")
		return
	}
	rawIDToken, ok := token.Extra("id_token").(string)
	if !ok {
		c.String(http.StatusBadGateway, "no id_token in response")
		return
	}
	idToken, err := a.verifier.Verify(c.Request.Context(), rawIDToken)
	if err != nil {
		log.Printf("[auth] id_token verify failed: %v", err)
		c.String(http.StatusUnauthorized, "id_token verification failed")
		return
	}
	var claims oidcClaims
	if err := idToken.Claims(&claims); err != nil {
		c.String(http.StatusBadGateway, "claims parse failed")
		return
	}
	if claims.Nonce != pending.Nonce {
		c.String(http.StatusUnauthorized, "nonce mismatch")
		return
	}
	name := claims.Name
	if name == "" {
		name = claims.PreferredUsername
	}
	if name == "" {
		name = claims.Email
	}
	admin := false
	for _, g := range claims.Groups {
		if g == a.cfg.adminGroup {
			admin = true
			break
		}
	}
	expires := idToken.Expiry
	if cap := time.Now().Add(sessionCookieMax); expires.After(cap) {
		expires = cap
	}
	user := &identity{
		Sub: idToken.Subject, Email: claims.Email, Name: name,
		Username: claims.PreferredUsername,
		Groups:   claims.Groups, Admin: admin, Expires: expires,
		IDToken: rawIDToken,
	}
	sessionPayload, _ := json.Marshal(user)
	sealed, err := a.seal(sessionPayload)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "cookie seal failed"})
		return
	}
	if len(sealed) > maxCookieSize {
		user.IDToken = ""
		sessionPayload, _ = json.Marshal(user)
		sealed, err = a.seal(sessionPayload)
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": "cookie seal failed"})
			return
		}
	}
	http.SetCookie(c.Writer, &http.Cookie{
		Name: sessionCookieName, Value: sealed, Path: "/",
		HttpOnly: true, SameSite: http.SameSiteLaxMode, Secure: secureCookie(c),
		Expires: expires,
	})
	http.SetCookie(c.Writer, &http.Cookie{
		Name: oauthCookieName, Value: "", Path: "/auth", MaxAge: -1, HttpOnly: true,
	})
	c.Redirect(http.StatusFound, sanitizeNext(pending.Next))
}

func (a *authService) logout(c *gin.Context) {
	var idTokenHint string
	if user := a.sessionIdentity(c); user != nil {
		idTokenHint = user.IDToken
	}
	http.SetCookie(c.Writer, &http.Cookie{
		Name: sessionCookieName, Value: "", Path: "/", MaxAge: -1, HttpOnly: true,
	})
	logoutURL := "/"
	if a.endSession != "" {
		u, err := url.Parse(a.endSession)
		if err == nil {
			q := u.Query()
			if idTokenHint != "" {
				q.Set("id_token_hint", idTokenHint)
			}
			q.Set("post_logout_redirect_uri", requestScheme(c)+"://"+c.Request.Host+"/")
			u.RawQuery = q.Encode()
			logoutURL = u.String()
		}
	}
	c.JSON(http.StatusOK, gin.H{"logout_url": logoutURL})
}

func (a *authService) sessionIdentity(c *gin.Context) *identity {
	raw, err := c.Cookie(sessionCookieName)
	if err != nil {
		return nil
	}
	payload, err := a.open(raw)
	if err != nil {
		return nil
	}
	var user identity
	if err := json.Unmarshal(payload, &user); err != nil {
		return nil
	}
	if time.Now().After(user.Expires) {
		return nil
	}
	return &user
}

const identityContextKey = "flux.identity"

func (a *authService) userFrom(c *gin.Context) *identity {
	if v, ok := c.Get(identityContextKey); ok {
		if user, ok := v.(*identity); ok {
			return user
		}
	}
	return nil
}

// requireUser resolves the request identity: session cookie when auth is
// enabled, anonymous admin when disabled. API callers get a 401 JSON.
func (a *authService) requireUser() gin.HandlerFunc {
	return func(c *gin.Context) {
		var user *identity
		if !a.enabled {
			user = anonymousAdmin
		} else {
			user = a.sessionIdentity(c)
		}
		if user == nil {
			c.AbortWithStatusJSON(http.StatusUnauthorized, gin.H{
				"error": "unauthenticated",
				"login": "/auth/login",
			})
			return
		}
		c.Set(identityContextKey, user)
		c.Next()
	}
}

func (a *authService) requireAdmin() gin.HandlerFunc {
	return func(c *gin.Context) {
		user := a.userFrom(c)
		if user == nil || !user.Admin {
			c.AbortWithStatusJSON(http.StatusForbidden, gin.H{"error": "admin required"})
			return
		}
		c.Next()
	}
}

// requireMachineToken guards host-facing registration endpoints with the
// shared bearer token. Open when no token is configured.
func (a *authService) requireMachineToken() gin.HandlerFunc {
	return func(c *gin.Context) {
		if a.cfg.relayToken == "" {
			if a.enabled {
				a.tokenWarn.Do(func() {
					log.Printf("[auth] WARN: FLUX_RELAY_TOKEN unset; machine registration endpoints are unauthenticated")
				})
			}
			c.Next()
			return
		}
		header := c.GetHeader("Authorization")
		token := strings.TrimPrefix(header, "Bearer ")
		if token == header || subtle.ConstantTimeCompare([]byte(token), []byte(a.cfg.relayToken)) != 1 {
			c.AbortWithStatusJSON(http.StatusUnauthorized, gin.H{"error": "invalid bearer token"})
			return
		}
		c.Next()
	}
}

// issueTicket returns a single-use WebTransport ticket bound to the user.
func (a *authService) issueTicket(user *identity) string {
	a.mu.Lock()
	defer a.mu.Unlock()
	now := time.Now()
	for key, t := range a.tickets {
		if now.After(t.expires) {
			delete(a.tickets, key)
		}
	}
	ticket := randomToken(32)
	a.tickets[ticket] = &wtTicket{user: user, expires: now.Add(wtTicketTTL)}
	return ticket
}

// consumeTicket redeems a ticket exactly once.
func (a *authService) consumeTicket(ticket string) *identity {
	a.mu.Lock()
	defer a.mu.Unlock()
	entry, ok := a.tickets[ticket]
	if !ok {
		return nil
	}
	delete(a.tickets, ticket)
	if time.Now().After(entry.expires) {
		return nil
	}
	return entry.user
}

func (a *authService) registerRoutes(r *gin.Engine) {
	r.GET("/auth/login", a.login)
	r.GET("/auth/callback", a.callback)
	r.POST("/auth/logout", a.logout)
}

// staticGuard redirects unauthenticated HTML navigations to the login page.
// Static assets (paths with a file extension or under /_next/) stay public.
func (a *authService) staticGuard(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if a.enabled && (r.Method == http.MethodGet || r.Method == http.MethodHead) {
			p := r.URL.Path
			asset := strings.HasPrefix(p, "/_next/") || strings.Contains(path.Base(p), ".")
			if !asset {
				if a.sessionIdentityFromRequest(r) == nil {
					http.Redirect(w, r, "/auth/login?next="+url.QueryEscape(p), http.StatusFound)
					return
				}
			}
		}
		next.ServeHTTP(w, r)
	})
}

func (a *authService) sessionIdentityFromRequest(r *http.Request) *identity {
	cookie, err := r.Cookie(sessionCookieName)
	if err != nil {
		return nil
	}
	payload, err := a.open(cookie.Value)
	if err != nil {
		return nil
	}
	var user identity
	if err := json.Unmarshal(payload, &user); err != nil {
		return nil
	}
	if time.Now().After(user.Expires) {
		return nil
	}
	return &user
}
