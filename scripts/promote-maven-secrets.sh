#!/usr/bin/env bash
# Promote the four Maven Central credentials to ORG-level secrets on
# goliajp, so every repository publishing under jp.golia shares one set
# and a rotation happens in one place.
#
#   bash scripts/promote-maven-secrets.sh <path-to-armoured-signing-key>
#
# Three of the four already live on this machine, in the Gradle
# properties file under the vanniktech plugin's names:
#
#   CENTRAL_USERNAME  ← mavenCentralUsername
#   CENTRAL_PASSWORD  ← mavenCentralPassword
#   SIGNING_PASSWORD  ← signingInMemoryKeyPassword
#
# The fourth, SIGNING_KEY, is the armoured PRIVATE key and is NOT here:
# the local keyring holds FBD802632CFAD78B (smix SDK release signing),
# while the artifacts actually on Central under jp.golia are signed by
# 22BD3D63FE94A270 — a different key, whose public half is on
# keyserver.ubuntu.com with its user ID intact. Export the private half
# from wherever it lives and pass the file:
#
#   gpg --export-secret-keys --armor 22BD3D63FE94A270 > /tmp/signing.asc
#   bash scripts/promote-maven-secrets.sh /tmp/signing.asc
#   shred -u /tmp/signing.asc     # or rm, on a filesystem without shred
#
# No value is ever printed. `gh secret set` encrypts locally before
# sending, and this script pipes values on stdin so none of them can
# land in shell history or a log.
set -euo pipefail

ORG=goliajp
REPOS=kevy,sentori
GRADLE_PROPS="$HOME/.gradle/gradle.properties"

KEYFILE="${1:-}"
if [ -z "$KEYFILE" ] || [ ! -f "$KEYFILE" ]; then
    echo "usage: $0 <path-to-armoured-signing-key>" >&2
    echo "  gpg --export-secret-keys --armor 22BD3D63FE94A270 > /tmp/signing.asc" >&2
    exit 1
fi
grep -q "BEGIN PGP PRIVATE KEY BLOCK" "$KEYFILE" || {
    echo "✗ $KEYFILE is not an armoured PGP private key" >&2
    echo "  (a public key or a key id will be accepted by GitHub and then" >&2
    echo "   fail at signing time, which is a far more expensive place to" >&2
    echo "   find out)" >&2
    exit 1
}

# The key in the file must be the one Central already trusts, or the
# first publish fails validation against a signature nobody can check.
FPR="$(gpg --batch --import-options show-only --import "$KEYFILE" 2>/dev/null \
    | grep -Eo '[0-9A-F]{40}' | head -1)"
case "$FPR" in
    *22BD3D63FE94A270) ;;
    "") echo "✗ could not read a key id out of $KEYFILE" >&2; exit 1;;
    *)  echo "✗ that key is ${FPR: -16}, but the artifacts on Central under" >&2
        echo "  jp.golia are signed by 22BD3D63FE94A270. Publishing with a" >&2
        echo "  different key means publishing under a different signing" >&2
        echo "  identity — a deliberate decision, not a default. If that is" >&2
        echo "  what you want, upload its public half to" >&2
        echo "  keyserver.ubuntu.com first and edit this check." >&2
        exit 1;;
esac

read_prop() {
    python3 -c "
import re, pathlib, sys
s = pathlib.Path('$GRADLE_PROPS').read_text()
m = re.search(r'^\s*$1\s*=\s*(.*)\$', s, re.M)
sys.stdout.write(m.group(1).strip() if m else '')
"
}

set_secret() { # name, value on stdin
    printf '%s' "$2" | gh secret set "$1" --org "$ORG" \
        --visibility selected --repos "$REPOS" --body -
    echo "  ✓ $1"
}

echo "→ reading three from ${GRADLE_PROPS/#$HOME/\~}"
for pair in "CENTRAL_USERNAME:mavenCentralUsername" \
            "CENTRAL_PASSWORD:mavenCentralPassword" \
            "SIGNING_PASSWORD:signingInMemoryKeyPassword"; do
    name="${pair%%:*}" prop="${pair##*:}"
    value="$(read_prop "$prop")"
    [ -n "$value" ] || { echo "✗ $prop is not in $GRADLE_PROPS" >&2; exit 1; }
    set_secret "$name" "$value"
done

echo "→ the signing key from $KEYFILE"
set_secret SIGNING_KEY "$(cat "$KEYFILE")"

echo "→ what the org holds now"
gh api "orgs/$ORG/actions/secrets" -q '.secrets[] | "  " + .name + "  (" + .visibility + ")"'

cat <<'NOTE'

Next, and it matters: the four still exist as REPOSITORY secrets on
sentori, and a repository secret shadows an organization one of the
same name. Until they are deleted, sentori keeps using its own copies —
which is the drift this promotion exists to end.

    gh secret delete CENTRAL_USERNAME -R goliajp/sentori
    gh secret delete CENTRAL_PASSWORD -R goliajp/sentori
    gh secret delete SIGNING_KEY      -R goliajp/sentori
    gh secret delete SIGNING_PASSWORD -R goliajp/sentori

Delete them only after one sentori publish has run green on the org
copies — the point of the shadowing rule is that it lets you verify
before you remove the fallback.
NOTE
