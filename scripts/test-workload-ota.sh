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
#   (S18, build `workload-supervisor-probe`)
#   scripts/test-workload-ota.sh <url> <token> mkprobe <out> [fault=none|fail-start|freeze|health-fail|reset-on-start|reset-on-stop] [period_ms=500] [size=65536] [salt]
#   scripts/test-workload-ota.sh <url> <token> runtime           # etat OTM2 + execution (running/health)
#   scripts/test-workload-ota.sh <url> <token> sup-activate <file>  # activate -> pending_confirmation, runtime reellement lance
#   scripts/test-workload-ota.sh <url> <token> sup-confirm       # confirm -> valid (refuse si non Healthy)
#   scripts/test-workload-ota.sh <url> <token> sup-rollback      # rollback manuel -> precedent Valid (ou Empty)
#   scripts/test-workload-ota.sh <url> <token> sup-reject <file> <reason>   # (S19) image refusee a l'activation: 422 image_rejected, Staged conserve
#   scripts/test-workload-ota.sh <url> <token> wait-running <file>          # (S19) attend que ce digest tourne
#   scripts/test-workload-ota.sh <url> <token> sup-start-fault <file>  # artefact fail-start: activate -> 500 rolled_back, ancien relance
#   scripts/test-workload-ota.sh <url> <token> sup-health-fault <file> # artefact health-fail: activate OK, confirm -> 409 workload_unhealthy
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
    for m in "GET status" "POST prepare" "POST activate" "POST confirm" "POST rollback" "PUT write"; do
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

run_mkprobe() {
    local out="${1:?out}" fault="${2:-none}" period="${3:-500}" size="${4:-65536}" salt="${5:-$RANDOM}"
    python3 - "$out" "$fault" "$period" "$size" "$salt" <<'PY'
import sys, struct, hashlib
out, fault, period, size, salt = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
codes = {"none":0,"fail-start":1,"freeze":2,"health-fail":3,"reset-on-start":4,"reset-on-stop":5}
head = b"S18PROBE" + bytes([1, codes[fault]]) + struct.pack("<H", period) + b"\0\0\0\0"
body = b""; n = 0
while len(head) + len(body) < size:
    body += hashlib.sha256(f"{salt}:{n}".encode()).digest(); n += 1
open(out, "wb").write((head + body)[:size])
PY
    echo "$out: fault=$fault period=${period}ms size=$(stat -c%s "$out") $(digest_of "$out")"
}

run_runtime() {
    local s; s=$(wl_get /status)
    echo "state=$(jget "$s" state) active=$(jget "$s" active.version) candidate=$(jget "$s" candidate.version) previous=$(jget "$s" previous.version)"
    echo "runtime: supervised=$(jget "$s" runtime.supervised) running=$(jget "$s" runtime.running) health=$(jget "$s" runtime.health) artifact=$(jget "$s" runtime.artifact.digest)"
}

wait_health() { # expected, tries
    local want="$1" n="${2:-20}" h
    while [[ "$n" -gt 0 ]]; do
        h=$(jget "$(wl_get /status)" runtime.health); [[ "$h" == "$want" ]] && { echo "$h"; return 0; }
        sleep 1; n=$((n - 1))
    done
    echo "$h"; return 1
}

run_sup_activate() {
    local file="${1:?file}"
    local r; r=$(wl_post /activate "{\"digest\":\"$(digest_of "$file")\"}")
    echo "activate -> $(status_of "$r") $(body_of "$r")"
    check "activate -> 200" "$(status_of "$r")" "200"
    check "status == pending_confirmation" "$(jget "$(body_of "$r")" status)" "pending_confirmation"
    local s; s=$(wl_get /status)
    check "OTM2 state == pending_confirmation" "$(jget "$s" state)" "pending_confirmation"
    check "le candidat tourne vraiment" "$(jget "$s" runtime.running)" "True"
    check "runtime.artifact == digest" "$(jget "$s" runtime.artifact.digest)" "$(digest_of "$file")"
    check "health healthy" "$(wait_health healthy 15)" "healthy"
}

run_sup_confirm() {
    local r; r=$(wl_post /confirm '{}')
    echo "confirm -> $(status_of "$r") $(body_of "$r")"
    check "confirm -> 200 valid" "$(status_of "$r")$(jget "$(body_of "$r")" status)" "200valid"
    check "OTM2 state == valid" "$(jget "$(wl_get /status)" state)" "valid"
}

run_sup_rollback() {
    local before; before=$(wl_get /status)
    local r; r=$(wl_post /rollback '{}')
    echo "rollback -> $(status_of "$r") $(body_of "$r")"
    check "rollback -> 200 rolled_back" "$(status_of "$r")$(jget "$(body_of "$r")" status)" "200rolled_back"
    local s; s=$(wl_get /status)
    local want; want=$(jget "$before" previous.digest)
    if [[ -n "$want" && "$want" != "None" ]]; then
        check "state == valid (precedent)" "$(jget "$s" state)" "valid"
        check "le precedent tourne vraiment" "$(jget "$s" runtime.artifact.digest)" "$want"
    else
        check "state == empty" "$(jget "$s" state)" "empty"
        check "plus rien ne tourne" "$(jget "$s" runtime.running)" "False"
    fi
}

run_sup_start_fault() {
    local file="${1:?file}" before; before=$(wl_get /status)
    local old; old=$(jget "$before" runtime.artifact.digest)
    local r; r=$(wl_post /activate "{\"digest\":\"$(digest_of "$file")\"}")
    echo "activate -> $(status_of "$r") $(body_of "$r")"
    check "echec de demarrage -> 500" "$(status_of "$r")" "500"
    check "error == activation_failed" "$(jget "$(body_of "$r")" error)" "activation_failed"
    check "rolled_back == True" "$(jget "$(body_of "$r")" rolled_back)" "True"
    local s; s=$(wl_get /status)
    check "etat revenu a valid" "$(jget "$s" state)" "valid"
    check "l'ancien Workload tourne a nouveau" "$(jget "$s" runtime.artifact.digest)" "$old"
}

run_sup_health_fault() {
    local file="${1:?file}"
    run_sup_activate_nohealth "$file"
    local r; r=$(wl_post /confirm '{}')
    echo "confirm -> $(status_of "$r") $(body_of "$r")"
    check "confirm refuse -> 409" "$(status_of "$r")" "409"
    check "error == workload_unhealthy" "$(jget "$(body_of "$r")" error)" "workload_unhealthy"
    check "toujours pending_confirmation" "$(jget "$(wl_get /status)" state)" "pending_confirmation"
}

run_sup_activate_nohealth() {
    local file="${1:?file}"
    local r; r=$(wl_post /activate "{\"digest\":\"$(digest_of "$file")\"}")
    echo "activate -> $(status_of "$r") $(body_of "$r")"
    check "activate -> 200" "$(status_of "$r")" "200"
    check "runtime.health == unhealthy" "$(wait_health unhealthy 20)" "unhealthy"
}

# S19 (build `workload-native`): a native image the gate refuses -> 422 image_rejected, the
# candidate stays Staged, the running Workload is untouched, and nothing was executed.
run_sup_reject() {
    local file="${1:?file}" reason="${2:?reason}" api="${3:-1.0}"
    local before; before=$(wl_get /status)
    local r; r=$(wl_post /activate "{\"digest\":\"$(digest_of "$file")\"}")
    echo "activate -> $(status_of "$r") $(body_of "$r")"
    check "activation refusee -> 422" "$(status_of "$r")" "422"
    check "error == image_rejected" "$(jget "$(body_of "$r")" error)" "image_rejected"
    check "reason == $reason" "$(jget "$(body_of "$r")" reason)" "$reason"
    local s; s=$(wl_get /status)
    check "le candidat reste Staged" "$(jget "$s" state)" "staged"
    check "le Workload actif est intact" "$(jget "$s" runtime.artifact.digest)" "$(jget "$before" runtime.artifact.digest)"
}

# Wait until the running Workload reports the expected identity digest.
run_wait_running() {
    local file="${1:?file}" n=15 want; want=$(digest_of "$file")
    while [[ "$n" -gt 0 ]]; do
        [[ "$(jget "$(wl_get /status)" runtime.artifact.digest)" == "$want" ]] && { echo "running $want"; return 0; }
        sleep 1; n=$((n - 1))
    done
    echo "pas en cours d'execution: $want"; return 1
}

case "$MODE" in
    sup-reject) run_sup_reject "${4:-}" "${5:-}" ;;
    wait-running) run_wait_running "${4:-}" ;;
    mkprobe) shift 3; run_mkprobe "$@" ;;
    runtime) run_runtime ;;
    sup-activate) run_sup_activate "${4:-}" ;;
    sup-confirm) run_sup_confirm ;;
    sup-rollback) run_sup_rollback ;;
    sup-start-fault) run_sup_start_fault "${4:-}" ;;
    sup-health-fault) run_sup_health_fault "${4:-}" ;;
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
