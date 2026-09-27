#!/usr/bin/env bash
# Rejoue contre un device réel les campagnes de test de l'API v1alpha1
# (contrat §4/§4a/§6) faites à la main pendant le dev -- pour éviter les
# régressions au lieu de retaper les mêmes curl à chaque fois.
#
# Usage :
#   scripts/test-api.sh <url> <token> [safe]        # défaut : tests non-destructifs, rejouables à volonté
#   scripts/test-api.sh <url> <token> reboot         # POST /reboot puis attend que le device revienne
#   scripts/test-api.sh <url> <token> rotate-token   # POST /token -- change l'auth, affiche le nouveau token
#   scripts/test-api.sh <url> <token> ota-activate   # active le slot déjà stagé par `safe` -- coupe le device en l'air, voir l'avertissement dans la fonction
#   scripts/test-api.sh <url> <token> push-cert <cert.pem> <key.pem>  # POST /tls/cert -- bascule le serveur admin en HTTPS:443
#   scripts/test-api.sh <url> <token> push-ca <ca.pem>                 # POST /tls/ca -- CA à vérifier pour heartbeat/logs sortants
#   scripts/test-api.sh <url> <token> push-firmware <image.bin> [deployment-id]
#       # prepare + écriture Content-Range chunkée (16 Ko), TOUS les chunks
#       # sur une seule connexion TCP+TLS (curl -K/--next) -- chemin normal/
#       # rapide. Un PUT monolithique (tout le fichier d'un coup, sans
#       # Content-Range) échoue de façon reproductible sur ce device
#       # ("Empty reply from server" pour ~1,2 Mo) : ce n'était pas le
#       # nombre de requêtes qui coûtait cher, c'était une poignée de main
#       # TLS par requête -- mesuré ~88% du temps total avant ce correctif.
#   scripts/test-api.sh <url> <token> push-firmware-resumable <image.bin> [deployment-id]
#       # mode de test de reprise : écriture Content-Range chunkée (16 Ko),
#       # pilotée par written=N que le device rapporte (partial ou 416),
#       # une connexion (donc une poignée de main TLS) par chunk --
#       # délibérément plus lent, c'est le prix pour exercer le protocole de
#       # resync après un accroc réseau simulé entre deux chunks.
#
# `safe` ne laisse aucun effet de bord dangereux : il stage un faux binaire
# de test sur le slot inactif (visible dans `GET /info`'s `staged` jusqu'au
# prochain vrai cycle OTA), mais ne touche jamais au boot ni à l'auth. Les
# autres sous-commandes sont volontairement séparées, jamais groupées dans
# un mode "tout lancer" : chacune a un effet de bord réel sur le device
# (`push-cert` remplace l'identité serveur mais ne change jamais de protocole).
set -euo pipefail

URL="${1:?Usage: $0 <url> <token> [safe|reboot|rotate-token|ota-activate|push-cert|push-ca]}"
TOKEN="${2:?Usage: $0 <url> <token> [safe|reboot|rotate-token|ota-activate|push-cert|push-ca]}"
MODE="${3:-safe}"
URL="${URL%/}"

if [[ "$URL" != https://* ]]; then
    echo "REFUS: l'API Embewi est HTTPS-only; URL attendue: https://..." >&2
    exit 2
fi

PASS=0
FAIL=0

# --- petits utilitaires -------------------------------------------------

# jget <json> <clé> : extrait une valeur top-level (ou "a.b" en profondeur)
# d'une réponse JSON. jq n'est pas garanti présent sur toutes les machines
# de dev ESP -- python3, lui, l'est presque toujours (esptool/espflash
# eux-mêmes en dépendent).
jget() {
    python3 -c '
import json, sys
try:
    data = json.loads(sys.argv[1])
    for key in sys.argv[2].split("."):
        data = data[key]
    print(data if not isinstance(data, (dict, list)) else json.dumps(data))
except Exception:
    print("")
' "$1" "$2"
}

check() {
    local desc="$1" got="$2" want="$3"
    if [[ "$got" == "$want" ]]; then
        echo "  OK   $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL $desc (attendu=$want obtenu=$got)"
        FAIL=$((FAIL + 1))
    fi
}

check_ne() {
    local desc="$1" got="$2" not_want="$3"
    if [[ "$got" != "$not_want" ]]; then
        echo "  OK   $desc"
        PASS=$((PASS + 1))
    else
        echo "  FAIL $desc (n'aurait pas dû valoir '$not_want')"
        FAIL=$((FAIL + 1))
    fi
}

# -k : l'identité bootstrap est auto-signée tant qu'aucune PKI n'a remplacé
# le certificat. Le script refuse HTTP avant toute requête.
auth_get() { curl -sk -m 10 -H "Authorization: Bearer $TOKEN" "$URL$1"; }
auth_post() { curl -sk -m 10 -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$2" "$URL$1"; }
http_code() { curl -sk -o /dev/null -w "%{http_code}" -m 10 "$@"; }

# --- campagne "safe" (contrat §4, non-destructif) ------------------------

run_safe() {
    echo "== GET /v1alpha1/info =="
    local info; info=$(auth_get /v1alpha1/info)
    check "node_id non vide" "$([[ -n "$(jget "$info" node_id)" ]] && echo yes)" "yes"
    check "api_versions contient v1alpha1" "$(jget "$info" api_versions)" '["v1alpha1"]'
    check "partition_layout" "$(jget "$info" partition_layout)" "embewi-ab-v1"
    check "state == running" "$(jget "$info" state)" "running"
    check_ne "active_slot non vide (MMU, pas otadata)" "$(jget "$info" active_slot)" ""
    # `boot` est lu directement dans `otadata` (EWBT), indépendamment de `active_slot`
    # (MMU) : un rollback se voit ici comme boot.slot != active_slot / boot.state invalid.
    check "boot.slot == active_slot (otadata == MMU)" "$(jget "$info" boot.slot)" "$(jget "$info" active_slot)"
    check "boot.state == valid (EWBT)" "$(jget "$info" boot.state)" "valid"
    check_ne "boot.seq > 0" "$(jget "$info" boot.seq)" "0"
    check_ne "firmware.name non vide" "$(jget "$info" firmware.name)" ""
    check_ne "ram_size non vide" "$(jget "$info" ram_size)" ""

    echo "== GET /v1alpha1/health =="
    local health; health=$(auth_get /v1alpha1/health)
    check "status == ok" "$(jget "$health" status)" "ok"
    check "checks.storage == ok" "$(jget "$health" checks.storage)" "ok"

    echo "== GET /v1alpha1/config =="
    local config; config=$(auth_get /v1alpha1/config)
    check_ne "generation présent" "$(jget "$config" generation)" ""

    echo "== Auth =="
    check "sans token -> 401" "$(http_code "$URL/v1alpha1/info")" "401"
    check "mauvais token -> 401" "$(http_code -H 'Authorization: Bearer wrong' "$URL/v1alpha1/info")" "401"

    echo "== POST /v1alpha1/ota/prepare (compat) =="
    local chip; chip=$(jget "$info" chip)
    local bad_chip; bad_chip=$(auth_post /v1alpha1/ota/prepare "{\"deployment_id\":\"test\",\"digest\":\"sha256:x\",\"size\":1,\"chip\":\"bidon\",\"partition_layout\":\"embewi-ab-v1\"}")
    check "chip_mismatch" "$(jget "$bad_chip" reason)" "chip_mismatch"
    local bad_layout; bad_layout=$(auth_post /v1alpha1/ota/prepare "{\"deployment_id\":\"test\",\"digest\":\"sha256:x\",\"size\":1,\"chip\":\"$chip\",\"partition_layout\":\"bidon\"}")
    check "layout_mismatch" "$(jget "$bad_layout" reason)" "layout_mismatch"
    local ok_prepare; ok_prepare=$(auth_post /v1alpha1/ota/prepare "{\"deployment_id\":\"test-api-sh\",\"digest\":\"sha256:x\",\"size\":1,\"chip\":\"$chip\",\"partition_layout\":\"embewi-ab-v1\"}")
    check "accepted" "$(jget "$ok_prepare" accepted)" "True"
    check_ne "target_slot renvoyé" "$(jget "$ok_prepare" target_slot)" ""

    echo "== PUT /v1alpha1/ota/write (écriture monolithique) =="
    local tmp; tmp=$(mktemp)
    printf 'embewi-test-api-sh-payload' > "$tmp"
    local expected; expected="sha256:$(sha256sum "$tmp" | cut -d' ' -f1)"
    local write; write=$(curl -sk -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh" -H "X-Embewi-Digest: $expected" \
        --data-binary @"$tmp" "$URL/v1alpha1/ota/write")
    check "status == written" "$(jget "$write" status)" "written"
    check "digest calculé == attendu" "$(jget "$write" digest)" "$expected"

    echo "== PUT /v1alpha1/ota/write (digest volontairement faux) =="
    local bad; bad=$(curl -sk -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh-bad" -H "X-Embewi-Digest: sha256:0000000000000000000000000000000000000000000000000000000000000000" \
        --data-binary @"$tmp" "$URL/v1alpha1/ota/write")
    check "digest_mismatch" "$(jget "$bad" status)" "digest_mismatch"
    # `write_begin` (contrat post-2026-09-23 : supersession avant écriture)
    # a déjà superseded/effacé test-api-sh dès le premier octet de ce PUT --
    # avant même que le digest de B ne puisse être vérifié, qui ne l'est
    # qu'à `write_finish`. Le slot physique peut donc déjà être partiellement
    # écrasé : NVS ne doit plus jamais prétendre que test-api-sh y est encore
    # intact. Fail-safe voulu : après un digest invalide, rien n'est
    # activable, jamais un candidat que le flash ne reflète plus fidèlement.
    local after; after=$(auth_get /v1alpha1/info)
    check "fail-safe : plus rien d'activable après un digest invalide (A a été superseded par le begin de B)" \
        "$(jget "$after" staged.state)" "none"

    echo "== PUT /v1alpha1/ota/write (reprise Content-Range) =="
    # `written` est désormais DURABLE (ota.rs: écriture sector-aware, un seul
    # erase+program par secteur de 4 Ko) : il ne peut avancer que par pas de
    # secteur, pas à chaque octet accepté. Le split est donc calé exactement
    # sur un secteur (premier chunk = 1 secteur plein, second = le reliquat)
    # pour exercer ce comportement précisément, pas un artefact de petite
    # taille de payload.
    local sector=4096
    local total=$((sector + 500))
    head -c "$total" /dev/urandom > "$tmp"
    local half=$sector
    head -c "$half" "$tmp" > "$tmp.part1"
    tail -c +"$((half + 1))" "$tmp" > "$tmp.part2"
    local expected_cr; expected_cr="sha256:$(sha256sum "$tmp" | cut -d' ' -f1)"
    local part1; part1=$(curl -sk -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh-cr" -H "X-Embewi-Digest: $expected_cr" \
        -H "Content-Range: bytes 0-$((half - 1))/$total" \
        --data-binary @"$tmp.part1" "$URL/v1alpha1/ota/write")
    check "1er chunk -> partial" "$(jget "$part1" status)" "partial"
    check "written == 1 secteur plein flushé (durable, pas juste accepté)" "$(jget "$part1" written)" "$half"
    local part2; part2=$(curl -sk -m 15 -X PUT -H "Authorization: Bearer $TOKEN" \
        -H "X-Embewi-Deployment-Id: test-api-sh-cr" -H "X-Embewi-Digest: $expected_cr" \
        -H "Content-Range: bytes $half-$((total - 1))/$total" \
        --data-binary @"$tmp.part2" "$URL/v1alpha1/ota/write")
    check "2e chunk -> written" "$(jget "$part2" status)" "written"
    check "digest final correct après reprise" "$(jget "$part2" digest)" "$expected_cr"

    echo "== PUT /v1alpha1/ota/write (resync sur mauvais offset) =="
    local resync; resync=$(curl -sk -m 15 -o /tmp/resync_body.json -w "%{http_code}" -X PUT \
        -H "Authorization: Bearer $TOKEN" -H "X-Embewi-Deployment-Id: test-api-sh-resync" \
        -H "X-Embewi-Digest: $expected_cr" \
        -H "Content-Range: bytes 999-$((999 + half - 1))/999999" --data-binary @"$tmp.part1" "$URL/v1alpha1/ota/write")
    check "offset erroné -> 416" "$resync" "416"
    check "erreur == range_mismatch" "$(jget "$(cat /tmp/resync_body.json)" error)" "range_mismatch"

    echo "== PUT /v1alpha1/ota/write (Content-Range malformé) =="
    local malformed; malformed=$(curl -sk -o /tmp/malformed_body.json -w "%{http_code}" -m 10 -X PUT \
        -H "Authorization: Bearer $TOKEN" -H "X-Embewi-Deployment-Id: test-api-sh-malformed" \
        -H "X-Embewi-Digest: $expected_cr" -H "Content-Range: n'importe-quoi" \
        --data-binary @"$tmp.part1" "$URL/v1alpha1/ota/write")
    check "header invalide -> 400" "$malformed" "400"
    check "erreur == bad_content_range" "$(jget "$(cat /tmp/malformed_body.json)" error)" "bad_content_range"

    # Protocole durci : chaque refus se fait avant de toucher la session.
    put_write() { # put_write <fichier> <dep-id> <digest> [Content-Range] -> "<code> <corps>"
        local file="$1" dep="$2" dig="$3" range="${4:-}"
        local args=(-sk -m 15 -o /tmp/put_body.json -w "%{http_code}" -X PUT -H "Authorization: Bearer $TOKEN")
        [[ -n "$dep" ]] && args+=(-H "X-Embewi-Deployment-Id: $dep")
        [[ -n "$dig" ]] && args+=(-H "X-Embewi-Digest: $dig")
        [[ -n "$range" ]] && args+=(-H "Content-Range: $range")
        local code; code=$(curl "${args[@]}" --data-binary @"$file" "$URL/v1alpha1/ota/write")
        echo "$code $(jget "$(cat /tmp/put_body.json)" error)"
    }
    echo "== PUT /v1alpha1/ota/write (validation des en-têtes) =="
    check "sans deployment_id -> 400" "$(put_write "$tmp.part1" "" "$expected_cr")" "400 missing_deployment_id"
    check "sans digest -> 400" "$(put_write "$tmp.part1" dep "")" "400 bad_digest"
    check "digest mal formé -> 400" "$(put_write "$tmp.part1" dep "sha256:abc")" "400 bad_digest"
    check "Content-Length != plage -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 0-1/$total")" "400 content_length_mismatch"
    check "plage inversée -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 10-5/$total")" "400 bad_content_range"
    check "fin >= total -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 0-$((half - 1))/$((half - 1))")" "400 bad_content_range"
    check "total = 0 -> 400" \
        "$(put_write "$tmp.part1" dep "$expected_cr" "bytes 0-$((half - 1))/0")" "400 bad_content_range"

    echo "== PUT /v1alpha1/ota/write (session figée : deployment_id / digest / total) =="
    check "1er chunk -> 200" "$(put_write "$tmp.part1" test-api-sh-sm "$expected_cr" "bytes 0-$((half - 1))/$total" | cut -d' ' -f1)" "200"
    check "autre deployment_id -> 409" \
        "$(put_write "$tmp.part2" other-dep "$expected_cr" "bytes $half-$((total - 1))/$total")" "409 session_mismatch"
    check "autre digest -> 409" \
        "$(put_write "$tmp.part2" test-api-sh-sm "sha256:$(printf '1%.0s' $(seq 64))" "bytes $half-$((total - 1))/$total")" "409 session_mismatch"
    check "autre total -> 409" \
        "$(put_write "$tmp.part2" test-api-sh-sm "$expected_cr" "bytes $half-$((total - 1))/$((total + 1))")" "409 session_mismatch"
    check "chunk final avec les bons paramètres -> 200" \
        "$(put_write "$tmp.part2" test-api-sh-sm "$expected_cr" "bytes $half-$((total - 1))/$total" | cut -d' ' -f1)" "200"

    echo "== POST /v1alpha1/ota/activate (deployment_id différent) =="
    local act; act=$(curl -sk -m 10 -o /tmp/act_body.json -w "%{http_code}" -X POST -H "Authorization: Bearer $TOKEN" \
        -H "Content-Type: application/json" -d '{"deployment_id":"pas-le-bon"}' "$URL/v1alpha1/ota/activate")
    check "activate d'un autre deployment -> 409 deployment_mismatch" \
        "$act $(jget "$(cat /tmp/act_body.json)" error)" "409 deployment_mismatch"
    check "staged.deployment_id inchangé" "$(jget "$(auth_get /v1alpha1/info)" staged.deployment_id)" "test-api-sh-sm"

    rm -f /tmp/put_body.json /tmp/act_body.json "$tmp" "$tmp.part1" "$tmp.part2" /tmp/resync_body.json /tmp/malformed_body.json

    echo
    echo "-- $PASS OK / $FAIL FAIL --"
    echo "Note : GET /info->staged pointe maintenant vers le faux binaire de ce" \
         "test (deployment_id=test-api-sh-sm) jusqu'au prochain vrai cycle OTA" \
         "(prepare+write+activate) -- ce n'est pas dangereux, juste cosmétique."
    [[ $FAIL -eq 0 ]]
}

# --- sous-commandes destructives, une par une -----------------------------

run_reboot() {
    echo "POST /v1alpha1/reboot -- le device va couper la connexion."
    auth_post /v1alpha1/reboot '' ; echo
    echo "Attente du retour (jusqu'à 30s)..."
    for _ in $(seq 1 10); do
        sleep 3
        if [[ "$(http_code -H "Authorization: Bearer $TOKEN" "$URL/v1alpha1/info")" == "200" ]]; then
            echo "OK, le device répond à nouveau."
            return 0
        fi
    done
    echo "FAIL : pas de réponse après 30s."
    return 1
}

run_rotate_token() {
    echo "POST /v1alpha1/token -- invalide $TOKEN pour le reste de cette session."
    local new_token; new_token=$(python3 -c "import secrets; print(secrets.token_hex(16))")
    local resp; resp=$(auth_post /v1alpha1/token "{\"token\":\"$new_token\"}")
    check "status == rotated" "$(jget "$resp" status)" "rotated"
    echo "Nouveau token (à conserver) : $new_token"
}

run_ota_activate() {
    cat <<'EOF'
ATTENTION : /ota/activate fait rebooter le device sur le slot déjà stagé
(voir `safe`'s staged.deployment_id via GET /info). Si ce slot ne contient
pas un vrai firmware bootable, le bootloader doit récupérer tout seul sur
l'autre slot -- déjà vérifié une fois à la main, mais chaque nouveau test
répète ce risque. Ctrl-C dans les 5s pour annuler.
EOF
    sleep 5
    local dep_id; dep_id=$(jget "$(auth_get /v1alpha1/info)" staged.deployment_id)
    echo "Activation de deployment_id=$dep_id..."
    auth_post /v1alpha1/ota/activate "{\"deployment_id\":\"$dep_id\",\"reboot\":true}"; echo
    run_reboot
}

run_push_cert() {
    local cert_file="${1:?Usage: $0 <url> <token> push-cert <cert.pem> <key.pem>}"
    local key_file="${2:?Usage: $0 <url> <token> push-cert <cert.pem> <key.pem>}"
    local payload; payload=$(python3 -c "
import json, sys
print(json.dumps({'cert_pem': open(sys.argv[1]).read(), 'key_pem': open(sys.argv[2]).read()}))
" "$cert_file" "$key_file")
    local resp; resp=$(auth_post /v1alpha1/tls/cert "$payload")
    check "status == saved" "$(jget "$resp" status)" "saved"
    echo "Certificat serveur remplacé ; l'API reste exclusivement sur HTTPS:443."
}

run_push_ca() {
    local ca_file="${1:?Usage: $0 <url> <token> push-ca <ca.pem>}"
    local payload; payload=$(python3 -c "
import json, sys
print(json.dumps({'ca_pem': open(sys.argv[1]).read()}))
" "$ca_file")
    local resp; resp=$(auth_post /v1alpha1/tls/ca "$payload")
    check "status == saved" "$(jget "$resp" status)" "saved"
    echo "CA enregistré : heartbeat/logs sortants (contrat §5) vérifieront désormais les certs contre ce CA."
}

run_push_firmware() {
    local file="${1:?Usage: $0 <url> <token> push-firmware <image.bin> [deployment-id]}"
    local dep="${2:-push-$(date +%s)}"
    local total; total=$(stat -c%s "$file")
    local digest; digest="sha256:$(sha256sum "$file" | cut -d" " -f1)"
    echo "deployment_id=$dep total=$total digest=$digest"

    local info; info=$(auth_get /v1alpha1/info)
    local chip; chip=$(jget "$info" chip)
    echo "== POST /v1alpha1/ota/prepare =="
    local prep; prep=$(auth_post /v1alpha1/ota/prepare \
        "{\"deployment_id\":\"$dep\",\"digest\":\"$digest\",\"size\":$total,\"chip\":\"$chip\",\"partition_layout\":\"embewi-ab-v1\"}")
    echo "  target_slot=$(jget "$prep" target_slot)"
    [[ "$(jget "$prep" accepted)" == "True" ]] || { echo "prepare refusé: $prep"; return 1; }

    # A single PUT with the whole ~1.2 MB body reproducibly ends in "Empty
    # reply from server" on this device's LAN path (pre-existing, unrelated
    # to OTA logic -- confirmed hardware-side, not this branch's fault).
    # What actually eliminates the per-chunk cost isn't one giant request,
    # it's not re-paying the TCP+TLS handshake for every chunk: still
    # Content-Range chunks (16 KiB, same wire format `push-firmware-resumable`
    # and the device's own resume logic use), all sent over one connection
    # `curl` keeps alive across every request in a single `-K` config
    # invocation (`--next` between blocks). Measured on real hardware: 75
    # separate connections ~270-295s, the same 75 chunks over one connection
    # ~29-34s -- an ~88% cut from the connection reuse alone.
    echo "== PUT /v1alpha1/ota/write (Content-Range chunké, connexion unique) =="
    local workdir; workdir=$(mktemp -d)
    local chunk=16384
    local conf="$workdir/requests.conf"
    local marker="===EMBEWI-CHUNK-END==="
    : > "$conf"
    local offset=0 n=0
    while [[ "$offset" -lt "$total" ]]; do
        local end=$((offset + chunk))
        [[ "$end" -gt "$total" ]] && end=$total
        local len=$((end - offset))
        local part; part=$(printf '%s/chunk_%05d.bin' "$workdir" "$n")
        dd if="$file" of="$part" bs=1M iflag=skip_bytes,count_bytes skip="$offset" count="$len" 2>/dev/null
        {
            # `next` separates requests within one `-K` invocation -- it
            # must never trail the last block (curl then tries to parse a
            # request after it and fails with "no URL specified!", exit 2,
            # which `set -e` turns into a silent abort of this whole
            # function even though every chunk already landed durably).
            [[ "$n" -gt 0 ]] && echo 'next'
            echo 'insecure'
            echo "url = \"$URL/v1alpha1/ota/write\""
            echo 'request = "PUT"'
            echo "header = \"Authorization: Bearer $TOKEN\""
            echo "header = \"X-Embewi-Deployment-Id: $dep\""
            echo "header = \"X-Embewi-Digest: $digest\""
            echo "header = \"Content-Range: bytes $offset-$((end - 1))/$total\""
            echo "data-binary = \"@$part\""
            echo "write-out = \"\\n$marker\\n\""
        } >> "$conf"
        offset=$end
        n=$((n + 1))
    done

    local out; out=$(curl -sk -K "$conf" 2>/dev/null)
    rm -rf "$workdir"

    # Every chunk's response is separated by the marker; the OTA outcome
    # (written/digest_mismatch/error) only ever lands in the last one.
    local resp; resp=$(printf '%s' "$out" | awk -v RS="$marker" 'NF{last=$0} END{print last}' | tr -d '\n')

    local status; status=$(jget "$resp" status)
    case "$status" in
        written)
            check "written == taille image" "$(jget "$resp" written)" "$total"
            check "digest final == attendu" "$(jget "$resp" digest)" "$digest"
            echo "OK: $total octets écrits en $n chunks sur une connexion, deployment_id=$dep"
            ;;
        digest_mismatch)
            echo "digest_mismatch -- le contenu envoyé ne correspond pas au digest annoncé."
            return 1
            ;;
        *)
            echo "réponse OTA inattendue (dernier chunk): $resp"
            echo "Pour diagnostiquer chunk par chunk avec resync : push-firmware-resumable"
            return 1
            ;;
    esac
}

run_push_firmware_resumable() {
    local file="${1:?Usage: $0 <url> <token> push-firmware-resumable <image.bin> [deployment-id]}"
    local dep="${2:-push-$(date +%s)}"
    local total; total=$(stat -c%s "$file")
    local digest; digest="sha256:$(sha256sum "$file" | cut -d' ' -f1)"
    echo "deployment_id=$dep total=$total digest=$digest"

    local info; info=$(auth_get /v1alpha1/info)
    local chip; chip=$(jget "$info" chip)
    echo "== POST /v1alpha1/ota/prepare =="
    local prep; prep=$(auth_post /v1alpha1/ota/prepare \
        "{\"deployment_id\":\"$dep\",\"digest\":\"$digest\",\"size\":$total,\"chip\":\"$chip\",\"partition_layout\":\"embewi-ab-v1\"}")
    echo "  target_slot=$(jget "$prep" target_slot)"
    [[ "$(jget "$prep" accepted)" == "True" ]] || { echo "prepare refusé: $prep"; return 1; }

    # Piloté par les réponses du device, jamais par un compteur local : le
    # `cursor` (= prochain Content-Range à envoyer) ne bouge QUE sur ce que
    # le device rapporte comme `written` (durable), que ce soit via un
    # `partial` normal ou via le `written` qu'un `416 range_mismatch`
    # renvoie après un accroc réseau -- exactement le protocole de reprise
    # que `fibewi::resume_plan`/`WriteSession` expose déjà côté firmware
    # (séparation `received` vs `durable`). Ne jamais supposer que ce que le
    # client vient d'envoyer est ce que le device a effectivement rendu
    # durable.
    local chunk=16384
    local cursor=0
    local stall=0
    local body; body=$(mktemp)
    while [[ "$cursor" -lt "$total" ]]; do
        local end=$((cursor + chunk))
        [[ "$end" -gt "$total" ]] && end=$total
        local len=$((end - cursor))
        local code
        code=$(curl -sk -m 25 -o "$body" -w "%{http_code}" -X PUT \
            -H "Authorization: Bearer $TOKEN" -H "X-Embewi-Deployment-Id: $dep" -H "X-Embewi-Digest: $digest" \
            -H "Content-Range: bytes $cursor-$((end - 1))/$total" \
            --data-binary @<(dd if="$file" bs=1 skip="$cursor" count="$len" 2>/dev/null) \
            "$URL/v1alpha1/ota/write" 2>/dev/null || echo "000")
        local resp; resp=$(cat "$body" 2>/dev/null)
        local status; status=$(jget "$resp" status)
        local reported; reported=$(jget "$resp" written)

        if [[ "$status" == "written" ]]; then
            check "digest final == attendu" "$(jget "$resp" digest)" "$digest"
            echo "OK: $total octets écrits, deployment_id=$dep"
            rm -f "$body"
            return 0
        elif [[ "$status" == "digest_mismatch" ]]; then
            echo "digest_mismatch -- le contenu envoyé ne correspond pas au digest annoncé, abandon."
            rm -f "$body"
            return 1
        elif [[ "$status" == "partial" && -n "$reported" ]]; then
            # Signal utile seulement si `written` diverge de ce qu'un
            # compteur local naïf aurait supposé (`end`) -- l'avancement
            # normal (`written == end`) n'a rien d'un resync et ne doit pas
            # être bruité comme tel.
            [[ "$reported" != "$end" ]] && echo "  (désync détectée : device à written=$reported, pas $end -- resync)"
            cursor=$reported
            stall=0
        elif [[ "$code" == "416" && -n "$reported" ]]; then
            echo "  416 range_mismatch -- resync sur written=$reported (cursor était $cursor)"
            cursor=$reported
            stall=0
        else
            stall=$((stall + 1))
            echo "  anomalie (code=$code status=$status resp=$resp) à cursor=$cursor, retry $stall/8"
            [[ "$stall" -ge 8 ]] && { echo "ABANDON après $stall échecs à cursor=$cursor"; rm -f "$body"; return 1; }
            sleep 2
        fi
    done
    rm -f "$body"
}

case "$MODE" in
    safe) run_safe ;;
    reboot) run_reboot ;;
    rotate-token) run_rotate_token ;;
    ota-activate) run_ota_activate ;;
    push-cert) run_push_cert "${4:-}" "${5:-}" ;;
    push-ca) run_push_ca "${4:-}" ;;
    push-firmware) run_push_firmware "${4:-}" "${5:-}" ;;
    push-firmware-resumable) run_push_firmware_resumable "${4:-}" "${5:-}" ;;
    *) echo "Mode inconnu: $MODE (safe|reboot|rotate-token|ota-activate|push-cert|push-ca|push-firmware|push-firmware-resumable)" >&2; exit 1 ;;
esac
