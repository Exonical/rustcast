#!/usr/bin/env bash
# Pin the flux-ldap outpost's auto-created API token to FLUX_LDAP_OUTPOST_TOKEN
# so the flux-authentik-ldap container can authenticate. Idempotent; retries
# until Authentik is up and the blueprint-applied outpost + token exist.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
set -a; . "$here/.env.authentik"; set +a
for i in $(seq 1 120); do
  out=$(podman exec flux-authentik-server ak shell -c "
from authentik.core.models import Token
from authentik.outposts.models import Outpost
o = Outpost.objects.filter(name='flux-ldap').first()
assert o, 'flux-ldap outpost not applied yet'
t = Token.objects.get(identifier=f'ak-outpost-{o.pk}-api')
t.key = '$FLUX_LDAP_OUTPOST_TOKEN'
t.expiring = False
t.save()
print('pinned', t.identifier)
" 2>&1)
  if printf '%s' "$out" | grep -q 'pinned ak-outpost'; then
    echo "done: pinned outpost token"
    exit 0
  fi
  sleep 5
done
echo "gave up after 10 minutes" >&2
exit 1
