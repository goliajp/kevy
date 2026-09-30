#!/usr/bin/env bash
# Make a deploy key the SSH identity for every later git step of a CI job.
#
#   bash scripts/deploy-key.sh "$KEY"
#
# github.com's host key is pinned rather than learned: accepting whatever
# answers on first contact would hand a write key to whoever answered.
# The value is the one GitHub publishes at https://api.github.com/meta
# (ssh_keys), fingerprint SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.
set -euo pipefail

KEY=${1:-}
[ -n "$KEY" ] || { echo "✗ deploy key is empty — is the secret set and passed to this job?" >&2; exit 1; }

dir="${RUNNER_TEMP:?}/deploy-ssh"
install -m 700 -d "$dir"
printf '%s\n' "$KEY" > "$dir/key"
chmod 600 "$dir/key"
echo "github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl" \
    > "$dir/known_hosts"

echo "GIT_SSH_COMMAND=ssh -i $dir/key -o IdentitiesOnly=yes -o UserKnownHostsFile=$dir/known_hosts -o StrictHostKeyChecking=yes" \
    >> "${GITHUB_ENV:?}"
git config --global user.name "github-actions[bot]"
git config --global user.email "41898282+github-actions[bot]@users.noreply.github.com"
