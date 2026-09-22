# Test Certificates

This directory contains test certificates for validating the TLS features in mock TDS server tests.

## Test Certificate Files

### valid_cert.pem / key.pem
A valid self-signed certificate and private key in PEM format for testing TLS connections.
These files are NOT tracked in git (they contain secrets). Generate them locally using the scripts below.

### valid_cert.der  
The same certificate in DER (binary) format for testing DER file loading.

### ca_cert.pem / ca_key.pem
A private test CA used by the ServerCA (custom trust root) tests.

### ca_signed_cert.pem / ca_signed_key.pem
A leaf certificate for `localhost` / `127.0.0.1` issued by the test CA above.
The PowerShell generator additionally writes `ca_signed_identity.pfx` for
Windows, where identities are loaded from PKCS#12 files.

### unrelated_ca_cert.pem / unrelated_ca_key.pem
A second, unrelated CA used to verify that trusting one CA does not trust another.

### invalid_format.txt
An invalid file that doesn't contain a valid certificate, used to test error handling.

## Generating Test Certificates

Before running TLS tests, generate the test certificates locally.
The ServerCA tests require these fixtures on every platform; missing, unreadable,
or malformed files fail the tests rather than silently skipping TLS coverage.

### From repository root (recommended for CI/CD):

**Linux/macOS:**
```bash
./scripts/generate_mock_tds_server_certs.sh
```

**Windows (PowerShell):**
```powershell
.\scripts\generate_mock_tds_server_certs.ps1
```

### From this directory:

```bash
./generate_certs.sh
```

**Note:** These certificates are for testing only and should never be used in production.
The key.pem and valid_cert.pem files are gitignored to prevent pushing secrets.
