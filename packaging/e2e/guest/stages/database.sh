#!/usr/bin/env bash
# A local MariaDB, as a single-host ZoneMinder install has.
. /e2e/lib.sh

section "MariaDB"
case "$E2E_FAMILY" in
  deb)
    apt-get update -qq
    sys_install mariadb-server >/dev/null ;;
  rpm)
    if command -v zypper >/dev/null 2>&1; then
      sys_install mariadb mariadb-client >/dev/null
    else
      sys_install mariadb-server >/dev/null
    fi ;;
  arch)
    sys_install mariadb >/dev/null
    mariadb-install-db --user=mysql --basedir=/usr --datadir=/var/lib/mysql >/dev/null ;;
esac

# Many installs run MariaDB with skip-name-resolve, where a 'zmuser'@'localhost'
# grant matches the Unix socket only and TCP to 127.0.0.1 is refused (#215).
# Without it 127.0.0.1 resolves back to localhost and TCP passes, hiding a
# client that ignores the socket.
for d in /etc/mysql/mariadb.conf.d /etc/my.cnf.d; do
  if [[ -d "$d" ]]; then
    printf '[mysqld]\nskip-name-resolve\n' >"$d/99-e2e.cnf"
    break
  fi
done

unit=mariadb.service
systemctl cat "$unit" >/dev/null 2>&1 || unit=mysql.service
# Restart, not just start: the Debian packages start the server on install,
# before the configuration above existed.
systemctl enable "$unit" >/dev/null 2>&1
systemctl restart "$unit"
check "database server is running" wait_until 60 svc_active "$unit"
client=mariadb
command -v mariadb >/dev/null 2>&1 || client=mysql
check_eq "skip-name-resolve is on" "1" "$("$client" -uroot -Nse 'SELECT @@skip_name_resolve')"

finish
