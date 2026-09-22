# Generate test certificates for mock TDS server TLS tests
# This script generates self-signed certificates for testing purposes only.
# Do NOT use these certificates in production.
#
# This script uses native .NET/PowerShell APIs and does NOT require OpenSSL.

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$CertDir = Join-Path $ScriptDir "..\mssql-tds\tests\test_certificates"

# Create directory if it doesn't exist
if (-not (Test-Path $CertDir)) {
    New-Item -ItemType Directory -Path $CertDir -Force | Out-Null
}

Write-Host "Generating test certificates for mock TDS server tests..."

$KeyPath = Join-Path $CertDir "key.pem"
$CertPemPath = Join-Path $CertDir "valid_cert.pem"
$CertDerPath = Join-Path $CertDir "valid_cert.der"
$PfxPath = Join-Path $CertDir "identity.pfx"

# Generate self-signed certificate using .NET APIs (no OpenSSL required)
try {
    # Create RSA key pair
    $rsa = [System.Security.Cryptography.RSA]::Create(2048)
    
    # Create certificate request
    $certRequest = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
        "CN=localhost, O=Test, L=Test, ST=Test, C=US",
        $rsa,
        [System.Security.Cryptography.HashAlgorithmName]::SHA256,
        [System.Security.Cryptography.RSASignaturePadding]::Pkcs1
    )
    
    # Add Subject Alternative Name extension for localhost
    $sanBuilder = [System.Security.Cryptography.X509Certificates.SubjectAlternativeNameBuilder]::new()
    $sanBuilder.AddDnsName("localhost")
    $sanBuilder.AddIpAddress([System.Net.IPAddress]::Parse("127.0.0.1"))
    $certRequest.CertificateExtensions.Add($sanBuilder.Build())
    
    # Add Basic Constraints (not a CA)
    $certRequest.CertificateExtensions.Add(
        [System.Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($false, $false, 0, $true)
    )
    
    # Add Key Usage
    $certRequest.CertificateExtensions.Add(
        [System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
            [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature -bor
            [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyEncipherment,
            $true
        )
    )
    
    # Add Enhanced Key Usage (Server Authentication)
    $serverAuthOid = [System.Security.Cryptography.Oid]::new("1.3.6.1.5.5.7.3.1", "Server Authentication")
    $oidCollection = [System.Security.Cryptography.OidCollection]::new()
    $oidCollection.Add($serverAuthOid) | Out-Null
    $enhancedKeyUsage = [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new(
        $oidCollection,
        $true
    )
    $certRequest.CertificateExtensions.Add($enhancedKeyUsage)
    
    # Create self-signed certificate (valid for 10 years)
    $notBefore = [System.DateTimeOffset]::UtcNow
    $notAfter = $notBefore.AddYears(10)
    $cert = $certRequest.CreateSelfSigned($notBefore, $notAfter)
    
    # Export certificate in DER format
    $derBytes = $cert.RawData
    [System.IO.File]::WriteAllBytes($CertDerPath, $derBytes)
    
    # Export certificate in PEM format
    $certPem = "-----BEGIN CERTIFICATE-----`n"
    $certPem += [System.Convert]::ToBase64String($derBytes, [System.Base64FormattingOptions]::InsertLineBreaks)
    $certPem += "`n-----END CERTIFICATE-----`n"
    [System.IO.File]::WriteAllText($CertPemPath, $certPem)
    
    # Export private key in PEM format
    $keyBytes = $rsa.ExportRSAPrivateKey()
    $keyPem = "-----BEGIN RSA PRIVATE KEY-----`n"
    $keyPem += [System.Convert]::ToBase64String($keyBytes, [System.Base64FormattingOptions]::InsertLineBreaks)
    $keyPem += "`n-----END RSA PRIVATE KEY-----`n"
    [System.IO.File]::WriteAllText($KeyPath, $keyPem)
    
    # Export PKCS#12 (.pfx) file with empty password
    # This allows native-tls to load the identity without needing OpenSSL
    $pfxBytes = $cert.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Pfx, "")
    [System.IO.File]::WriteAllBytes($PfxPath, $pfxBytes)
    
    # Clean up
    $rsa.Dispose()
    $cert.Dispose()
}
catch {
    Write-Error "Failed to generate certificate: $_"
    exit 1
}

# Generate the ServerCA fixtures: a private CA, a leaf issued by it, and an
# unrelated CA.
try {
    $CaCertPath = Join-Path $CertDir "ca_cert.pem"
    $CaKeyPath = Join-Path $CertDir "ca_key.pem"
    $CaSignedCertPath = Join-Path $CertDir "ca_signed_cert.pem"
    $CaSignedKeyPath = Join-Path $CertDir "ca_signed_key.pem"
    $CaSignedPfxPath = Join-Path $CertDir "ca_signed_identity.pfx"
    $UnrelatedCaCertPath = Join-Path $CertDir "unrelated_ca_cert.pem"
    $UnrelatedCaKeyPath = Join-Path $CertDir "unrelated_ca_key.pem"

    function Write-PemCertificate([byte[]]$Der, [string]$Path) {
        $pem = "-----BEGIN CERTIFICATE-----`n"
        $pem += [System.Convert]::ToBase64String($Der, [System.Base64FormattingOptions]::InsertLineBreaks)
        $pem += "`n-----END CERTIFICATE-----`n"
        [System.IO.File]::WriteAllText($Path, $pem)
    }

    function Write-PemRsaKey($Rsa, [string]$Path) {
        $pem = "-----BEGIN RSA PRIVATE KEY-----`n"
        $pem += [System.Convert]::ToBase64String($Rsa.ExportRSAPrivateKey(), [System.Base64FormattingOptions]::InsertLineBreaks)
        $pem += "`n-----END RSA PRIVATE KEY-----`n"
        [System.IO.File]::WriteAllText($Path, $pem)
    }

    function New-TestCa([string]$Subject) {
        $rsa = [System.Security.Cryptography.RSA]::Create(2048)
        $request = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
            $Subject,
            $rsa,
            [System.Security.Cryptography.HashAlgorithmName]::SHA256,
            [System.Security.Cryptography.RSASignaturePadding]::Pkcs1
        )
        $request.CertificateExtensions.Add(
            [System.Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($true, $false, 0, $true)
        )
        $request.CertificateExtensions.Add(
            [System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
                [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyCertSign -bor
                [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature,
                $true
            )
        )
        $notBefore = [System.DateTimeOffset]::UtcNow
        $cert = $request.CreateSelfSigned($notBefore, $notBefore.AddYears(10))
        return @{ Rsa = $rsa; Cert = $cert }
    }

    $ca = New-TestCa "CN=Test Root CA, O=Test, L=Test, ST=Test, C=US"
    Write-PemCertificate $ca.Cert.RawData $CaCertPath
    Write-PemRsaKey $ca.Rsa $CaKeyPath

    $unrelatedCa = New-TestCa "CN=Unrelated Root CA, O=Test, L=Test, ST=Test, C=US"
    Write-PemCertificate $unrelatedCa.Cert.RawData $UnrelatedCaCertPath
    Write-PemRsaKey $unrelatedCa.Rsa $UnrelatedCaKeyPath

    # Leaf certificate for localhost / 127.0.0.1 issued by the test CA
    $leafRsa = [System.Security.Cryptography.RSA]::Create(2048)
    $leafRequest = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
        "CN=localhost, O=Test, L=Test, ST=Test, C=US",
        $leafRsa,
        [System.Security.Cryptography.HashAlgorithmName]::SHA256,
        [System.Security.Cryptography.RSASignaturePadding]::Pkcs1
    )
    $leafSan = [System.Security.Cryptography.X509Certificates.SubjectAlternativeNameBuilder]::new()
    $leafSan.AddDnsName("localhost")
    $leafSan.AddIpAddress([System.Net.IPAddress]::Parse("127.0.0.1"))
    $leafRequest.CertificateExtensions.Add($leafSan.Build())
    $leafRequest.CertificateExtensions.Add(
        [System.Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($false, $false, 0, $true)
    )
    $leafRequest.CertificateExtensions.Add(
        [System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
            [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature -bor
            [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyEncipherment,
            $true
        )
    )
    $leafOids = [System.Security.Cryptography.OidCollection]::new()
    $leafOids.Add([System.Security.Cryptography.Oid]::new("1.3.6.1.5.5.7.3.1", "Server Authentication")) | Out-Null
    $leafRequest.CertificateExtensions.Add(
        [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new($leafOids, $true)
    )

    $serial = [byte[]]::new(8)
    [System.Security.Cryptography.RandomNumberGenerator]::Fill($serial)
    $leafNotBefore = [System.DateTimeOffset]::UtcNow
    $leafCert = $leafRequest.Create($ca.Cert, $leafNotBefore, $leafNotBefore.AddYears(9), $serial)

    Write-PemCertificate $leafCert.RawData $CaSignedCertPath
    Write-PemRsaKey $leafRsa $CaSignedKeyPath

    $leafWithKey = [System.Security.Cryptography.X509Certificates.RSACertificateExtensions]::CopyWithPrivateKey($leafCert, $leafRsa)
    [System.IO.File]::WriteAllBytes(
        $CaSignedPfxPath,
        $leafWithKey.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Pfx, "")
    )

    $ca.Rsa.Dispose()
    $ca.Cert.Dispose()
    $unrelatedCa.Rsa.Dispose()
    $unrelatedCa.Cert.Dispose()
    $leafRsa.Dispose()
    $leafCert.Dispose()
    $leafWithKey.Dispose()
}
catch {
    Write-Error "Failed to generate ServerCA test certificates: $_"
    exit 1
}

Write-Host "Test certificates generated in $CertDir`:"
Write-Host "  - key.pem (private key)"
Write-Host "  - valid_cert.pem (certificate in PEM format)"
Write-Host "  - valid_cert.der (certificate in DER format)"
Write-Host "  - identity.pfx (PKCS#12 identity, empty password)"
Write-Host "  - ca_cert.pem / ca_key.pem (private test CA)"
Write-Host "  - ca_signed_cert.pem / ca_signed_key.pem / ca_signed_identity.pfx (leaf signed by the test CA)"
Write-Host "  - unrelated_ca_cert.pem / unrelated_ca_key.pem (unrelated CA)"
Write-Host ""
Write-Host "Note: These are for testing only. Do not commit key.pem or valid_cert.pem to git."
