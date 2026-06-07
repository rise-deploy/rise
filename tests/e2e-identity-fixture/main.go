// Command e2e-identity-fixture is a tiny app deployed by the Rise e2e suites to
// prove the workload-identity contract end to end from *inside* a deployed
// container, identically on every deployment backend.
//
// It exercises, on each request to /identity:
//
//   - the pre-minted token files written from `.rise.toml`'s
//     `[identity.audiences]` (read fresh every request so the harness can poll
//     for the controller's in-place refresh);
//   - the token-exchange endpoint (POST {RISE_ISSUER}/api/v1/identity/token with
//     the bootstrap credential), proving on-demand issuance;
//   - that both tokens are genuine Rise-signed RS256 JWTs, by verifying their
//     signatures against the JWKS advertised by OIDC discovery.
//
// Because verification happens in-container, the harness assertions reduce to
// "curl this app and check the JSON" — byte-for-byte identical across backends.
// The reachability mechanism (Traefik vs. port-forward) is the only thing that
// differs, and that lives in the per-backend e2e scripts.
package main

import (
	"bytes"
	"crypto"
	"crypto/rsa"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"math/big"
	"net/http"
	"os"
	"strings"
	"time"
)

const (
	credentialPath = "/var/run/secrets/rise/identity/credential"
	tokenDir       = "/var/run/secrets/rise/identity/tokens"
)

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}

	mux := http.NewServeMux()
	// `/` answers 200 so default health probes / ingress routing treat the app as
	// up; `/identity` and `/healthz` are more specific and take precedence.
	mux.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = io.WriteString(w, "e2e-identity-fixture\n")
	})
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = io.WriteString(w, "ok\n")
	})
	mux.HandleFunc("/identity", handleIdentity)

	addr := ":" + port
	log.Printf("e2e-identity-fixture listening on %s (issuer=%s)", addr, riseIssuer())
	srv := &http.Server{Addr: addr, Handler: mux, ReadHeaderTimeout: 10 * time.Second}
	log.Fatal(srv.ListenAndServe())
}

func riseIssuer() string {
	return strings.TrimRight(os.Getenv("RISE_ISSUER"), "/")
}

// tokenResult is the per-token report returned to the harness.
type tokenResult struct {
	Present        bool           `json:"present"`
	SignatureValid bool           `json:"signature_valid"`
	Claims         map[string]any `json:"claims,omitempty"`
	Error          string         `json:"error,omitempty"`
}

type identityResponse struct {
	Issuer            string      `json:"issuer"`
	CredentialPresent bool        `json:"credential_present"`
	FileToken         tokenResult `json:"file_token"`
	ExchangedToken    tokenResult `json:"exchanged_token"`
}

// handleIdentity reports the app's current workload identity.
//
//	GET /identity?audience=<exchange-aud>&file=<token-file-name>
//
// `file` defaults to "e2e" (the audience key the e2e .rise.toml declares) and
// `audience` defaults to that file's declared audience value.
func handleIdentity(w http.ResponseWriter, r *http.Request) {
	fileName := r.URL.Query().Get("file")
	if fileName == "" {
		fileName = "e2e"
	}
	audience := r.URL.Query().Get("audience")
	if audience == "" {
		audience = "rise-e2e-audience"
	}

	resp := identityResponse{Issuer: riseIssuer()}

	// The verifier is shared by both checks; fetch the JWKS once.
	jwks, jwksErr := fetchJWKS(riseIssuer())

	// (1) Pre-minted token file, read fresh on every request.
	resp.FileToken = inspectToken(readFileToken(fileName), jwks, jwksErr)

	// (2) On-demand exchange of the bootstrap credential.
	cred, credErr := os.ReadFile(credentialPath)
	resp.CredentialPresent = credErr == nil && len(bytes.TrimSpace(cred)) > 0
	exchanged, exErr := exchangeToken(strings.TrimSpace(string(cred)), audience)
	if exErr != nil {
		resp.ExchangedToken = tokenResult{Error: exErr.Error()}
	} else {
		resp.ExchangedToken = inspectToken(exchanged, jwks, jwksErr)
	}

	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(resp)
}

func readFileToken(name string) string {
	raw, err := os.ReadFile(tokenDir + "/" + name)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(raw))
}

// inspectToken decodes a token and verifies its RS256 signature against the JWKS.
func inspectToken(token string, jwks *jwkSet, jwksErr error) tokenResult {
	res := tokenResult{Present: token != ""}
	if token == "" {
		res.Error = "token missing"
		return res
	}
	if jwksErr != nil {
		res.Error = fmt.Sprintf("jwks unavailable: %v", jwksErr)
		// Still surface the claims (unverified) to aid debugging.
		if claims, err := decodeClaims(token); err == nil {
			res.Claims = claims
		}
		return res
	}
	claims, err := verifyJWT(token, jwks)
	if err != nil {
		res.Error = err.Error()
		if claims, derr := decodeClaims(token); derr == nil {
			res.Claims = claims
		}
		return res
	}
	res.SignatureValid = true
	res.Claims = claims
	return res
}

// exchangeToken POSTs the bootstrap credential to the token-exchange endpoint.
func exchangeToken(credential, audience string) (string, error) {
	if credential == "" {
		return "", fmt.Errorf("bootstrap credential missing at %s", credentialPath)
	}
	body, _ := json.Marshal(map[string]string{"audience": audience})
	req, err := http.NewRequest(http.MethodPost, riseIssuer()+"/api/v1/identity/token", bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+credential)
	req.Header.Set("Content-Type", "application/json")

	client := &http.Client{Timeout: 15 * time.Second}
	httpResp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer httpResp.Body.Close()
	respBody, _ := io.ReadAll(httpResp.Body)
	if httpResp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("exchange returned %d: %s", httpResp.StatusCode, strings.TrimSpace(string(respBody)))
	}
	var parsed struct {
		Token string `json:"token"`
	}
	if err := json.Unmarshal(respBody, &parsed); err != nil {
		return "", fmt.Errorf("decode exchange response: %w", err)
	}
	if parsed.Token == "" {
		return "", fmt.Errorf("exchange response had no token: %s", strings.TrimSpace(string(respBody)))
	}
	return parsed.Token, nil
}

// --- Minimal stdlib RS256 / JWKS verification (no third-party deps) ---

type jwk struct {
	Kid string `json:"kid"`
	Kty string `json:"kty"`
	Alg string `json:"alg"`
	N   string `json:"n"`
	E   string `json:"e"`
}

type jwkSet struct {
	Keys []jwk `json:"keys"`
}

func (s *jwkSet) find(kid string) *jwk {
	for i := range s.Keys {
		if s.Keys[i].Kid == kid {
			return &s.Keys[i]
		}
	}
	// Single-key JWKS: fall back to the only key when no kid is present.
	if kid == "" && len(s.Keys) == 1 {
		return &s.Keys[0]
	}
	return nil
}

func (k *jwk) rsaPublicKey() (*rsa.PublicKey, error) {
	if k.Kty != "RSA" {
		return nil, fmt.Errorf("unsupported key type %q", k.Kty)
	}
	nBytes, err := base64.RawURLEncoding.DecodeString(k.N)
	if err != nil {
		return nil, fmt.Errorf("decode modulus: %w", err)
	}
	eBytes, err := base64.RawURLEncoding.DecodeString(k.E)
	if err != nil {
		return nil, fmt.Errorf("decode exponent: %w", err)
	}
	e := 0
	for _, b := range eBytes {
		e = e<<8 | int(b)
	}
	return &rsa.PublicKey{N: new(big.Int).SetBytes(nBytes), E: e}, nil
}

// fetchJWKS resolves the JWKS via OIDC discovery, exactly as any verifier would.
func fetchJWKS(issuer string) (*jwkSet, error) {
	if issuer == "" {
		return nil, fmt.Errorf("RISE_ISSUER is not set")
	}
	client := &http.Client{Timeout: 15 * time.Second}

	discoResp, err := client.Get(issuer + "/.well-known/openid-configuration")
	if err != nil {
		return nil, fmt.Errorf("fetch discovery: %w", err)
	}
	defer discoResp.Body.Close()
	if discoResp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("discovery returned %d", discoResp.StatusCode)
	}
	var disco struct {
		JwksURI string `json:"jwks_uri"`
	}
	if err := json.NewDecoder(discoResp.Body).Decode(&disco); err != nil {
		return nil, fmt.Errorf("decode discovery: %w", err)
	}
	if disco.JwksURI == "" {
		return nil, fmt.Errorf("discovery advertised no jwks_uri")
	}

	jwksResp, err := client.Get(disco.JwksURI)
	if err != nil {
		return nil, fmt.Errorf("fetch jwks: %w", err)
	}
	defer jwksResp.Body.Close()
	if jwksResp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("jwks returned %d", jwksResp.StatusCode)
	}
	var set jwkSet
	if err := json.NewDecoder(jwksResp.Body).Decode(&set); err != nil {
		return nil, fmt.Errorf("decode jwks: %w", err)
	}
	if len(set.Keys) == 0 {
		return nil, fmt.Errorf("jwks had no keys")
	}
	return &set, nil
}

// verifyJWT checks a compact RS256 JWT against the JWKS and returns its claims.
func verifyJWT(token string, jwks *jwkSet) (map[string]any, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return nil, fmt.Errorf("not a compact JWT (%d segments)", len(parts))
	}
	headerJSON, err := base64.RawURLEncoding.DecodeString(parts[0])
	if err != nil {
		return nil, fmt.Errorf("decode header: %w", err)
	}
	var header struct {
		Alg string `json:"alg"`
		Kid string `json:"kid"`
	}
	if err := json.Unmarshal(headerJSON, &header); err != nil {
		return nil, fmt.Errorf("parse header: %w", err)
	}
	if header.Alg != "RS256" {
		return nil, fmt.Errorf("unexpected alg %q (want RS256)", header.Alg)
	}
	key := jwks.find(header.Kid)
	if key == nil {
		return nil, fmt.Errorf("kid %q not found in JWKS", header.Kid)
	}
	pub, err := key.rsaPublicKey()
	if err != nil {
		return nil, err
	}
	sig, err := base64.RawURLEncoding.DecodeString(parts[2])
	if err != nil {
		return nil, fmt.Errorf("decode signature: %w", err)
	}
	hashed := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	if err := rsa.VerifyPKCS1v15(pub, crypto.SHA256, hashed[:], sig); err != nil {
		return nil, fmt.Errorf("signature verification failed: %w", err)
	}
	return decodeClaims(token)
}

// decodeClaims decodes the JWT payload without verifying the signature.
func decodeClaims(token string) (map[string]any, error) {
	parts := strings.Split(token, ".")
	if len(parts) < 2 {
		return nil, fmt.Errorf("not a JWT")
	}
	payloadJSON, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return nil, fmt.Errorf("decode payload: %w", err)
	}
	var claims map[string]any
	if err := json.Unmarshal(payloadJSON, &claims); err != nil {
		return nil, fmt.Errorf("parse payload: %w", err)
	}
	return claims, nil
}
