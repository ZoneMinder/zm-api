#!/usr/bin/env bash
# No ZoneMinder on the host at all (a zm-next host, or zm-api installed
# first): the package creates its own account and still starts.
. /e2e/lib.sh

section "Install with no ZoneMinder present"
mv /etc/zm /etc/zm.absent
rm -rf /var/lib/zm-api /var/log/zm-api /run/zm

pkg_install >"$ARTIFACTS/zm-api-standalone-install.log" 2>&1 \
  || { tail -40 "$ARTIFACTS/zm-api-standalone-install.log"; exit 1; }

check "dedicated account created" id zoneminder
check_eq "service user" "zoneminder" "$(svc_prop zm-api.service User)"
check_eq "service group" "zoneminder" "$(svc_prop zm-api.service Group)"
check_eq "ZoneMinder's account is unmodified" \
  "$(cat "$STATE/web-user.before")" "$(id "$E2E_WEB_USER")"
for g in $(svc_prop zm-api.service SupplementaryGroups); do
  check "supplementary group $g exists" getent group "$g"
done

# With no zm.conf the database settings come from zm-api.env.
cat >>/etc/zm-api/zm-api.env <<ENV
APP_DB__HOST=localhost
APP_DB__USERNAME=zmuser
APP_DB__PASSWORD=zmpass
APP_DB__DATABASE_NAME=zm
ENV
systemctl enable zm-api.service >/dev/null 2>&1 || true
systemctl restart zm-api.service
check "zm-api is running" wait_until 60 svc_active zm-api.service
check "API answers" wait_until 60 api_up
check "socket directory created for the account" \
  test "$(stat -c %U /run/zm)" = zoneminder
check "login" login

finish
