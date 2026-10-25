#!/bin/bash
# CI-specific database setup script for GitHub Actions
# Works with existing service containers (no container management)
# Waits for databases to be ready and loads ZoneMinder schema

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SCHEMA_FILE="$PROJECT_ROOT/zm_create.sql.in"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Diagnostics go to stderr so `$(process_schema)` captures only the file path.
log_info() { echo -e "${GREEN}[INFO]${NC} $1" >&2; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1" >&2; }
log_error() { echo -e "${RED}[ERROR]${NC} $1" >&2; }
log_debug() { echo -e "${BLUE}[DEBUG]${NC} $1" >&2; }

# MySQL configuration from environment
MYSQL_HOST="${MYSQL_HOST:-127.0.0.1}"
MYSQL_PORT="${MYSQL_PORT:-3307}"
MYSQL_USER="${MYSQL_USER:-zmuser}"
MYSQL_PASSWORD="${MYSQL_PASSWORD:-zmpass}"
MYSQL_DATABASE="${MYSQL_DATABASE:-zm_test}"

# PostgreSQL configuration from environment
POSTGRES_HOST="${POSTGRES_HOST:-127.0.0.1}"
POSTGRES_PORT="${POSTGRES_PORT:-5433}"
POSTGRES_USER="${POSTGRES_USER:-postgres}"
POSTGRES_PASSWORD="${POSTGRES_PASSWORD:-test_root_pass}"
POSTGRES_DB="${POSTGRES_DB:-zm_test_pg}"

# Process schema file (reused from db-manager.sh logic)
process_schema() {
    local output_file="$PROJECT_ROOT/zm_schema_processed.sql"
    
    if [ ! -f "$SCHEMA_FILE" ]; then
        log_info "Downloading ZoneMinder schema..."
        curl -sL https://raw.githubusercontent.com/ZoneMinder/zoneminder/master/db/zm_create.sql.in -o "$SCHEMA_FILE"
    fi
    
    log_debug "Processing schema file..."
    # Resolve `source @PKGDATADIR@/db/X.sql` directives by inlining the
    # referenced file from the repo's db/ directory. ZoneMinder splits
    # Object_Types / User_Preferences (and seed data) into separate files;
    # the old `-e '/^source /d'` strip silently dropped those tables.
    awk -v db_dir="$PROJECT_ROOT/db" '
        /^source @PKGDATADIR@\/db\// {
            f = $2; sub(/^@PKGDATADIR@\/db\//, "", f)
            path = db_dir "/" f
            n = 0
            while ((getline line < path) > 0) { print line; n++ }
            close(path)
            if (n == 0) print "-- WARNING: unresolved source: " f > "/dev/stderr"
            next
        }
        { print }
    ' "$SCHEMA_FILE" \
        | sed -e 's/@ZM_DB_NAME@/'"$MYSQL_DATABASE"'/g' \
              -e 's/@ZM_MYSQL_ENGINE@/InnoDB/g' \
              -e 's/@PKGDATADIR@/./g' \
        > "$output_file"
    
    echo "$output_file"
}

# Wait for MySQL to be ready
wait_for_mysql() {
    log_info "Waiting for MySQL to be ready on $MYSQL_HOST:$MYSQL_PORT..."
    
    local retries=30
    local wait_seconds=2
    local count=0
    
    while [ $count -lt $retries ]; do
        if mysql -h"$MYSQL_HOST" -P"$MYSQL_PORT" -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" -e "SELECT 1" >/dev/null 2>&1; then
            log_info "✅ MySQL is ready!"
            return 0
        fi
        
        count=$((count + 1))
        echo "  Waiting... ($count/$retries)"
        sleep $wait_seconds
    done
    
    log_error "MySQL failed to become ready after $((retries * wait_seconds)) seconds"
    return 1
}

# Wait for PostgreSQL to be ready
wait_for_postgresql() {
    log_info "Waiting for PostgreSQL to be ready on $POSTGRES_HOST:$POSTGRES_PORT..."
    
    local retries=30
    local wait_seconds=2
    local count=0
    
    while [ $count -lt $retries ]; do
        if PGPASSWORD="$POSTGRES_PASSWORD" psql -h"$POSTGRES_HOST" -p"$POSTGRES_PORT" -U"$POSTGRES_USER" -d"$POSTGRES_DB" -c "SELECT 1" >/dev/null 2>&1; then
            log_info "✅ PostgreSQL is ready!"
            return 0
        fi
        
        count=$((count + 1))
        echo "  Waiting... ($count/$retries)"
        sleep $wait_seconds
    done
    
    log_error "PostgreSQL failed to become ready after $((retries * wait_seconds)) seconds"
    return 1
}

# Setup MySQL schema
setup_mysql() {
    log_info "Setting up MySQL database..."
    
    # Wait for database to be ready
    wait_for_mysql || return 1
    
    # Process and load schema
    local schema_sql
    schema_sql=$(process_schema)
    
    log_info "Loading ZoneMinder schema into MySQL..."
    # ERROR 1304 (a procedure that already exists) is expected on a re-run.
    # Any other error leaves the schema half loaded, so stop here rather than
    # let the tests fail later for no visible reason.
    local errors
    errors=$(mysql -h"$MYSQL_HOST" -P"$MYSQL_PORT" -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" \
        "$MYSQL_DATABASE" < "$schema_sql" 2>&1 | grep "^ERROR" | grep -v "ERROR 1304" || true)
    if [ -n "$errors" ]; then
        log_error "Loading the schema failed:"
        echo "$errors" >&2
        return 1
    fi

    log_info "✅ MySQL database ready!"
    log_info "   Connection: mysql://$MYSQL_USER:****@$MYSQL_HOST:$MYSQL_PORT/$MYSQL_DATABASE"
}

# Setup PostgreSQL database
setup_postgresql() {
    log_info "Setting up PostgreSQL database..."
    
    # Wait for database to be ready
    wait_for_postgresql || return 1
    
    log_info "✅ PostgreSQL database ready!"
    log_info "   Connection: postgresql://$POSTGRES_USER:****@$POSTGRES_HOST:$POSTGRES_PORT/$POSTGRES_DB"
    log_warn "   PostgreSQL schema migration not yet implemented"
}

# Main execution
case "${1:-both}" in
    mysql)
        setup_mysql
        ;;
    postgresql|postgres)
        setup_postgresql
        ;;
    both)
        setup_mysql
        setup_postgresql
        ;;
    *)
        echo "Usage: $0 {mysql|postgresql|both}"
        echo ""
        echo "  mysql       - Setup MySQL with ZoneMinder schema"
        echo "  postgresql  - Setup PostgreSQL database"
        echo "  both        - Setup both databases (default)"
        echo ""
        echo "Environment variables:"
        echo "  MYSQL_HOST, MYSQL_PORT, MYSQL_USER, MYSQL_PASSWORD, MYSQL_DATABASE"
        echo "  POSTGRES_HOST, POSTGRES_PORT, POSTGRES_USER, POSTGRES_PASSWORD, POSTGRES_DB"
        exit 1
        ;;
esac
