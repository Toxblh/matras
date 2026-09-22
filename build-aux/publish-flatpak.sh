#!/usr/bin/env bash
# Build Matras as a flatpak, and optionally publish the ostree repository that
# ru.toxblh.Matras.flatpakref points at.
#
#   build-aux/publish-flatpak.sh            build into .flatpak-repo, nothing else
#   build-aux/publish-flatpak.sh --publish  build, sign, and push that repository
#
# The repository is a plain ostree repo committed to a Forgejo branch and
# served over `…/raw/branch/<branch>`; there is no separate static host to
# keep alive. It is pushed as a single squashed commit every time, because a
# binary artifact store has no history worth keeping and Forgejo would carry
# every superseded object forever.
#
# Publishing reads from the environment:
#   FLATPAK_GPG_ID       fingerprint or uid of the signing key           (required)
#   FLATPAK_GPG_KEY_B64  base64 of that key exported with --export-secret-keys,
#                        for CI; omit it to sign with the local keyring
#   FLATPAK_GPG_PASSPHRASE  that key's passphrase, required alongside it
#   FLATPAK_REPO_SSH_KEY_B64  base64 of a private key registered as a write
#                        deploy key on the published repository; without it the
#                        push uses whatever SSH identity the caller already has
#   FLATPAK_REPO_KNOWN_HOSTS  the host's verified SSH host key, required
#                        alongside FLATPAK_REPO_SSH_KEY_B64
set -Eeuo pipefail

# flatpak build needs a session bus; a CI container has none of its own.
[[ -n ${DBUS_SESSION_BUS_ADDRESS:-} ]] || exec dbus-run-session -- "$0" "$@"

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$root"

app_id=ru.toxblh.Matras
app_branch=${FLATPAK_BRANCH:-stable}
repo_host=${FLATPAK_REPO_HOST:-altlinux.space}
repo_path=${FLATPAK_REPO_PATH:-toxblh/flatpak}
repo_branch=${FLATPAK_REPO_BRANCH:-master}
remote=ssh://forgejo@$repo_host/$repo_path.git

repo_dir=$root/.flatpak-repo
build_dir=$root/.flatpak-build
state_dir=$root/.flatpak-state

trap 'eval "${ssh_cleanup:-:}"' EXIT

publish=false
case ${1:-} in
  --publish) publish=true ;;
  '') ;;
  *) echo "usage: ${0##*/} [--publish]" >&2; exit 2 ;;
esac

# ALT ships GnuPG 1 as `gpg`; ostree needs 2.
gpg_bin=$(command -v gpg2 || command -v gpg) \
  || { echo 'gpg is required' >&2; exit 1; }
if command -v flatpak-builder >/dev/null; then
  builder=(flatpak-builder)
else
  builder=(flatpak run org.flatpak.Builder)
fi

sign=()
if [[ -n ${FLATPAK_GPG_ID:-} ]]; then
  if [[ -n ${FLATPAK_GPG_KEY_B64:-} ]]; then
    umask 077
    # Outside the checkout: gpg-agent puts its sockets in the homedir when
    # there is no /run/user, and the module source is a copy of the checkout,
    # which cannot contain a socket.
    GNUPGHOME=$(mktemp -d)
    export GNUPGHOME
    trap 'rm -rf -- "$GNUPGHOME"; eval "${ssh_cleanup:-:}"' EXIT
    [[ -n ${FLATPAK_GPG_PASSPHRASE:-} ]] \
      || { echo 'FLATPAK_GPG_KEY_B64 needs FLATPAK_GPG_PASSPHRASE' >&2; exit 1; }
    printf '%s' "$FLATPAK_GPG_PASSPHRASE" > "$GNUPGHOME/passphrase"
    unset FLATPAK_GPG_PASSPHRASE
    chmod 600 -- "$GNUPGHOME/passphrase"
    # gpg-agent runs pinentry for every use of a secret key, and the build
    # image ships none at all. ostree signs through gpgme, which cannot pass a
    # passphrase itself, so the answer has to come from a pinentry.
    cat > "$GNUPGHOME/pinentry" <<'PINENTRY'
#!/bin/sh
echo "OK Pleased to meet you"
while IFS= read -r line; do
  case "$line" in
    GETPIN*) printf 'D %s\nOK\n' "$(cat "${0%/*}/passphrase")" ;;
    BYE*)    echo "OK closing connection"; exit 0 ;;
    *)       echo "OK" ;;
  esac
done
PINENTRY
    chmod 700 -- "$GNUPGHOME/pinentry"
    printf 'pinentry-program %s/pinentry\nallow-loopback-pinentry\n' \
      "$GNUPGHOME" > "$GNUPGHOME/gpg-agent.conf"
    key=$FLATPAK_GPG_KEY_B64
    unset FLATPAK_GPG_KEY_B64
    printf '%s' "$key" | base64 -d | "$gpg_bin" --batch --quiet \
      --pinentry-mode loopback --passphrase-file "$GNUPGHOME/passphrase" --import
    unset key
    sign=(--gpg-sign="$FLATPAK_GPG_ID" --gpg-homedir="$GNUPGHOME")
  else
    sign=(--gpg-sign="$FLATPAK_GPG_ID")
  fi
fi

if $publish && (( ${#sign[@]} == 0 )); then
  echo 'refusing to publish an unsigned repository: set FLATPAK_GPG_ID' >&2
  exit 1
fi

if [[ -n ${FLATPAK_REPO_SSH_KEY_B64:-} ]]; then
  [[ -n ${FLATPAK_REPO_KNOWN_HOSTS:-} ]] \
    || { echo 'FLATPAK_REPO_SSH_KEY_B64 needs FLATPAK_REPO_KNOWN_HOSTS' >&2; exit 1; }
  umask 077
  ssh_dir=$(mktemp -d)
  ssh_cleanup="rm -rf -- $(printf %q "$ssh_dir")"
  key=$FLATPAK_REPO_SSH_KEY_B64
  hosts=$FLATPAK_REPO_KNOWN_HOSTS
  unset FLATPAK_REPO_SSH_KEY_B64 FLATPAK_REPO_KNOWN_HOSTS
  printf '%s' "$key" | base64 -d > "$ssh_dir/id"
  printf '%s\n' "$hosts" > "$ssh_dir/known_hosts"
  unset key hosts
  chmod 600 -- "$ssh_dir/id" "$ssh_dir/known_hosts"
  # The host key is pinned, never accepted on trust: this identity can write
  # to the repository every user installs from.
  export GIT_SSH_COMMAND="ssh -i $ssh_dir/id -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$ssh_dir/known_hosts"
fi

if $publish; then
  # Start from what is already published so ostree can prune and delta
  # against it. `ls-remote` first, so an unreachable host fails here instead
  # of looking like an empty repository we are about to force-push over.
  rm -rf -- "$repo_dir"
  if git ls-remote --exit-code --heads "$remote" "$repo_branch" >/dev/null; then
    git clone --quiet --depth 1 --branch "$repo_branch" "$remote" "$repo_dir"
    # git cannot carry an empty directory, and ostree wants all of these to
    # exist even while they hold nothing.
    mkdir -p -- "$repo_dir"/{extensions,state,tmp} \
      "$repo_dir"/refs/{heads,mirrors,remotes}
  else
    # No such branch — but distinguish "first publish" from "host down", or a
    # network blip would look like an empty repository and we would force-push
    # a fresh one over everything already published. Nothing is created here:
    # flatpak-builder refuses a --repo directory that exists but holds no
    # ostree, so the git repository is laid over the ostree one afterwards.
    git ls-remote "$remote" >/dev/null
  fi
fi

# rofiles-fuse cannot start inside a CI container: flatpak there believes it
# is sandboxed and routes fusermount through a portal that is not running.
"${builder[@]}" --force-clean --disable-rofiles-fuse \
  --state-dir="$state_dir" \
  --repo="$repo_dir" \
  --default-branch="$app_branch" \
  "$build_dir" "build-aux/$app_id.yml"

# Signing happens here rather than inside flatpak-builder: locally the builder
# is itself a flatpak, and the gpg-agent it starts in that sandbox has no
# pinentry to fall back on.
if (( ${#sign[@]} )); then
  flatpak build-sign "${sign[@]}" "$repo_dir"
fi

# build-update-repo leaves an unchanged appstream commit as it is, so one created
# by an earlier unsigned publish stays unsigned. Clients then reject the whole
# appstream branch ("GPG verification enabled, but no signatures found"), and
# software centres show no icon, description or screenshots for the app. Sign
# any appstream commit that has no signature before the summary is regenerated.
if (( ${#sign[@]} )); then
  while read -r ref; do
    commit=$(ostree rev-parse --repo="$repo_dir" "$ref")
    [[ -e $repo_dir/objects/${commit:0:2}/${commit:2}.commitmeta ]] && continue
    ostree gpg-sign --repo="$repo_dir" ${GNUPGHOME:+--gpg-homedir="$GNUPGHOME"} \
      "$commit" "$FLATPAK_GPG_ID"
  done < <(ostree refs --repo="$repo_dir" | grep '^appstream')
fi

flatpak build-update-repo \
  --title=Matras \
  --default-branch="$app_branch" \
  --generate-static-deltas \
  --prune --prune-depth=2 \
  ${sign[@]+"${sign[@]}"} \
  "$repo_dir"

if (( ${#sign[@]} )); then
  # The ref file is published beside the repository it points at, so its
  # embedded key can never drift from the key that signed the commit, and the
  # source tree never has to carry a copy.
  key_b64=$("$gpg_bin" --export "$FLATPAK_GPG_ID" | base64 -w0)
  cat > "$repo_dir/$app_id.flatpakref" <<EOF
[Flatpak Ref]
Title=Matras
Name=$app_id
Branch=$app_branch
Url=https://$repo_host/$repo_path/raw/branch/$repo_branch
Homepage=https://github.com/Toxblh/matras
RuntimeRepo=https://flathub.org/repo/flathub.flatpakrepo
IsRuntime=false
GPGKey=$key_b64
EOF
fi

$publish || { echo "built into $repo_dir; re-run with --publish to push it"; exit 0; }

# ostree's lock and scratch space are runtime state, not published content.
printf '%s\n' '.lock' 'tmp/' > "$repo_dir/.gitignore"

[[ -d $repo_dir/.git ]] || git init -q -b "$repo_branch" -- "$repo_dir"
git -C "$repo_dir" config user.name 'Matras CI'
git -C "$repo_dir" config user.email 'toxblh@gmail.com'
git -C "$repo_dir" checkout -q --orphan snapshot
git -C "$repo_dir" add -A
git -C "$repo_dir" commit -q -m "$app_id//$app_branch at $(date -u +%Y-%m-%dT%H:%M:%SZ)"

git -C "$repo_dir" push --force "$remote" "snapshot:$repo_branch"

echo "published $app_id//$app_branch"
echo "install with: https://$repo_host/$repo_path/raw/branch/$repo_branch/$app_id.flatpakref"
