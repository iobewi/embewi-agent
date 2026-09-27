# Processus de validation matérielle — ESP32-S3 (embewi-agent-esp)

Document de passation pour un autre agent : comment construire, flasher, tester
et valider ce firmware sur du matériel ESP32-S3 réel, depuis ce dépôt. Écrit
après la campagne de portage S3 du 2026-09-26 (voir la mémoire de session
`esp32s3_port_status.md` côté humain pour l'historique complet des bugs
trouvés).

## 1. Construire l'image factory

```sh
bash scripts/build-boot.sh
```

Ce script :
1. Construit le bootloader (aujourd'hui : `scripts/build-espbewi-bootloader.sh`,
   qui clone `espbewi` au SHA figé dans le script et compile
   `bootloader/esp` — **vérifier ce chemin avant de l'utiliser**, il a déjà
   bougé plusieurs fois entre dépôts pendant cette migration ; voir §6).
2. Compile `embewi-agent-esp` (le binaire agent réel) avec
   `cargo build --release --bin embewi-agent-esp`.
3. Compile `embewi-init` (l'image jetable de provisioning), en lui injectant
   la taille/digest/deployment-id de l'agent via
   `EMBEWI_FACTORY_AGENT_SIZE`/`_DIGEST`/`_DEPLOYMENT`.
4. Fusionne bootloader + table de partitions + `embewi-init` (slot `ota_0`)
   en une image factory unique : `web/firmware/esp32s3/firmware.bin`.
5. Produit séparément `web/firmware/esp32s3/agent.bin` (à flasher dans
   `ota_1`, offset `0x1a0000` — cet offset est dans `web/manifest.json` et
   doit rester cohérent avec `partitions.csv`).

Toolchain requis : le rustup toolchain `esp` (fork nightly Xtensa) —
`cargo +esp build -Z build-std=core,alloc --target xtensa-esp32s3-none-elf`.
Le compilateur croisé GCC réel vit sous
`/opt/rustup/toolchains/esp/xtensa-esp-elf/*/xtensa-esp-elf/bin/`. Ce dépôt a
son `.cargo/config.toml`/`rust-toolchain.toml` déjà configurés pour ça côté
`feat/esp32s3-target` (marqué TEMPORARY, à vérifier selon la branche).

Sortie de `build-boot.sh` à noter systématiquement :
```
Factory image : web/firmware/esp32s3/firmware.bin (N bytes)
Agent ota_1   : web/firmware/esp32s3/agent.bin (N bytes)
Agent digest  : sha256:...
Deployment    : factory-<16 premiers hex du digest>
```

## 2. Servir et flasher via ESP Web Tools

Le flash se fait **par navigateur**, pas par câble USB direct depuis cet
agent (pas d'accès au port série de l'humain). Cet agent sert seulement les
fichiers :

```sh
npx http-server web -p 8080 -c-1
```

Puis l'humain ouvre `http://localhost:8080/` (ou l'IP de la machine qui sert)
dans un navigateur Chrome/Edge (Web Serial API requise), choisit
`ESP32-S3` dans `web/manifest.json`, et flashe avec **Erase** pour un test
propre depuis zéro.

Après un build, **toujours vérifier que le serveur sert bien le nouveau
binaire** avant de dire à l'humain de flasher (le cache navigateur/serveur a
déjà causé des tests sur du vieux code par le passé) :

```sh
sha256sum web/firmware/esp32s3/firmware.bin
curl -s http://localhost:8080/firmware/esp32s3/firmware.bin | sha256sum
# les deux hash doivent être identiques
```

## 3. API du device : `scripts/test-api.sh`

Une fois le device flashé et provisionné (Wi-Fi + token obtenus par
l'humain via le formulaire de provisioning HTTPS), toute interaction se fait
via ce script — jamais de `curl` à la main pour les cas courants, il encode
déjà les pièges connus (Content-Range chunké, en-têtes requis, etc.).

```sh
scripts/test-api.sh <url> <token> [safe|reboot|rotate-token|ota-activate|push-cert|push-ca|push-firmware|push-firmware-resumable]
```

- **`safe`** (mode par défaut) : suite de ~45 tests non-destructifs
  (`/info`, `/health`, `/config`, auth, tout le protocole OTA prepare/write
  en edge cases). Aucun effet de bord dangereux — stage un faux binaire sur
  le slot inactif mais n'active jamais rien. À rejouer après **chaque**
  changement pour détecter une régression avant de risquer un reboot réel.
- **`push-firmware <image.bin> [deployment-id]`** : cycle réel
  prepare+write (Content-Range chunké 16 Ko, une seule connexion TLS
  réutilisée — un `PUT` monolithique échoue de façon reproductible sur ce
  matériel, "Empty reply from server"). Stage l'image sur le slot inactif,
  ne l'active pas.
- **`ota-activate`** : active le slot déjà stagé et **reboote le device
  immédiatement**. Le script affiche un avertissement et attend 5 s
  (Ctrl-C pour annuler) avant d'agir — ne jamais lancer ça sans prévenir
  l'humain au préalable, c'est une action à effet de bord réel.
- **`push-cert`/`push-ca`** : voir §4.
- **`reboot`** / **`rotate-token`** : effets de bord réels, mêmes précautions.

Pattern de vérification après un `ota-activate` : interroger `/v1alpha1/info`
en boucle (le device reboote, coupe la connexion, revient en 10-25 s) et
regarder `active_slot`, `boot.seq`, `boot.state`. `boot.state: valid`
confirme que l'auto-validation (`PendingVerify -> AwaitConfirmation ->
Confirmed`) s'est faite seule.

## 4. TLS / certificats — `tooling/mock-core/`

Le device ne parle en clair à rien : son "Core" de test est un serveur mock
tournant **sur la machine de l'humain** (pas dans le sandbox de cet agent —
pas de route réseau directe vers le LAN du device depuis ce conteneur). Le
rôle de cet agent est de générer les certificats et de les pousser via
l'API, pas de faire tourner le mock lui-même.

```sh
tooling/mock-core/generate-certs.sh <ip-ou-host-du-mock>
# produit ca.pem / ca-key.pem / server.pem / server.key à côté du script
```

**Piège déjà rencontré et documenté dans le script lui-même** : ne jamais
réutiliser un `server.pem` sans le `ca.pem` généré dans la même passe — un
leaf cert dont l'émetteur ne correspond à aucune CA connue produit
`MbedtlsError(-9984 / 0x2700)` (`X509_CERT_VERIFY_FAILED`) sur chaque
tentative de connexion, symptôme qui ne dit pas directement "mauvais
certificat".

Séquence complète pour remettre le mock en service :
1. Générer la paire ici (`generate-certs.sh <ip>`).
2. Donner `server.pem`/`server.key` à l'humain pour qu'il relance
   `mock_core.py --cert server.pem --key server.key --port 8443` **de son
   côté**.
3. Pousser `ca.pem` sur le device : `scripts/test-api.sh <url> <token>
   push-ca tooling/mock-core/ca.pem`.
4. Confirmer via les logs du mock (`conn #N heartbeat`, `conn #N: WS upgrade
   accepted`, `conn #N log: ...`) que heartbeat **et** log-stream se
   connectent — ce sont deux canaux TLS séparés, l'un ne prouve pas l'autre.

`ctrl_url` (adresse du Core que le device compose lui-même) est fixé une
fois pour toutes au provisioning (formulaire HTTPS ou Improv Serial) et ne
peut pas être changé par l'API ensuite — sauf full erase + reprovisioning.

## 5. Interpréter les logs série bruts

Les logs collés par l'humain (copie depuis un moniteur série externe, pas de
port série direct pour cet agent) sont **souvent corrompus autour d'un
reset matériel** : lignes concaténées sans saut de ligne, valeurs
numériques manquantes au milieu d'un message par ailleurs lisible, ordre des
lignes pas forcément chronologique. Ce n'est quasi jamais un vrai bug de
transmission UART — c'est un artefact de buffering au moment précis où le
CPU redémarre. Ne pas sur-interpréter un `MbedtlsError` ou une valeur
manquante avant d'avoir confirmé, via une nouvelle valeur numérique propre
retrouvée ailleurs dans le même log ou via l'API, que ce n'est pas juste un
fragment tronqué.

Repères utiles dans les logs du bootloader (`boot: ...`) et de l'agent
(`INFO -`/`WARN -`) :
- `rst:0x15 (USB_UART_CHIP_RESET)` → reset provoqué par l'outil de flash/monitor,
  normal en fin de session.
- `rst:0x10 (RTCWDT_RTC_RST)` → reset volontaire par le watchdog RTC, **le
  mécanisme de reboot normal** de ce firmware (`reboot_after_delay` dans
  `src/http/mod.rs`), pas un hang.
- `boot: slot N image refused, reason R` → `R` est un `image_error_code`
  (1=Read, 2=BadMagic, 3=BadChip, 4=BadSegmentCount, ... voir
  `image_error_code()` côté bootloader) — rollback automatique attendu si un
  autre slot est valide.

## 6. Frontière d'architecture à vérifier avant toute action

Ce projet a beaucoup bougé la frontière "qui possède le bootloader" pendant
cette migration (`fibewi` → `espbewi`, plusieurs branches parallèles dont
une abandonnée puis réutilisée par erreur une fois). **Avant de construire
ou de proposer un correctif sur le bootloader**, vérifier concrètement,
pas en confiance sur une note de mémoire :

```sh
cat scripts/build-boot.sh scripts/build-espbewi-bootloader.sh 2>/dev/null
# quel dépôt/rev est réellement pinné MAINTENANT ?
```

Puis confirmer que ce pin pointe vers du code réellement testé sur
matériel (chercher les commentaires du fichier `.x` du linker visé — un
commentaire qui documente un historique de bug est un bon signe qu'il a été
testé ; son absence ne prouve rien dans un sens ou dans l'autre, vérifier la
taille/position de la fenêtre DRAM face à `rom_spiflash_legacy_data` — voir
§7).

## 7. Bug matériel de référence : `rom_spiflash_legacy_data`

Si un nouveau portage/refactor du bootloader Xtensa (S3 ou autre chip avec
ROM SPI flash driver classique) plante tôt, sortie propre jusqu'au premier
appel flash ROM puis chaos illisible, sans `PanicInfo::location()`
exploitable (fichier vide, ligne 0) : **vérifier en premier** si la fenêtre
DRAM du bootloader (là où vit sa propre pile, qui démarre en haut de la
fenêtre et grandit vers le bas) chevauche l'adresse fixe que `esp-rom-sys`
fournit pour `rom_spiflash_legacy_data` sur ce chip (`grep -rn
"rom_spiflash_legacy_data" <chip>.rom.ld` dans les sources d'`esp-rom-sys`).
Le driver flash ROM lit/écrit cette structure à une adresse fixe
indépendante de l'application ; si la pile du bootloader vit dessus, le
premier appel flash corrompt la pile en cours d'exécution. Agrandir la
fenêtre sans changer sa position n'aide pas — il faut que la fenêtre entière
reste sous cette adresse (ou strictement au-dessus), avec une marge de
sécurité (l'ESP32-C3 en garde ~16 Ko).
