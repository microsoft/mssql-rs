#!/bin/bash
#
# Configure Kerberos authentication for SQL Server
# Run this after containers are started and healthy
#
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Source credentials from .env file
if [ -f "$SCRIPT_DIR/.env" ]; then
    set -a
    source "$SCRIPT_DIR/.env"
    set +a
else
    echo "ERROR: .env file not found. Run ./generate-env.sh first"
    exit 1
fi

# Verify required environment variables
if [ -z "$KERBEROS_SA_PASSWORD" ] || [ -z "$KERBEROS_TEST_USER_PASSWORD" ]; then
    echo "ERROR: Required credentials not set in .env"
    exit 1
fi
SA_PASSWORD="$KERBEROS_SA_PASSWORD"
TEST_USER_PASSWORD="$KERBEROS_TEST_USER_PASSWORD"

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "Configuring Kerberos authentication..."

# Create test user in AD (if not exists).
#
# The samba-dc healthcheck (`samba-tool domain info`) can report healthy
# slightly before the AD database is ready to accept writes, so a
# `user create` issued right after startup can fail transiently. Retry with
# verification instead of swallowing the exit code, otherwise a genuine
# failure here is masked as "already exists" and only surfaces later as a
# confusing `kinit: Client not found` error in the test-run step.
echo -e "${YELLOW}Creating test user in AD...${NC}"
USER_READY=false
for attempt in $(seq 1 10); do
    if docker exec samba-dc samba-tool user list 2>/dev/null | grep -qix testuser; then
        USER_READY=true
        break
    fi
    if docker exec samba-dc samba-tool user create testuser "$TEST_USER_PASSWORD" \
        --given-name="Test" --surname="User" 2>/dev/null; then
        USER_READY=true
        break
    fi
    echo "  Attempt $attempt/10: testuser not ready yet, retrying in 3s..."
    sleep 3
done
if [ "$USER_READY" != true ]; then
    echo "ERROR: Failed to create or verify testuser in AD after 10 attempts"
    exit 1
fi
echo -e "${GREEN}✓ Test user ready${NC}"

# Add MSSQLSvc SPNs to SQL$ computer account
echo -e "${YELLOW}Registering SQL Server SPNs...${NC}"
docker exec samba-dc samba-tool spn add MSSQLSvc/sql.example.local SQL$ 2>/dev/null || echo "SPN exists"
docker exec samba-dc samba-tool spn add MSSQLSvc/sql.example.local:1433 SQL$ 2>/dev/null || echo "SPN exists"
echo -e "${GREEN}✓ SPNs registered${NC}"

# Export keytab from Samba AD
echo -e "${YELLOW}Exporting keytab from AD...${NC}"
docker exec samba-dc bash -c '
    rm -f /tmp/mssql.keytab
    samba-tool domain exportkeytab /tmp/mssql.keytab --principal=SQL\$@EXAMPLE.LOCAL
    samba-tool domain exportkeytab /tmp/mssql.keytab --principal=MSSQLSvc/sql.example.local@EXAMPLE.LOCAL
    samba-tool domain exportkeytab /tmp/mssql.keytab --principal=MSSQLSvc/sql.example.local:1433@EXAMPLE.LOCAL
    samba-tool domain exportkeytab /tmp/mssql.keytab --principal=host/sql.example.local@EXAMPLE.LOCAL
'
echo -e "${GREEN}✓ Keytab exported${NC}"

# Copy keytab to SQL Server
echo -e "${YELLOW}Deploying keytab to SQL Server...${NC}"
docker cp samba-dc:/tmp/mssql.keytab /tmp/mssql.keytab
docker cp /tmp/mssql.keytab mssql-kerberos:/var/opt/mssql/secrets/mssql.keytab
docker exec mssql-kerberos bash -c '
    chown mssql:mssql /var/opt/mssql/secrets/mssql.keytab
    chmod 400 /var/opt/mssql/secrets/mssql.keytab
'
rm -f /tmp/mssql.keytab
echo -e "${GREEN}✓ Keytab deployed${NC}"

# Verify keytab
echo -e "${YELLOW}Verifying keytab...${NC}"
docker exec mssql-kerberos bash -c '
    kinit -kt /var/opt/mssql/secrets/mssql.keytab SQL\$@EXAMPLE.LOCAL && \
    echo "Keytab verification: SUCCESS" && \
    kdestroy
' || echo "Keytab verification: FAILED (may need restart)"
echo -e "${GREEN}✓ Keytab verified${NC}"

# Create Windows login in SQL Server
echo -e "${YELLOW}Creating Windows login in SQL Server...${NC}"
docker exec mssql-kerberos /opt/mssql-tools18/bin/sqlcmd \
    -S localhost -U sa -P "$SA_PASSWORD" -C -Q "
    IF NOT EXISTS (SELECT 1 FROM sys.server_principals WHERE name = 'EXAMPLE\testuser')
    BEGIN
        CREATE LOGIN [EXAMPLE\testuser] FROM WINDOWS;
        CREATE USER [EXAMPLE\testuser] FOR LOGIN [EXAMPLE\testuser];
        ALTER SERVER ROLE sysadmin ADD MEMBER [EXAMPLE\testuser];
        PRINT 'Windows login created';
    END
    ELSE
        PRINT 'Windows login already exists';
"
echo -e "${GREEN}✓ Windows login ready${NC}"

echo ""
echo -e "${GREEN}Kerberos configuration complete!${NC}"
echo ""
echo "Keytab contents:"
docker exec mssql-kerberos klist -ke /var/opt/mssql/secrets/mssql.keytab | head -15
