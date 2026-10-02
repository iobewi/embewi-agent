#!/usr/bin/env bash
# Campagne de test de l'OTA Workload (S17) contre un device réel, en HTTPS + Bearer.
# L'artefact est OPAQUE : seul son contenu (taille, SHA-256) compte, jamais sa sémantique.
#
# Usage :
#   scripts/test-workload-ota.sh <url> <token> status
#   scripts/test-workload-ota.sh <url> <token> safe              # non destructif : auth, erreurs, aucune écriture flash
#   scripts/test-workload-ota.sh <url> <token> push <file> [id] [version] [req_api=1.0] [chunk_kib=16]
#   scripts/test-workload-ota.sh <url> <token> push-bad-digest <file>   # le contenu envoyé != le digest annoncé -> 422, jamais staged
#   scripts/test-workload-ota.sh <url> <token> resume <file>     # coupe l'upload à mi-parcours, teste les 416, reprend et finit
#   scripts/test-workload-ota.sh <url> <token> compat <file>     # stage un Workload exigeant une API future -> activate refusé (409)
#   scripts/test-workload-ota.sh <url> <token> activate <file>   # POST /activate (S17 : 501 supervisor_unavailable attendu)
#
# `push*`, `resume`, `compat` ÉCRIVENT dans le slot Workload inactif (plusieurs MiB selon <file>) :
# ils ne font donc volontairement pas partie de `safe`.
set -euo pipefail

URL="${1:?Usage: $0 <url> <token> <mode> ...}"
TOKEN="${2:?Usage: $0 <url> <token> <mode> ...}"
MODE="${3:?Usage: $0 <url> <token> <mode> ...}"
URL="${URL%/}"
BASE="$URL/v1alpha1/workload/ota"
[[ "$URL" == https://* ]] || { echo "REFUS: HTTPS uniquement" >&2; exit 2; }

PASS=0; FAIL=0
jget() { python3 -c '
import json,sys
try:
    d=json.loads(sys.argv[1])
    for k in sys.argv[2].split("."): d=d[k]
    print(d if not isinstance(d,(dict,list)) else json.dumps(d))
except Exception: print("")' "$1" "$2"; }
check() { if [[ "$2" == "$3" ]]; then echo "  OK   $1"; PASS=$((PASS+1)); else echo "  FAIL $1 (attendu=$3 obtenu=$2)"; FAIL=$((FAIL+1)); fi; }
code() { curl -sk -o /dev/null -w "%{http_code}" -m 15 "$@"; }
wl_get() { curl -sk -m 15 -H "Authorization: Bearer $TOKEN" "$BASE$1"; }
wl_post() { curl -sk -m 15 -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$2" "$BASE$1" -w '\n%{http_code}'; }
digest_of() { echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"; }
body_of() { sed '$d' <<<"$1"; }
status_of() { tail -n1 <<<"$1"; }

prepare_body() { # file id version major minor
    printf '{"artifact_id":"%s","version":"%s","size":%s,"digest":"%s","required_runtime_api":{"major":%s,"minor":%s}}' \
        "$2" "$3" "$(stat -c%s "$1")" "$(digest_of "$1")" "$4" "$5"
}

# Envoie les chunks [first,last) de <data> (déclarés comme parties de <total>/<digest>) sur UNE connexion.
send_chunks() { # data digest total chunk first last
    local data="$1" digest="$2" total="$3" chunk="$4" first="$5" last="$6"
    local workdir; workdir=$(mktemp -d); local conf="$workdir/r.conf"; : > "$conf"
    local n="$first"
    while [[ "$n" -lt "$last" ]]; do
        local offset=$((n * chunk)); local end=$((offset + chunk)); [[ "$end" -gt "$total" ]] && end=$total
        [[ "$offset" -ge "$total" ]] && break
        local part; part=$(printf '%s/c%05d' "$workdir" "$n")
        dd if="$data" of="$part" bs=1M iflag=skip_bytes,count_bytes skip="$offset" count=$((end - offset)) 2>/dev/null
        {
            [[ "$n" -gt "$first" ]] && echo 'next'
            echo 'insecure'; echo "url = \"$BASE/write\""; echo 'request = "PUT"'
            echo "header = \"Authorization: Bearer $TOKEN\""
            echo "header = \"X-Embewi-Digest: $digest\""
            echo "header = \"Content-Range: bytes $offset-$((end - 1))/$total\""
            echo "data-binary = \"@$part\""
            echo 'write-out = "\n===END %{http_code}===\n"'
        } >> "$conf"
        n=$((n + 1))
    done
    curl -sk -m 600 -K "$conf" 2>/dev/null || true
    rm -rf "$workdir"
}

run_status() {
    echo "== GET /workload/ota/status =="
    local s; s=$(wl_get /status); echo "$s" | python3 -m json.tool 2>/dev/null || echo "$s"
}

run_safe() {
    echo "== auth (sans/mauvais Bearer -> 401) =="
    for m in "GET status" "POST prepare" "POST activate" "PUT write"; do
        set -- $m
        check "$1 /$2 sans token" "$(code -X "$1" -d '{}' "$BASE/$2")" "401"
        check "$1 /$2 mauvais token" "$(code -X "$1" -H 'Authorization: Bearer nope' -d '{}' "$BASE/$2")" "401"
    done
    echo "== status =="
    local s; s=$(wl_get /status)
    check "status 200 JSON" "$([[ -n "$(jget "$s" supported)" ]] && echo yes)" "yes"
    local supported; supported=$(jget "$s" supported)
    echo "  supported=$supported state=$(jget "$s" state) max_artifact_size=$(jget "$s" max_artifact_size) reason=$(jget "$s" reason)"
    echo "== requêtes invalides (aucune écriture flash) =="
    local r
    r=$(wl_post /prepare 'not json');          check "prepare non-JSON -> 400/409" "$([[ "$(status_of "$r")" =~ ^(400|409)$ ]] && echo ok)" "ok"
    r=$(wl_post /prepare '{"artifact_id":"x"}'); check "prepare incomplet -> 400/409" "$([[ "$(status_of "$r")" =~ ^(400|409)$ ]] && echo ok)" "ok"
    r=$(wl_post /activate '{}');               check "activate sans digest -> 400" "$(status_of "$r")" "400"
    if [[ "$supported" == "True" ]]; then
        local max; max=$(jget "$s" max_artifact_size)
        r=$(wl_post /prepare "{\"artifact_id\":\"big\",\"version\":\"1\",\"size\":$((max + 1)),\"digest\":\"sha256:$(printf '0%.0s' {1..64})\",\"required_runtime_api\":{\"major\":1,\"minor\":0}}")
        check "prepare > slot -> 413 (rien effacé)" "$(status_of "$r")" "413"
    else
        r=$(wl_post /prepare "{\"artifact_id\":\"x\",\"version\":\"1\",\"size\":10,\"digest\":\"sha256:$(printf '0%.0s' {1..64})\",\"required_runtime_api\":{\"major\":1,\"minor\":0}}")
        check "prepare sur appareil non supporté -> 409" "$(status_of "$r")" "409"
        check "reason == MissingMeta" "$(jget "$(body_of "$r")" reason)" "MissingMeta"
    fi
    echo "== /v1alpha1/ota/* (Agent) inchangé : sans token -> 401 =="
    check "POST /ota/prepare sans token" "$(code -X POST -d '{}' "$URL/v1alpha1/ota/prepare")" "401"
}

run_push() {
    local file="${1:?file}" id="${2:-pod}" version="${3:-1.0.0}" api="${4:-1.0}" chunk_kib="${5:-16}"
    local major="${api%%.*}" minor="${api##*.}" chunk=$((chunk_kib * 1024))
    local total; total=$(stat -c%s "$file"); local digest; digest=$(digest_of "$file")
    echo "id=$id version=$version size=$total digest=$digest requires=$api"
    local t0; t0=$(date +%s.%N)
    local r; r=$(wl_post /prepare "$(prepare_body "$file" "$id" "$version" "$major" "$minor")")
    echo "== prepare -> $(status_of "$r") $(body_of "$r")"
    [[ "$(status_of "$r")" == "200" ]] || return 1
    local chunks=$(( (total + chunk - 1) / chunk ))
    local out; out=$(send_chunks "$file" "$digest" "$total" "$chunk" 0 "$chunks")
    local t1; t1=$(date +%s.%N)
    echo "$out" | grep -c '===END 200===' | xargs echo "chunks acceptés (200):"
    local final; final=$(echo "$out" | grep -B1 '===END' | tail -n 2 | head -n 1)
    echo "dernière réponse: $final"
    python3 - "$t0" "$t1" "$total" <<'PY'
import sys
t0,t1,total=float(sys.argv[1]),float(sys.argv[2]),int(sys.argv[3])
dt=t1-t0
print(f"transfert HTTPS: {total} B en {dt:.1f} s = {total/1024/dt:.0f} KiB/s")
PY
    check "dernier chunk -> staged" "$(jget "$final" status)" "staged"
    check "digest rapporté == digest du fichier" "$(jget "$final" digest)" "$digest"
    run_status
}

run_push_bad_digest() {
    local file="${1:?file}"
    local good; good=$(digest_of "$file"); local total; total=$(stat -c%s "$file")
    local evil; evil=$(mktemp); cp "$file" "$evil"; printf '\xAA' | dd of="$evil" bs=1 seek=$((total - 1)) conv=notrunc 2>/dev/null
    local before; before=$(jget "$(wl_get /status)" state)
    local r; r=$(wl_post /prepare "$(prepare_body "$file" pod bad 1 0)")
    [[ "$(status_of "$r")" == "200" ]] || { echo "prepare refusé: $r"; return 1; }
    local out; out=$(send_chunks "$evil" "$good" "$total" 16384 0 $(( (total + 16383) / 16384 )))
    local last; last=$(echo "$out" | grep -B1 '===END' | tail -n 2 | head -n 1)
    local lastcode; lastcode=$(echo "$out" | grep -o '===END [0-9]*===' | tail -n1 | tr -dc '0-9')
    echo "dernière réponse: $lastcode $last"
    check "digest faux -> 422" "$lastcode" "422"
    check "error == digest_mismatch" "$(jget "$last" error)" "digest_mismatch"
    local after; after=$(wl_get /status)
    # Un nouvel upload remplace le candidat précédent dès son premier octet (même slot) :
    # après un échec il n'y a donc plus aucun candidat Staged -- jamais un Staged sur des octets écrasés.
    check "jamais staged (le nouveau digest faux n'est pas candidat)" "$([[ "$(jget "$after" state)" != "staged" ]] && echo yes)" "yes"
    check "aucun candidat déclaré" "$(jget "$after" candidate)" "None"
    rm -f "$evil"
}

run_resume() {
    local file="${1:?file}"; local total; total=$(stat -c%s "$file"); local digest; digest=$(digest_of "$file")
    local chunk=16384; local chunks=$(( (total + chunk - 1) / chunk )); local half=$((chunks / 2))
    [[ "$half" -ge 2 ]] || { echo "fichier trop petit pour tester la reprise (>= 64 KiB)"; return 1; }
    local r; r=$(wl_post /prepare "$(prepare_body "$file" pod resume 1 0)"); [[ "$(status_of "$r")" == "200" ]] || { echo "$r"; return 1; }
    echo "== première moitié ($half chunks), puis 'coupure' =="
    send_chunks "$file" "$digest" "$total" "$chunk" 0 "$half" | grep -o '===END [0-9]*===' | sort | uniq -c
    echo "== trou (saute un chunk) -> 416 + point de reprise =="
    local gap; gap=$(send_chunks "$file" "$digest" "$total" "$chunk" $((half + 1)) $((half + 2)))
    check "trou -> 416" "$(echo "$gap" | grep -o '===END [0-9]*===' | tr -dc '0-9')" "416"
    echo "$gap" | head -n1
    echo "== recouvrement -> 416 =="
    local over; over=$(send_chunks "$file" "$digest" "$total" "$chunk" $((half - 1)) "$half")
    check "recouvrement -> 416" "$(echo "$over" | grep -o '===END [0-9]*===' | tr -dc '0-9')" "416"
    echo "== reprise au point rapporté jusqu'au bout =="
    local rest; rest=$(send_chunks "$file" "$digest" "$total" "$chunk" "$half" "$chunks")
    local final; final=$(echo "$rest" | grep -B1 '===END' | tail -n 2 | head -n 1)
    check "reprise -> staged" "$(jget "$final" status)" "staged"
    check "digest final correct" "$(jget "$final" digest)" "$digest"
}

run_compat() {
    local file="${1:?file}"
    run_push "$file" pod future-api 9.9 16
    local r; r=$(wl_post /activate "{\"digest\":\"$(digest_of "$file")\"}")
    echo "activate -> $(status_of "$r") $(body_of "$r")"
    check "activation incompatible refusée (409)" "$(status_of "$r")" "409"
    check "error == incompatible_runtime_api" "$(jget "$(body_of "$r")" error)" "incompatible_runtime_api"
    local s; s=$(wl_get /status)
    check "le candidat reste Staged" "$(jget "$s" state)" "staged"
    check "candidat conservé" "$(jget "$s" candidate.version)" "future-api"
}

run_activate() {
    local file="${1:?file}"
    local r; r=$(wl_post /activate "{\"digest\":\"$(digest_of "$file")\"}")
    echo "activate -> $(status_of "$r") $(body_of "$r")"
    check "pas de superviseur -> 501" "$(status_of "$r")" "501"
    check "error == supervisor_unavailable" "$(jget "$(body_of "$r")" error)" "supervisor_unavailable"
    check "l'état reste staged" "$(jget "$(wl_get /status)" state)" "staged"
}

case "$MODE" in
    status) run_status ;;
    safe) run_safe ;;
    push) shift 3; run_push "$@" ;;
    push-bad-digest) run_push_bad_digest "${4:-}" ;;
    resume) run_resume "${4:-}" ;;
    compat) run_compat "${4:-}" ;;
    activate) run_activate "${4:-}" ;;
    *) echo "Mode inconnu: $MODE" >&2; exit 1 ;;
esac
echo; echo "-- $PASS OK / $FAIL FAIL --"
[[ "$FAIL" -eq 0 ]]
