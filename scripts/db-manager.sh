#!/bin/bash
# Cross-platform database manager for development and CI/CD.
#
# Runtimes (auto-detected; override with DB_RUNTIME=docker|podman|native):
#   - Docker Desktop (default, macOS/Linux/Windows)
#   - Podman (fallback)
#   - Native Homebrew MySQL/PostgreSQL
#
# Apple Container support was removed (persistent stability issues); Docker
# Desktop is now the default local runtime.

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

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }
log_debug() { echo -e "${BLUE}[DEBUG]${NC} $1"; }

# Detect container runtime. An explicit DB_RUNTIME always wins.
detect_runtime() {
    if [ -n "${DB_RUNTIME:-}" ]; then
        echo "$DB_RUNTIME"
        return 0
    fi

    # Docker Desktop first, on every OS.
    if command -v docker &> /dev/null && docker info &> /dev/null 2>&1; then
        echo "docker"
        return 0
    elif command -v podman &> /dev/null; then
        echo "podman"
        return 0
    elif command -v mysql &> /dev/null && command -v psql &> /dev/null; then
        echo "native"
        return 0
    fi

    echo "none"
}

RUNTIME=$(detect_runtime)
log_info "Detected runtime: $RUNTIME ($(uname -s))"

case "$RUNTIME" in
    docker)
        CONTAINER_CMD="docker"
        log_info "Using Docker Desktop"
        if ! docker info &> /dev/null 2>&1; then
            log_error "Docker is installed but the daemon is not running. Start Docker Desktop and retry."
            exit 1
        fi
        ;;
    podman)
        CONTAINER_CMD="podman"
        log_info "Using Podman"
        ;;
    native)
        CONTAINER_CMD=""
        log_info "Using native MySQL/PostgreSQL installations"
        ;;
    none)
        log_error "No supported database runtime found!"
        log_info "Install one of:"
        log_info "  - Docker Desktop (recommended): https://docker.com"
        log_info "  - Podman: brew install podman"
        log_info "  - Native MySQL + PostgreSQL (Homebrew)"
        log_info "Or set DB_RUNTIME=docker|podman|native to force one."
        exit 1
        ;;
    *)
        log_error "Unknown DB_RUNTIME='$RUNTIME' (expected docker|podman|native)"
        exit 1
        ;;
esac

# Container names (self-contained; the script does not use docker-compose).
CONTAINER_NAME_MYSQL="${CONTAINER_NAME_MYSQL:-zm-api-mysql}"
CONTAINER_NAME_POSTGRES="${CONTAINER_NAME_POSTGRES:-zm-api-postgres}"

# MySQL configuration — defaults match the fallback URL in
# tests/common/test_db.rs (mysql://zmuser:zmpass@127.0.0.1:3307/zm_test).
MYSQL_HOST="${MYSQL_HOST:-127.0.0.1}"
MYSQL_PORT="${MYSQL_PORT:-3307}"
MYSQL_ROOT_PASSWORD="${MYSQL_ROOT_PASSWORD:-test_root_pass}"
MYSQL_USER="${MYSQL_USER:-zmuser}"
MYSQL_PASSWORD="${MYSQL_PASSWORD:-zmpass}"
MYSQL_DATABASE="${MYSQL_DATABASE:-zm_test}"

# PostgreSQL configuration
PG_HOST="${PG_HOST:-127.0.0.1}"
PG_PORT="${PG_PORT:-5433}"
PG_USER="${PG_USER:-postgres}"
PG_PASSWORD="${PG_PASSWORD:-test_root_pass}"
PG_DATABASE="${PG_DATABASE:-zm_test_pg}"

# Process schema file: inline ZoneMinder's `source @PKGDATADIR@/db/X.sql`
# directives (Object_Types / User_Preferences / seed data live in separate
# files) and substitute the DB name, so the whole schema loads from one file.
process_schema() {
    local output_file="$PROJECT_ROOT/zm_schema_processed.sql"

    if [ ! -f "$SCHEMA_FILE" ]; then
        log_info "Downloading ZoneMinder schema..."
        curl -sL https://raw.githubusercontent.com/ZoneMinder/zoneminder/master/db/zm_create.sql.in -o "$SCHEMA_FILE"
    fi

    log_debug "Processing schema file..." >&2
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

    # The Config rows a fresh install has (zmconfgen.pl puts them in the
    # packaged zm_create.sql; the .in file has none).
    if [ -f "$PROJECT_ROOT/db/zm_config.sql" ]; then
        cat "$PROJECT_ROOT/db/zm_config.sql" >> "$output_file"
    fi

    echo "$output_file"
}

# True iff the named container is currently running.
container_running() {
    [ -n "$CONTAINER_CMD" ] && "$CONTAINER_CMD" ps --format '{{.Names}}' 2>/dev/null | grep -qx "$1"
}

# True iff a container with the name exists (running or stopped).
container_exists() {
    [ -n "$CONTAINER_CMD" ] && "$CONTAINER_CMD" ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$1"
}

# Setup MySQL / MariaDB
setup_mysql() {
    log_info "Setting up MySQL database..."
    local schema_sql
    schema_sql=$(process_schema)
    local schema_container_path="/tmp/zm_schema.sql"

    if [ "$RUNTIME" = "native" ]; then
        log_info "Using native MySQL on port 3306"
        MYSQL_PORT=3306
        MYSQL_HOST=localhost
        mysql -uroot -e "CREATE DATABASE IF NOT EXISTS \`$MYSQL_DATABASE\`;" 2>/dev/null || {
            log_warn "Could not connect to MySQL. Ensure it's running: brew services start mysql"
            return 1
        }
        mysql -uroot -e "CREATE USER IF NOT EXISTS '$MYSQL_USER'@'localhost' IDENTIFIED BY '$MYSQL_PASSWORD'; GRANT ALL PRIVILEGES ON \`$MYSQL_DATABASE\`.* TO '$MYSQL_USER'@'localhost'; FLUSH PRIVILEGES;" 2>/dev/null || true
        mysql -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" "$MYSQL_DATABASE" < "$schema_sql" 2>&1 | grep -v "ERROR 1304" || true
        log_info "✅ MySQL database ready!"
        log_info "   Connection: mysql://$MYSQL_USER:****@$MYSQL_HOST:$MYSQL_PORT/$MYSQL_DATABASE"
        return 0
    fi

    if container_running "$CONTAINER_NAME_MYSQL"; then
        log_warn "MySQL container already running; reusing it"
    else
        # Clear any stopped container with the same name.
        container_exists "$CONTAINER_NAME_MYSQL" && "$CONTAINER_CMD" rm -f "$CONTAINER_NAME_MYSQL" >/dev/null 2>&1 || true

        log_info "Starting MariaDB 11.8 container ($CONTAINER_NAME_MYSQL)..."
        "$CONTAINER_CMD" run \
            --name "$CONTAINER_NAME_MYSQL" \
            -e MARIADB_ROOT_PASSWORD="$MYSQL_ROOT_PASSWORD" \
            -e MARIADB_DATABASE="$MYSQL_DATABASE" \
            -e MARIADB_USER="$MYSQL_USER" \
            -e MARIADB_PASSWORD="$MYSQL_PASSWORD" \
            -p "$MYSQL_PORT:3306" \
            -d \
            mariadb:11.8 \
            --character-set-server=utf8mb4 \
            --collation-server=utf8mb4_unicode_ci \
            --max-connections=200 >/dev/null

        log_info "Waiting for MySQL to accept connections..."
        local ready="no"
        for i in $(seq 1 30); do
            if "$CONTAINER_CMD" exec "$CONTAINER_NAME_MYSQL" \
                mariadb -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" -e "SELECT 1" &>/dev/null; then
                ready="yes"
                break
            fi
            echo "  Waiting... ($i/30)"
            sleep 2
        done
        if [ "$ready" != "yes" ]; then
            log_error "MySQL did not become ready in time"
            return 1
        fi

        # MariaDB creates the app user for localhost only; grant remote access
        # so host-side connections on the published port authenticate.
        "$CONTAINER_CMD" exec "$CONTAINER_NAME_MYSQL" \
            mariadb -uroot -p"$MYSQL_ROOT_PASSWORD" \
            -e "GRANT ALL PRIVILEGES ON \`$MYSQL_DATABASE\`.* TO '$MYSQL_USER'@'%' IDENTIFIED BY '$MYSQL_PASSWORD'; FLUSH PRIVILEGES;" 2>/dev/null || true
    fi

    log_info "Loading ZoneMinder schema (this may take a few minutes)..."
    local heartbeat_pid=""
    cleanup_heartbeat() {
        if [ -n "$heartbeat_pid" ]; then
            kill "$heartbeat_pid" 2>/dev/null || true
            wait "$heartbeat_pid" 2>/dev/null || true
            heartbeat_pid=""
        fi
    }
    trap cleanup_heartbeat RETURN
    ( while true; do log_info "Schema load still running..."; sleep 15; done ) &
    heartbeat_pid=$!

    # Start from an empty database: upstream's create script no longer drops
    # every table before creating it, so loading over a previous schema
    # aborts on the first table that still exists.
    "$CONTAINER_CMD" exec "$CONTAINER_NAME_MYSQL" \
        mariadb -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" \
        -e "DROP DATABASE IF EXISTS $MYSQL_DATABASE; CREATE DATABASE $MYSQL_DATABASE;"

    # Copy the processed schema into the container, then load it with the
    # container's own client (no host mysql client required).
    "$CONTAINER_CMD" cp "$schema_sql" "$CONTAINER_NAME_MYSQL:$schema_container_path"
    "$CONTAINER_CMD" exec "$CONTAINER_NAME_MYSQL" sh -lc \
        "mariadb -u\"$MYSQL_USER\" -p\"$MYSQL_PASSWORD\" \"$MYSQL_DATABASE\" < \"$schema_container_path\" 2>&1 | grep -v \"ERROR 1304\" || true"

    cleanup_heartbeat
    trap - RETURN

    log_info "✅ MySQL database ready!"
    log_info "   Connection: mysql://$MYSQL_USER:****@$MYSQL_HOST:$MYSQL_PORT/$MYSQL_DATABASE"
}

# Setup PostgreSQL (scaffolded; not the primary runtime path)
setup_postgresql() {
    log_info "Setting up PostgreSQL database..."

    if [ "$RUNTIME" = "native" ]; then
        log_info "Using native PostgreSQL on port 5432"
        PG_PORT=5432
        PG_HOST=localhost
        PGPASSWORD="$PG_PASSWORD" psql -h "$PG_HOST" -U "$PG_USER" -c "CREATE DATABASE $PG_DATABASE;" 2>/dev/null || {
            log_info "Database may already exist or PostgreSQL not running"
        }
        log_info "✅ PostgreSQL database ready!"
        return 0
    fi

    if container_running "$CONTAINER_NAME_POSTGRES"; then
        log_warn "PostgreSQL container already running; reusing it"
    else
        container_exists "$CONTAINER_NAME_POSTGRES" && "$CONTAINER_CMD" rm -f "$CONTAINER_NAME_POSTGRES" >/dev/null 2>&1 || true

        log_info "Starting PostgreSQL 16 container ($CONTAINER_NAME_POSTGRES)..."
        "$CONTAINER_CMD" run \
            --name "$CONTAINER_NAME_POSTGRES" \
            -e POSTGRES_PASSWORD="$PG_PASSWORD" \
            -e POSTGRES_USER="$PG_USER" \
            -e POSTGRES_DB="$PG_DATABASE" \
            -p "$PG_PORT:5432" \
            -d \
            postgres:16-alpine >/dev/null

        log_info "Waiting for PostgreSQL to be ready..."
        for i in $(seq 1 30); do
            if "$CONTAINER_CMD" exec "$CONTAINER_NAME_POSTGRES" pg_isready -U "$PG_USER" &>/dev/null; then
                break
            fi
            echo "  Waiting... ($i/30)"
            sleep 2
        done
    fi

    log_info "✅ PostgreSQL database ready!"
    log_info "   Connection: postgresql://$PG_USER:****@$PG_HOST:$PG_PORT/$PG_DATABASE"
}

# Stop and remove the managed containers.
stop_containers() {
    if [ "$RUNTIME" = "native" ]; then
        log_info "Native mode - use brew services to stop if needed"
        return 0
    fi
    log_info "Stopping database containers..."
    for name in "$CONTAINER_NAME_MYSQL" "$CONTAINER_NAME_POSTGRES"; do
        "$CONTAINER_CMD" rm -f "$name" >/dev/null 2>&1 && log_info "  removed $name" || true
    done
    log_info "Stopped"
}

# Dump MySQL schema (no data) to a file, via the container's client.
dump_mysql() {
    log_info "Dumping MySQL schema..."
    local output_file="$PROJECT_ROOT/mysql_schema_dump.sql"

    if [ "$RUNTIME" = "native" ]; then
        mysqldump -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" \
            --no-data --skip-comments "$MYSQL_DATABASE" > "$output_file"
    else
        "$CONTAINER_CMD" exec "$CONTAINER_NAME_MYSQL" \
            mariadb-dump -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" \
            --no-data --skip-comments "$MYSQL_DATABASE" > "$output_file"
    fi

    log_info "✅ Schema dumped to: $output_file"
}

# Generate SeaORM entities from the running MySQL.
generate_entities() {
    log_info "Generating SeaORM entities from MySQL..."
    if ! command -v sea-orm-cli &> /dev/null; then
        log_error "sea-orm-cli not found!"
        log_info "Install: cargo install sea-orm-cli"
        exit 1
    fi
    cd "$PROJECT_ROOT"
    local db_url="mysql://$MYSQL_USER:$MYSQL_PASSWORD@$MYSQL_HOST:$MYSQL_PORT/$MYSQL_DATABASE"
    sea-orm-cli generate entity \
        --database-url "$db_url" \
        --output-dir src/entity_from_mysql \
        --with-serde both
    log_info "✅ Entities generated in: src/entity_from_mysql/"
}

# Status check
status() {
    log_info "Database Status Check"
    echo ""
    log_info "Runtime: $RUNTIME"
    echo ""

    if [ "$RUNTIME" = "native" ]; then
        echo "  MySQL:"
        mysql -u"$MYSQL_USER" -p"$MYSQL_PASSWORD" -e "SELECT VERSION();" 2>/dev/null && \
            log_info "    ✅ Running" || log_warn "    ❌ Not accessible"
        echo "  PostgreSQL:"
        PGPASSWORD="$PG_PASSWORD" psql -h localhost -U "$PG_USER" -c "SELECT version();" 2>/dev/null | head -3 && \
            log_info "    ✅ Running" || log_warn "    ❌ Not accessible"
    else
        echo "  MySQL ($CONTAINER_NAME_MYSQL):"
        if container_running "$CONTAINER_NAME_MYSQL"; then
            log_info "    ✅ Running on port $MYSQL_PORT"
        else
            log_warn "    ❌ Not running"
        fi
        echo ""
        echo "  PostgreSQL ($CONTAINER_NAME_POSTGRES):"
        if container_running "$CONTAINER_NAME_POSTGRES"; then
            log_info "    ✅ Running on port $PG_PORT"
        else
            log_warn "    ❌ Not running"
        fi
    fi

    echo ""
    log_info "Connection strings:"
    echo "  MySQL:      mysql://$MYSQL_USER:****@$MYSQL_HOST:$MYSQL_PORT/$MYSQL_DATABASE"
    echo "  PostgreSQL: postgresql://$PG_USER:****@$PG_HOST:$PG_PORT/$PG_DATABASE"
}

# Main command router
case "${1:-}" in
    start|mysql)
        setup_mysql
        ;;
    postgresql|postgres)
        setup_postgresql
        ;;
    both)
        setup_mysql
        setup_postgresql
        ;;
    stop)
        stop_containers
        ;;
    dump)
        dump_mysql
        ;;
    generate|entities)
        generate_entities
        ;;
    status)
        status
        ;;
    full)
        setup_mysql
        dump_mysql
        generate_entities
        log_info "🎉 Full setup complete!"
        ;;
    *)
        echo "Usage: $0 {start|mysql|postgresql|both|stop|dump|generate|status|full}"
        echo ""
        echo "Detected runtime: $RUNTIME (override with DB_RUNTIME=docker|podman|native)"
        echo ""
        echo "Commands:"
        echo "  start | mysql   - Start MariaDB and load the ZoneMinder schema"
        echo "  postgresql      - Start the PostgreSQL container"
        echo "  both            - Start both databases"
        echo "  stop            - Stop and remove the database containers"
        echo "  dump            - Dump the MySQL schema (no data)"
        echo "  generate        - Regenerate SeaORM entities from MySQL"
        echo "  status          - Show runtime + container status"
        echo "  full            - mysql + dump + generate"
        ;;
esac
