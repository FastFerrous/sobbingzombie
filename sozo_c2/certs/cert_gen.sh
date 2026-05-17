#!/usr/bin/env bash
set -euo pipefail

# --- CA (root of trust, never used directly for TLS) ---
openssl req -x509 -newkey ed25519 \
    -keyout ca.key -out ca.crt \
    -days 3650 -nodes \
    -subj "/CN=sozo-ca"

# --- Write ext files ---
cat > server_ext.cnf << 'EOF'
[v3_req]
basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EOF

cat > client_ext.cnf << 'EOF'
[v3_req]
basicConstraints=CA:FALSE
keyUsage=digitalSignature
extendedKeyUsage=clientAuth
subjectAltName=DNS:client
EOF

# --- Server key + CSR ---
openssl req -newkey ed25519 \
    -keyout server.key -out server.csr \
    -nodes \
    -subj "/CN=localhost"

# --- Sign server cert with CA (v3) ---
openssl x509 -req \
    -in server.csr \
    -CA ca.crt -CAkey ca.key \
    -CAcreateserial \
    -out server.crt \
    -days 3650 \
    -extensions v3_req \
    -extfile server_ext.cnf

# --- Client key + CSR ---
openssl req -newkey ed25519 \
    -keyout client.key -out client.csr \
    -nodes \
    -subj "/CN=client"

# --- Sign client cert with CA (v3) ---
openssl x509 -req \
    -in client.csr \
    -CA ca.crt -CAkey ca.key \
    -CAcreateserial \
    -out client.crt \
    -days 3650 \
    -extensions v3_req \
    -extfile client_ext.cnf

# --- DER versions for Rust ---
openssl x509 -in ca.crt    -outform DER -out ca.crt.der
openssl x509 -in server.crt -outform DER -out server.crt.der
openssl x509 -in client.crt -outform DER -out client.crt.der
openssl pkey -in client.key -outform DER -out client.key.der

# --- Full chain for aioquic ---
cat server.crt ca.crt > server_chain.crt

# --- Cleanup ---
rm client.csr server.csr server_ext.cnf client_ext.cnf
rm client.key client.crt  # PEM versions not needed, Rust uses DER

echo "Generated:"
echo "  ca.key / ca.crt / ca.crt.der         — CA (root of trust)"
echo "  server.key / server.crt              — server cert signed by CA"
echo "  server.crt.der                       — server cert DER for Rust"
echo "  server_chain.crt                     — server cert + CA chain for aioquic"
echo "  client.crt.der / client.key.der      — client cert+key DER for Rust mTLS"