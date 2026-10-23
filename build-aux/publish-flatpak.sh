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
#   FORGEJO_TOKEN      Forgejo Actions' automatic write token for this repo;
#                      local runs use the caller's SSH identity instead.
set -Eeuo pipefail

# flatpak build needs a session bus; a CI container has none of its own.
[[ -n ${DBUS_SESSION_BUS_ADDRESS:-} ]] || exec dbus-run-session -- "$0" "$@"

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$root"

app_id=ru.toxblh.Matras
app_branch=${FLATPAK_BRANCH:-stable}
repo_host=${FLATPAK_REPO_HOST:-altlinux.space}
repo_path=${FLATPAK_REPO_PATH:-toxblh/matras}
repo_branch=${FLATPAK_REPO_BRANCH:-flatpak}

# The Actions token can write to this repository, including its flatpak branch.
# Keep it out of the build environment and pass it only to Git via askpass.
git_token=${FORGEJO_TOKEN:-${GITHUB_TOKEN:-}}
unset FORGEJO_TOKEN GITHUB_TOKEN
if [[ -n $git_token ]]; then
  remote=https://$repo_host/$repo_path.git
  askpass_dir=$(mktemp -d)
  cat > "$askpass_dir/askpass" <<'ASKPASS'
#!/bin/sh
case "$1" in
  *Username*) printf '%s\n' 'oauth2' ;;
  *Password*) printf '%s\n' "$MATRAS_GIT_TOKEN" ;;
esac
ASKPASS
  chmod 700 "$askpass_dir/askpass"
  git_remote() {
    MATRAS_GIT_TOKEN="$git_token" GIT_ASKPASS="$askpass_dir/askpass" \
      GIT_TERMINAL_PROMPT=0 git "$@"
  }
else
  remote=ssh://forgejo@$repo_host/$repo_path.git
  git_remote() { git "$@"; }
fi

repo_dir=$root/.flatpak-repo
build_dir=$root/.flatpak-build
state_dir=$root/.flatpak-state

cleanup() {
  [[ -z ${gpg_cleanup_dir:-} ]] || rm -rf -- "$gpg_cleanup_dir"
  [[ -z ${askpass_dir:-} ]] || rm -rf -- "$askpass_dir"
}
trap cleanup EXIT

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
    gpg_cleanup_dir=$GNUPGHOME
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

if $publish; then
  # Start from what is already published so ostree can prune and delta
  # against it. `ls-remote` first, so an unreachable host fails here instead
  # of looking like an empty repository we are about to force-push over.
  rm -rf -- "$repo_dir"
  if git_remote ls-remote --exit-code --heads "$remote" "$repo_branch" >/dev/null; then
    git_remote clone --quiet --depth 1 --branch "$repo_branch" "$remote" "$repo_dir"
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
    git_remote ls-remote "$remote" >/dev/null
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
Homepage=https://altlinux.space/toxblh/matras
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

git_remote -C "$repo_dir" push --force "$remote" "snapshot:$repo_branch"

echo "published $app_id//$app_branch"
echo "install with: https://$repo_host/$repo_path/raw/branch/$repo_branch/$app_id.flatpakref"
