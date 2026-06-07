package main

import (
	"crypto"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"math/big"
	"testing"
)

// signRS256 mints a compact RS256 JWT for the given claims/kid, mirroring how
// the Rise backend signs workload tokens.
func signRS256(t *testing.T, key *rsa.PrivateKey, kid string, claims map[string]any) string {
	t.Helper()
	header := map[string]any{"alg": "RS256", "typ": "JWT", "kid": kid}
	enc := func(v any) string {
		b, err := json.Marshal(v)
		if err != nil {
			t.Fatalf("marshal: %v", err)
		}
		return base64.RawURLEncoding.EncodeToString(b)
	}
	signingInput := enc(header) + "." + enc(claims)
	hashed := sha256.Sum256([]byte(signingInput))
	sig, err := rsa.SignPKCS1v15(rand.Reader, key, crypto.SHA256, hashed[:])
	if err != nil {
		t.Fatalf("sign: %v", err)
	}
	return signingInput + "." + base64.RawURLEncoding.EncodeToString(sig)
}

// jwksFor builds a single-key JWKS for a public key, as the backend's
// /api/v1/auth/jwks would publish it.
func jwksFor(t *testing.T, pub *rsa.PublicKey, kid string) *jwkSet {
	t.Helper()
	eBytes := big.NewInt(int64(pub.E)).Bytes()
	return &jwkSet{Keys: []jwk{{
		Kid: kid,
		Kty: "RSA",
		Alg: "RS256",
		N:   base64.RawURLEncoding.EncodeToString(pub.N.Bytes()),
		E:   base64.RawURLEncoding.EncodeToString(eBytes),
	}}}
}

func TestVerifyJWT_Valid(t *testing.T) {
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatalf("genkey: %v", err)
	}
	const kid = "test-kid"
	want := map[string]any{
		"iss": "http://rise.local",
		"sub": "rise:proj:my-app:env:production",
		"aud": "rise-e2e-audience",
	}
	token := signRS256(t, key, kid, want)
	jwks := jwksFor(t, &key.PublicKey, kid)

	claims, err := verifyJWT(token, jwks)
	if err != nil {
		t.Fatalf("verifyJWT: %v", err)
	}
	for k, v := range want {
		if claims[k] != v {
			t.Errorf("claim %q = %v, want %v", k, claims[k], v)
		}
	}
}

func TestVerifyJWT_TamperedSignatureRejected(t *testing.T) {
	key, _ := rsa.GenerateKey(rand.Reader, 2048)
	other, _ := rsa.GenerateKey(rand.Reader, 2048)
	const kid = "test-kid"
	token := signRS256(t, key, kid, map[string]any{"sub": "x"})
	// Publish a JWKS for a DIFFERENT key — signature must not verify.
	jwks := jwksFor(t, &other.PublicKey, kid)

	if _, err := verifyJWT(token, jwks); err == nil {
		t.Fatal("expected verification to fail for a key mismatch, got nil")
	}
}

func TestVerifyJWT_UnknownKidRejected(t *testing.T) {
	key, _ := rsa.GenerateKey(rand.Reader, 2048)
	token := signRS256(t, key, "kid-a", map[string]any{"sub": "x"})
	jwks := jwksFor(t, &key.PublicKey, "kid-b")

	if _, err := verifyJWT(token, jwks); err == nil {
		t.Fatal("expected verification to fail for an unknown kid, got nil")
	}
}

func TestRSAPublicKeyRoundTrip(t *testing.T) {
	key, _ := rsa.GenerateKey(rand.Reader, 2048)
	jwks := jwksFor(t, &key.PublicKey, "k")
	pub, err := jwks.Keys[0].rsaPublicKey()
	if err != nil {
		t.Fatalf("rsaPublicKey: %v", err)
	}
	if pub.N.Cmp(key.PublicKey.N) != 0 || pub.E != key.PublicKey.E {
		t.Fatalf("round-trip mismatch: got (N=%v,E=%d)", pub.N, pub.E)
	}
}
