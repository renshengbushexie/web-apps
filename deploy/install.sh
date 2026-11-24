#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

fail() { echo "ERROR: $*" >&2; exit 1; }
[[ $EUID == 0 ]] || fail "run as root"
[[ $(uname -m) == x86_64 ]] || fail "this package requires Linux x86_64"
package=$(cd -- "$(dirname -- "$0")" && pwd -P)
root=/opt/rust-knowledge
conf=/etc/rust-knowledge
state=/var/lib/rust-knowledge
units=/etc/systemd/system
for cmd in install systemctl curl openssl flock getent readlink ss; do
    command -v "$cmd" >/dev/null || fail "missing command: $cmd"
done
exec 9>/run/lock/rust-knowledge-install.lock
flock -n 9 || fail "another installation is running"
for file in VERSION kb-api kb-sync kb-api.service kb-sync.service; do
    [[ -f $package/$file && ! -L $package/$file ]] || fail "missing regular package file: $file"
done
version=$(cat "$package/VERSION")
[[ $version =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || fail "VERSION must be a full Git hash"
release=$root/releases/$version
install -d -m 0755 "$root" "$root/releases"
previous=""
if [[ -L $root/current ]]; then
    previous=$(readlink -e "$root/current") || fail "invalid current release symlink"
    [[ $(dirname "$previous") == "$root/releases" ]] || fail "unmanaged current release"
    for file in kb-api kb-api.service kb-sync.service; do
        [[ -f $previous/$file ]] || fail "previous release is incomplete"
    done
elif [[ -e $root/current ]]; then
    fail "current release path is not a symlink"
fi
for service in kb-api kb-sync; do
    fragment=$(systemctl show -p FragmentPath --value "$service.service" 2>/dev/null || :)
    if [[ -n $fragment && $fragment != "$units/$service.service" ]]; then
        fail "existing unmanaged service: $service"
    fi
    if [[ -e $units/$service.service ]] &&
       ! grep -Fq "ExecStart=$root/current/$service" "$units/$service.service"; then
        fail "existing unmanaged unit at $units/$service.service"
    fi
done

if ! getent passwd rust-knowledge >/dev/null; then
    useradd --system --user-group --home-dir "$state" --no-create-home --shell /sbin/nologin rust-knowledge
fi
getent group rust-knowledge >/dev/null || fail "missing service group"
[[ $(id -u rust-knowledge) != 0 ]] || fail "service user must not be root"
install -d -m 0750 -o rust-knowledge -g rust-knowledge "$state"
install -d -m 0700 "$conf"
if [[ ! -e $conf/api.env ]]; then
    env_tmp=$(mktemp "$conf/.api.env.XXXXXX")
    admin=$(openssl rand -hex 32)
    printf '%s\n' "KB_ADMIN_TOKEN=$admin" 'KB_SHOP_TOKEN_HASHES={}' \
        'KB_BIND=127.0.0.1:8022' "KB_DB=$state/knowledge.sqlite3" > "$env_tmp"
    unset admin
    mv -T "$env_tmp" "$conf/api.env"
fi
[[ -f $conf/api.env && ! -L $conf/api.env ]] || fail "api.env must be a regular file"
chmod 0600 "$conf/api.env"
if [[ -d $release ]]; then
    for file in kb-api kb-sync kb-api.service kb-sync.service VERSION; do
        cmp -s "$package/$file" "$release/$file" || fail "different content already exists for $version"
    done
else
    staging=$(mktemp -d "$root/releases/.incoming.XXXXXX")
    chmod 0755 "$staging"
    install -m 0755 "$package/kb-api" "$package/kb-sync" "$staging/"
    install -m 0644 "$package/kb-api.service" "$package/kb-sync.service" "$package/VERSION" "$staging/"
    mv -T "$staging" "$release"
fi

point_current() {
    local target=$1 link_dir
    link_dir=$(mktemp -d "$root/.link.XXXXXX") || return 1
    ln -s "$target" "$link_dir/current" || return 1
    mv -Tf "$link_dir/current" "$root/current" || return 1
    rmdir "$link_dir"
}
write_units() {
    install -m 0644 "$1/kb-api.service" "$1/kb-sync.service" "$units/" || return 1
    systemctl daemon-reload
}
healthy() {
    local target=$1 attempts=${2:-45} pid body i
    for ((i=0; i<attempts; i++)); do
        pid=$(systemctl show -p MainPID --value kb-api.service 2>/dev/null) || pid=0
        if [[ $pid =~ ^[1-9][0-9]*$ ]] && [[ $(readlink "/proc/$pid/exe" 2>/dev/null) == "$target/kb-api" ]] &&
           systemctl is-active --quiet kb-api.service &&
           ss -H -ltnp 'sport = :8022' | grep -Fq "pid=$pid,"; then
            body=$(curl --noproxy '*' -fsS --connect-timeout 1 --max-time 2 http://127.0.0.1:8022/healthz 2>/dev/null) || body=""
            if [[ $body =~ \"status\"[[:space:]]*:[[:space:]]*\"ok\" && $body =~ \"version\"[[:space:]]*:[[:space:]]*[0-9]+ ]]; then
                return 0
            fi
        fi
        sleep 1
    done
    return 1
}

if [[ $previous == "$release" ]] &&
   cmp -s "$release/kb-api.service" "$units/kb-api.service" &&
   cmp -s "$release/kb-sync.service" "$units/kb-sync.service" &&
   healthy "$release" 1; then
    systemctl enable kb-api.service
    echo "Already running healthy Rust knowledge release: $version"
    exit 0
fi

sync_running=0
if systemctl is-active --quiet kb-sync.service; then sync_running=1; fi
rollback() {
    trap - ERR INT TERM
    set +e
    echo "Release activation failed; inspect journalctl -u kb-api.service" >&2
    if [[ -n $previous ]]; then
        if point_current "$previous" && write_units "$previous" &&
           systemctl restart kb-api.service && healthy "$previous"; then
            if ((sync_running)); then systemctl restart kb-sync.service; fi
            echo "Previous API release is healthy: $(basename "$previous")" >&2
        else
            echo "Previous release could not be verified; operator attention required" >&2
        fi
    else
        systemctl stop kb-api.service
        echo "No previous release. API is stopped; configuration and data are retained." >&2
    fi
    exit 1
}
trap rollback ERR INT TERM
if ((sync_running)); then systemctl stop kb-sync.service; fi
point_current "$release"
write_units "$release"
systemctl restart kb-api.service
healthy "$release"
systemctl enable kb-api.service
if ((sync_running)); then systemctl start kb-sync.service; fi
trap - ERR INT TERM
echo "Active healthy Rust knowledge release: $version"
