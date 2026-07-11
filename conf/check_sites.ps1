param(
    [string]$HostName = $env:AS_HOSTNAME,
    [string]$User = $env:AS_USER,
    [string]$Port = $env:AS_PORT,
    [string]$Key = $env:AS_KEY,
    [string]$Ssh = $env:AS_SSH,
    [string[]]$Urls = @(
        'https://code.xiedeacc.com',
        'https://unlock-music.xiedeacc.com',
        'https://halo.xiedeacc.com',
        'https://immich.xiedeacc.com',
        'https://blog.xiedeacc.com',
        'https://rblog.xiedeacc.com'
    )
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($HostName)) { $HostName = '192.168.2.247' }
if ([string]::IsNullOrWhiteSpace($User)) { $User = 'root' }
if ([string]::IsNullOrWhiteSpace($Port)) { $Port = '10022' }
if ([string]::IsNullOrWhiteSpace($Ssh)) { $Ssh = 'ssh' }

$sshArgs = @('-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=accept-new', '-o', 'ConnectTimeout=20', '-p', $Port)
if (-not [string]::IsNullOrWhiteSpace($Key)) { $sshArgs += @('-i', $Key) }
$dest = "$User@$HostName"

$urlLines = ($Urls | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }) -join "`n"
$remoteTemplate = @'
set -u
failed=0
urls=$(cat <<'EOF_URLS'
__URL_LINES__
EOF_URLS
)
while IFS= read -r url; do
    [ -n "$url" ] || continue
    host="${url#https://}"
    host="${host%%/*}"
    tmp="$(mktemp)"
    body_tmp="$(mktemp)"
    code="$(curl -k -sS -D "$tmp" -o "$body_tmp" --max-time 20 "$url" -w '%{http_code}' 2>/tmp/check_site_err || true)"
    err="$(cat /tmp/check_site_err 2>/dev/null || true)"
    location="$(grep -i -m1 '^location:' "$tmp" | sed -E 's/^[Ll]ocation:[[:space:]]*//; s/[[:space:]]*\r?$//' || true)"
    gitlab_meta="$(grep -iq '^x-gitlab-meta:' "$tmp" && printf yes || true)"
    content_type="$(grep -i -m1 '^content-type:' "$tmp" | sed -E 's/^[Cc]ontent-[Tt]ype:[[:space:]]*//; s/[[:space:]]*\r?$//' || true)"
    final_code="$(curl -k -L -sS -o /dev/null --max-time 30 --max-redirs 5 "$url" -w '%{http_code}' 2>/dev/null || true)"
    status=OK
    reason=
    case "$host" in
        code.xiedeacc.com)
            if [ "$code" != "302" ] || ! printf '%s' "$location" | grep -q '/users/sign_in'; then
                status=FAIL
                reason="expected GitLab login redirect"
            fi
            ;;
        *)
            if [ "$gitlab_meta" = "yes" ] || printf '%s' "$location" | grep -q '/users/sign_in'; then
                status=FAIL
                reason="unexpected GitLab login redirect"
            elif ! printf '%s' "$code" | grep -Eq '^(2|3)[0-9][0-9]$'; then
                status=FAIL
                reason="unexpected status"
            fi
            ;;
    esac
    printf '%-34s status=%s final=%s gitlab=%s content=%s location=%s %s\n' "$url" "$code" "$final_code" "${gitlab_meta:-no}" "${content_type:-unknown}" "${location:--}" "$status"
    if [ -n "$err" ]; then printf '  curl_error=%s\n' "$err"; fi
    if [ "$status" != OK ]; then
        [ -n "$reason" ] && printf '  reason=%s\n' "$reason"
        failed=1
    fi
    rm -f "$tmp" "$body_tmp" /tmp/check_site_err
done <<EOF_CHECK_URLS
$urls
EOF_CHECK_URLS
exit "$failed"
'@
$remote = $remoteTemplate.Replace('__URL_LINES__', $urlLines)

$bytes = [Text.Encoding]::UTF8.GetBytes($remote)
$encoded = [Convert]::ToBase64String($bytes)
& $Ssh @sshArgs $dest "echo $encoded | base64 -d | bash"
exit $LASTEXITCODE
