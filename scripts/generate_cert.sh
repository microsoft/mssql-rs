#!/bin/bash
set -e

# Parameters
DAYS_VALID=365
CA_KEY="ca.key"
CA_CERT="ca.crt"
SERVER_KEY="mssql.key"
SERVER_CSR="mssql.csr"
SERVER_CERT="mssql.crt"
CONFIG_FILE="openssl.cnf"

# Subject details
SERVER_CN="sql1"
SAN_DNS="sql1"

# Get the local hostname for SAN
LOCAL_HOSTNAME=$(hostname 2>/dev/null || echo "")
echo "Adding local hostname to SAN: $LOCAL_HOSTNAME"

# Optional extra IP SAN entry (e.g. the public IP a cross-pool SQL host is
# reached on). Set EXTRA_IP_SAN to have it added to the certificate.
EXTRA_IP_SAN_LINE=""
if [ -n "${EXTRA_IP_SAN:-}" ]; then
    echo "Adding extra IP to SAN: $EXTRA_IP_SAN"
    EXTRA_IP_SAN_LINE="IP.3 = $EXTRA_IP_SAN"
fi

# 1. Generate self-signed CA
openssl req -x509 -nodes -newkey rsa:4096 -sha256 \
    -keyout "$CA_KEY" \
    -out "$CA_CERT" \
    -subj "/CN=MyTestCA" \
    -days "$DAYS_VALID"

# 2. Create OpenSSL config for SAN
cat > "$CONFIG_FILE" <<EOF
[ req ]
default_bits       = 2048
prompt             = no
default_md         = sha256
x509_extensions    = req_ext
distinguished_name = dn

[ dn ]
CN = $SERVER_CN

[ req_ext ]
subjectAltName = @alt_names

[ alt_names ]
DNS.1 = $SAN_DNS
DNS.2 = localhost
DNS.3 = $LOCAL_HOSTNAME
IP.1 = 127.0.0.1
IP.2 = ::1
${EXTRA_IP_SAN_LINE:-}

[ v3_ext ]
authorityKeyIdentifier=keyid,issuer
basicConstraints=CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt_names
EOF

# 3. Generate server private key and CSR
openssl req -new -nodes -newkey rsa:2048 \
    -keyout "$SERVER_KEY" \
    -out "$SERVER_CSR" \
    -config "$CONFIG_FILE"

# 4. Sign the server CSR with the CA
openssl x509 -req \
    -in "$SERVER_CSR" \
    -CA "$CA_CERT" -CAkey "$CA_KEY" -CAcreateserial \
    -out "$SERVER_CERT" \
    -days "$DAYS_VALID" \
    -sha256 \
    -extfile "$CONFIG_FILE" -extensions v3_ext
    
cp $SERVER_CERT mssql.pem

echo "✅ Certificates generated:"
echo "  CA:           $CA_CERT"
echo "  Server Key:   $SERVER_KEY"
echo "  Server Cert:  $SERVER_CERT"

