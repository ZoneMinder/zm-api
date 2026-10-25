#!/usr/bin/env bash
# A working ZoneMinder built from upstream master, recording from a camera,
# before zm-api is anywhere near the host. Everything later is measured
# against this.
. /e2e/lib.sh

section "ZoneMinder master"
case "$E2E_ZM" in
  zmrepo-deb)
    codename="$(. /etc/os-release && echo "${VERSION_CODENAME}")"
    install -d /etc/apt/keyrings
    curl -fsSL https://zmrepo.zoneminder.com/debian/archive-keyring.gpg \
      | gpg --dearmor --yes -o /etc/apt/keyrings/zoneminder.gpg
    echo "deb [signed-by=/etc/apt/keyrings/zoneminder.gpg] https://zmrepo.zoneminder.com/debian/master ${codename}/" \
      >/etc/apt/sources.list.d/zoneminder.list
    apt-get update -qq
    # Its postinst creates the database and the zmuser account.
    sys_install zoneminder >"$ARTIFACTS/zoneminder-install.log" 2>&1 \
      || { tail -40 "$ARTIFACTS/zoneminder-install.log"; exit 1; }
    ;;
  zmrepo-rpm)
    # Upstream's own steps: zmrepo's instructions, then distros/redhat/readme.
    rel="$(rpm -E %rhel)"
    dnf -y install epel-release dnf-plugins-core >/dev/null
    dnf config-manager --set-enabled crb
    dnf -y install --nogpgcheck \
      "https://mirrors.rpmfusion.org/free/el/rpmfusion-free-release-${rel}.noarch.rpm" >/dev/null
    zmrepo="$(curl -fsSL "https://zmrepo.zoneminder.com/el/${rel}/$(uname -m)/" \
      | grep -oE 'zmrepo-[0-9][^"<]*\.noarch\.rpm' | sort -uV | tail -1)"
    # zmrepo's release package requires rhel-release, which Rocky and Alma do
    # not provide (they ship rocky-release / almalinux-release), so dnf refuses
    # it. It only drops a .repo file and a GPG key, so install it without
    # dependency checks.
    rpm -Uvh --nodeps "https://zmrepo.zoneminder.com/el/${rel}/$(uname -m)/${zmrepo}" >/dev/null
    # zmrepo's packages are signed with a SHA1 key, which EL9's default crypto
    # policy refuses to import ("Hash algorithm SHA1 not available"). This is a
    # throwaway lab host installing from a repository that calls itself a
    # testing repo, so skip the signature check for it.
    dnf -y install --nogpgcheck zoneminder >"$ARTIFACTS/zoneminder-install.log" 2>&1 \
      || { tail -40 "$ARTIFACTS/zoneminder-install.log"; exit 1; }
    mariadb -uroot </usr/share/zoneminder/db/zm_create.sql
    mariadb -uroot -e "CREATE USER 'zmuser'@'localhost' IDENTIFIED BY 'zmpass';
                       GRANT ALL PRIVILEGES ON zm.* TO 'zmuser'@'localhost'; FLUSH PRIVILEGES;"
    setenforce 0 2>/dev/null || true
    ;;
  *) echo "no ZoneMinder source for ${E2E_DISTRO}" >&2; exit 1 ;;
esac

version="$(db "SELECT Value FROM Config WHERE Name='ZM_DYN_DB_VERSION'")"
note "ZoneMinder schema version: ${version}"
echo "$version" >"$ARTIFACTS/zoneminder-version"
check "zmuser can't log in over TCP (skip-name-resolve)" tcp_login_refused
check_eq "ZoneMinder's daemons run as" "$E2E_WEB_USER" "$(zm_conf ZM_WEB_USER)"

section "Synthetic camera"
# H.264 with AAC over RTSP, the common case for an IP camera, so zmc has an
# encoded stream to put on its stream socket. mediamtx serves a clip that
# ffmpeg loops without re-encoding.
mtx=v1.21.1
case "$(uname -m)" in
  x86_64)  mtx_arch=amd64 ;;
  aarch64) mtx_arch=arm64 ;;
esac
mkdir -p /opt/e2e-camera
cd /opt/e2e-camera || exit 1
curl -fsSL "https://github.com/bluenviron/mediamtx/releases/download/${mtx}/mediamtx_${mtx}_linux_${mtx_arch}.tar.gz" \
  | tar -xz mediamtx
ffmpeg -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=size=640x360:rate=10" \
  -f lavfi -i "sine=frequency=440:sample_rate=16000" -t 20 \
  -c:v libx264 -profile:v baseline -pix_fmt yuv420p -g 20 -bf 0 -b:v 300k \
  -c:a aac -b:a 32k -y clip.mp4
# The same picture in H.265, which many cameras send and which needs its own
# path through zm-api (#212). passive.sh switches the monitor to it.
ffmpeg -hide_banner -loglevel error \
  -f lavfi -i "testsrc2=size=640x360:rate=10" \
  -f lavfi -i "sine=frequency=440:sample_rate=16000" -t 20 \
  -c:v libx265 -preset ultrafast -pix_fmt yuv420p -x265-params "keyint=20:bframes=0:log-level=error" \
  -tag:v hvc1 -b:v 300k -c:a aac -b:a 32k -y clip265.mp4
cat >mediamtx.yml <<YAML
logLevel: warn
rtmp: no
hls: no
webrtc: no
srt: no
paths:
  cam:
    runOnInit: ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i /opt/e2e-camera/clip.mp4 -c copy -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:8554/cam
    runOnInitRestart: yes
  cam265:
    runOnInit: ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i /opt/e2e-camera/clip265.mp4 -c copy -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:8554/cam265
    runOnInitRestart: yes
YAML
cat >/etc/systemd/system/e2e-camera.service <<UNIT
[Unit]
Description=Synthetic RTSP camera for the packaging e2e test
[Service]
ExecStart=/opt/e2e-camera/mediamtx /opt/e2e-camera/mediamtx.yml
Restart=always
[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now e2e-camera.service
check "camera is streaming" wait_until 30 \
  ffprobe -loglevel error -rtsp_transport tcp rtsp://127.0.0.1:8554/cam
check "H.265 camera is streaming" wait_until 30 \
  ffprobe -loglevel error -rtsp_transport tcp rtsp://127.0.0.1:8554/cam265

section "Monitor"
# Continuous recording with video passthrough: every section is an event, so
# "is it still recording" has an answer within seconds.
db "INSERT INTO Monitors
      (Name, Type, Capturing, Analysing, Recording, Decoding, Method, Path,
       Width, Height, Colours, VideoWriter, SaveJPEGs, SectionLength, MinSectionLength)
    VALUES
      ('${MONITOR_NAME}', 'Ffmpeg', 'Always', 'None', 'Always', 'KeyFrames', 'rtpRtsp',
       'rtsp://127.0.0.1:8554/cam', 640, 360, 4, 2, 0, 15, 5)"
db "UPDATE Users SET Password='${ADMIN_HASH}' WHERE Username='${ADMIN_USER}'"

systemctl enable --now zoneminder.service
check "zoneminder.service is running" wait_until 60 svc_active zoneminder.service
check "zmc is capturing" wait_until 90 capturing
check "an event is recorded to disk" wait_until 90 recorded_after 0
check_eq "zmc runs as" "$E2E_WEB_USER" "$(zmc_user)"
check "stream socket exists" test -S "$(socks_dir)/stream_$(monitor_id).sock"

# What the install must leave exactly as it found it.
id "$E2E_WEB_USER" >"$STATE/web-user.before"
zmc_pid >"$STATE/zmc-pid.before"
inode "$(socks_dir)/zmdc.sock" >"$STATE/zmdc-sock.before"

finish
