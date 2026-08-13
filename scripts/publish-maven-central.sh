#!/usr/bin/env bash
# Publish jp.golia:kevy — the Java client — to Maven Central.
#
#   bash scripts/publish-maven-central.sh            # validate only
#   bash scripts/publish-maven-central.sh --publish   # validate, then publish
#
# Ported from sentori's script, which learned three things the hard way
# and is the reason this one does not have to:
#
#   1. The Portal takes a **zipped bundle by POST**, not a Maven
#      repository by PUT. A `distributionManagement` block aimed at the
#      upload endpoint looks right and cannot work. So: deploy to a
#      LOCAL staging repo, zip it, POST the zip.
#   2. The signing key's public half must be on a keyserver Central
#      reads, **with its user ID intact**. keys.openpgp.org strips the
#      UID until the address is verified by email, and GnuPG refuses to
#      import a UID-less key — so a key that lives only there cannot be
#      checked by anyone. keyserver.ubuntu.com serves it whole.
#   3. `PUBLISHED` is the Portal's word for it. What decides whether a
#      stranger can depend on this is whether repo1 serves the files,
#      so the last thing this script does is ask repo1.
#
# Requires (org-level GitHub secrets, so every repo that publishes under
# jp.golia shares one set):
#   CENTRAL_USERNAME / CENTRAL_PASSWORD  — a Portal user token
#   SIGNING_KEY / SIGNING_PASSWORD       — an armoured private key
#
# Publishing is irreversible: a released version cannot be withdrawn.
# Without `--publish` this stops at VALIDATED, which can be dropped.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PUBLISH=0
[ "${1:-}" = "--publish" ] && PUBLISH=1

for v in CENTRAL_USERNAME CENTRAL_PASSWORD SIGNING_KEY SIGNING_PASSWORD; do
  if [ -z "${!v:-}" ]; then
    echo "✗ $v is not set — this cannot sign or upload without it." >&2
    echo "  They are org-level secrets on goliajp; a repo that needs them" >&2
    echo "  must be in the secret's repository-access list." >&2
    exit 1
  fi
done

command -v mvn >/dev/null || { echo "✗ mvn is not on PATH" >&2; exit 1; }

POM="bindings/java/pom.xml"
# The project's own <version> — matched via its artifactId so a plugin's
# <version> can never be picked up instead. (A heredoc inside $( ) does
# not parse in bash, which is how the first draft of this line failed
# `bash -n`.)
VERSION="$(python3 -c 'import re,pathlib; print(re.search(r"<artifactId>kevy</artifactId>\s*<version>([^<]+)</version>", pathlib.Path("bindings/java/pom.xml").read_text()).group(1))')"
COORD="jp.golia:kevy:${VERSION}"
STAGING="$ROOT/bindings/java/target/staging-repo"
WORK="$(mktemp -d)"
BUNDLE="$ROOT/bindings/java/target/kevy-${VERSION}-bundle.zip"
GROUP_PATH="jp/golia/kevy"

# The version in the tree must match the tag this release is cut from,
# or Central gets sources no tag describes.
if [ -n "${GITHUB_REF_NAME:-}" ]; then
  case "$GITHUB_REF_NAME" in
    "v${VERSION}") ;;
    *) echo "✗ pom says ${VERSION} but the ref is ${GITHUB_REF_NAME}" >&2; exit 1;;
  esac
fi

echo "→ import the signing key"
export GNUPG_HOME_TMP="$(mktemp -d)"
chmod 700 "$GNUPG_HOME_TMP"
printf '%s' "$SIGNING_KEY" | GNUPGHOME="$GNUPG_HOME_TMP" gpg --batch --import 2>/dev/null
KEYID="$(GNUPGHOME="$GNUPG_HOME_TMP" gpg --batch --list-secret-keys --with-colons \
  | awk -F: '/^fpr:/{print $10; exit}')"
[ -n "$KEYID" ] || { echo "✗ could not read a key id out of SIGNING_KEY" >&2; exit 1; }
echo "      ${KEYID}"

echo "→ stage ${COORD} (signed, sources + javadoc)"
rm -rf "$STAGING"
GNUPGHOME="$GNUPG_HOME_TMP" mvn -q -f "$POM" -Prelease \
  -Dgpg.passphrase="$SIGNING_PASSWORD" \
  -DskipTests \
  -DaltDeploymentRepository="staging::default::file://${STAGING}" \
  deploy

DIR="${STAGING}/${GROUP_PATH}/${VERSION}"
[ -d "$DIR" ] || { echo "✗ nothing staged at ${DIR}" >&2; exit 1; }

# Central requires all three artifacts plus a signature for each. Check
# here, where the message can name the missing file, rather than letting
# the Portal answer with a validation error twenty minutes later.
for f in "kevy-${VERSION}.pom" "kevy-${VERSION}.jar" \
         "kevy-${VERSION}-sources.jar" "kevy-${VERSION}-javadoc.jar"; do
  [ -f "${DIR}/${f}" ] || { echo "✗ ${f} was not staged" >&2; exit 1; }
  [ -f "${DIR}/${f}.asc" ] || { echo "✗ ${f} has no signature" >&2; exit 1; }
done

echo "→ signatures verify against the PUBLISHED public key"
# Against a keyring holding only what a stranger can fetch. Verifying
# with the local secret keyring proves the file was signed here, not
# that anyone else can check it — which is the thing Central does.
VK="$(mktemp -d)"; chmod 700 "$VK"
curl -sS -m 60 -4 "https://keyserver.ubuntu.com/pks/lookup?op=get&search=0x${KEYID}" \
  | GNUPGHOME="$VK" gpg --batch --import >/dev/null 2>&1 || true
if ! GNUPGHOME="$VK" gpg --batch --verify "${DIR}/kevy-${VERSION}.pom.asc" \
     "${DIR}/kevy-${VERSION}.pom" >/dev/null 2>&1; then
  echo "✗ the signature does not verify against the key on keyserver.ubuntu.com." >&2
  echo "  Central checks it the same way, and would refuse the bundle." >&2
  echo "  Publish the public half:" >&2
  echo "    gpg --export --armor ${KEYID} > pub.asc" >&2
  echo "    curl -4 -X POST https://keyserver.ubuntu.com/pks/add --data-urlencode keytext@pub.asc" >&2
  exit 1
fi
echo "      good signature from ${KEYID}"

echo "→ bundle"
rm -f "$BUNDLE"
# Artifacts, their signatures and their checksums. Not
# `maven-metadata.xml` (the Portal writes its own), and not checksums
# *of* the signatures.
(cd "$STAGING" && find jp -type f \
  ! -name 'maven-metadata.xml*' \
  ! -name '*.asc.md5' ! -name '*.asc.sha1' \
  ! -name '*.asc.sha256' ! -name '*.asc.sha512' -print0) \
  | while IFS= read -r -d '' f; do
      mkdir -p "${WORK}/$(dirname "$f")"
      cp "${STAGING}/${f}" "${WORK}/${f}"
    done
(cd "$WORK" && zip -qr "$BUNDLE" jp)
echo "      $(cd "$WORK" && find jp -type f | wc -l | tr -d ' ') files, $(wc -c < "$BUNDLE") bytes"

AUTH="$(printf '%s:%s' "$CENTRAL_USERNAME" "$CENTRAL_PASSWORD" | base64 | tr -d '\n')"

echo "→ upload"
ID="$(curl -sS -m 600 -X POST \
  -H "Authorization: Bearer ${AUTH}" \
  -F "bundle=@${BUNDLE}" \
  "https://central.sonatype.com/api/v1/publisher/upload?name=${COORD}&publishingType=USER_MANAGED")"
case "$ID" in
  *-*-*-*-*) ;;
  *) echo "✗ upload did not return a deployment id: ${ID}" >&2; exit 1;;
esac
echo "      ${ID}"

state() {
  curl -sS -m 60 -X POST -H "Authorization: Bearer ${AUTH}" \
    "https://central.sonatype.com/api/v1/publisher/status?id=${ID}"
}
field() { python3 -c "import sys,json;print(json.load(sys.stdin).get('$1'))"; }

echo "→ validation"
ST=""
for _ in $(seq 1 60); do
  S="$(state)"
  ST="$(printf '%s' "$S" | field deploymentState)"
  case "$ST" in
    VALIDATED|PUBLISHED) break;;
    FAILED)
      echo "✗ ${ST}" >&2
      printf '%s' "$S" | python3 -m json.tool >&2
      exit 1;;
  esac
  sleep 10
done
[ "$ST" = "VALIDATED" ] || [ "$ST" = "PUBLISHED" ] || {
  echo "✗ still ${ST:-unknown} after ten minutes" >&2; exit 1; }
printf '%s' "$S" | python3 -c "
import sys, json
for w in (json.load(sys.stdin).get('warnings') or []):
    print(f'      warning: {w}')
" || true
echo "      ${ST}"

if [ "$PUBLISH" -eq 0 ]; then
  echo "✓ ${COORD} validated. It is NOT published — rerun with --publish,"
  echo "  or drop it at https://central.sonatype.com/publishing/deployments"
  exit 0
fi

echo "→ publish (irreversible)"
curl -sS -m 120 -X POST -H "Authorization: Bearer ${AUTH}" \
  "https://central.sonatype.com/api/v1/publisher/deployment/${ID}" >/dev/null

for _ in $(seq 1 60); do
  ST="$(state | field deploymentState)"
  [ "$ST" = "PUBLISHED" ] && break
  [ "$ST" = "FAILED" ] && { echo "✗ publish failed" >&2; exit 1; }
  sleep 20
done
[ "$ST" = "PUBLISHED" ] || { echo "✗ still ${ST} after twenty minutes" >&2; exit 1; }

# The Portal saying PUBLISHED is not the same as a stranger being able
# to depend on it. Ask the thing their build will ask.
echo "→ resolvable from repo1"
BASE="https://repo1.maven.org/maven2/${GROUP_PATH}/${VERSION}"
CODE=""
for _ in $(seq 1 40); do
  CODE="$(curl -sS -m 40 -o /dev/null -w '%{http_code}' "${BASE}/kevy-${VERSION}.pom")"
  [ "$CODE" = "200" ] && break
  sleep 30
done
[ "$CODE" = "200" ] || { echo "✗ repo1 still answers ${CODE} for the POM" >&2; exit 1; }
for f in ".jar" "-sources.jar" "-javadoc.jar" ".pom.asc"; do
  CODE="$(curl -sS -m 40 -o /dev/null -w '%{http_code}' "${BASE}/kevy-${VERSION}${f}")"
  [ "$CODE" = "200" ] || { echo "✗ repo1 answers ${CODE} for kevy-${VERSION}${f}" >&2; exit 1; }
  echo "      kevy-${VERSION}${f}"
done

echo "✓ ${COORD} is on Maven Central"
