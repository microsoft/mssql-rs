#!/bin/bash
# Generate test certificates for TLS testing
# These certificates are for testing only - do not use in production

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"
CERT_DIR="$SCRIPT_DIR"

echo "Generating test certificates..."

# Generate self-signed certificate and private key
openssl req -x509 -newkey rsa:2048 -keyout key.pem -out valid_cert.pem -days 3650 -nodes \
  -subj "/C=US/ST=Test/L=Test/O=Test/CN=localhost" 2>/dev/null

# Convert to DER format
openssl x509 -in valid_cert.pem -outform DER -out valid_cert.der 2>/dev/null

# Generate a private CA plus a leaf certificate signed by it. These back the
# custom trust root tests: the client trusts the CA, not the leaf.
openssl req -x509 -newkey rsa:2048 \
    -keyout "$CERT_DIR/ca_key.pem" \
    -out "$CERT_DIR/ca_cert.pem" \
    -days 3650 \
    -nodes \
    -subj "/C=US/ST=Test/L=Test/O=Test/CN=Test Root CA" 2>/dev/null

openssl req -newkey rsa:2048 \
    -keyout "$CERT_DIR/ca_signed_key.pem" \
    -out "$CERT_DIR/ca_signed.csr" \
    -nodes \
    -subj "/C=US/ST=Test/L=Test/O=Test/CN=localhost" 2>/dev/null

printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n" > "$CERT_DIR/ca_signed.ext"

openssl x509 -req \
    -in "$CERT_DIR/ca_signed.csr" \
    -CA "$CERT_DIR/ca_cert.pem" \
    -CAkey "$CERT_DIR/ca_key.pem" \
    -CAcreateserial \
    -out "$CERT_DIR/ca_signed_cert.pem" \
    -days 3650 \
    -extfile "$CERT_DIR/ca_signed.ext" 2>/dev/null

rm -f "$CERT_DIR/ca_signed.csr" "$CERT_DIR/ca_signed.ext" "$CERT_DIR/ca_cert.srl"

# An unrelated CA used to verify that trusting one CA does not trust another.
openssl req -x509 -newkey rsa:2048 \
    -keyout "$CERT_DIR/unrelated_ca_key.pem" \
    -out "$CERT_DIR/unrelated_ca_cert.pem" \
    -days 3650 \
    -nodes \
    -subj "/C=US/ST=Test/L=Test/O=Test/CN=Unrelated Root CA" 2>/dev/null

echo "Test certificates generated:"
echo "  - key.pem (private key)"
echo "  - valid_cert.pem (certificate in PEM format)"
echo "  - valid_cert.der (certificate in DER format)"
echo "  - ca_cert.pem / ca_key.pem (private test CA)"
echo "  - ca_signed_cert.pem / ca_signed_key.pem (leaf signed by the test CA)"
echo "  - unrelated_ca_cert.pem / unrelated_ca_key.pem (unrelated CA)"
echo ""
echo "Note: These are for testing only. Do not commit key.pem or valid_cert.pem to git."
