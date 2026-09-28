# Embewi Agent

Agent embarqué Rust du protocole [Embewi](https://github.com/iobewi/embewi)
(`v1alpha1`). La spécification Core ↔ Agent est dans
[`contract/docs/embewi-contract-v2.md`](contract/docs/embewi-contract-v2.md),
rattachée à ce dépôt sous forme de submodule.

**Objectif :** compiler le même agent avec un adaptateur de plateforme choisi
à la construction : [`iobewi-esp`](https://github.com/iobewi/iobewi-esp) pour ESP,
puis `rpbewi` pour RP2350 et `teensybewi` pour Teensy. Ces deux derniers
adaptateurs sont prévus, pas encore implémentés.

**État actuel :** malgré le renommage du dépôt, le code et les deux binaires
Cargo restent liés à l'ESP. La cible matérielle du firmware compilé ici est
l'ESP32-S3-N16R8. ESP32-C3 reste couvert par les briques partagées comme
cible de non-régression ; le cœur indépendant du matériel n'a pas encore été
extrait. Le paquet et le binaire portent encore le nom historique
`embewi-agent-esp` pendant cette migration.

## Architecture cible

| Composant | Responsabilité |
| --- | --- |
| `embewi-agent` | Services applicatifs, identité, configuration, authentification et assemblage des capacités requises, sans dépendance à une puce. |
| [IOBEWI HTTP](https://github.com/iobewi/iobewi) | Contrat et dispatch HTTP/TLS portables, implémentés par chaque plateforme. |
| [IOBEWI OTA](https://github.com/iobewi/iobewi/tree/main/services/ota) | Gestion OTA, transactions, reprise, validation et métadonnées durables indépendantes du matériel. |
| [IOBEWI Wi-Fi et TLS](https://github.com/iobewi/iobewi/tree/main/services) | Identifiants Wi-Fi, reconnexion, reprovisionnement, identité TLS et confiance durables. |
| [IOBEWI ConfigSpace](https://github.com/iobewi/iobewi/tree/main/services/config-space) | Espaces de configuration, quotas et générations indépendants du stockage physique. |
| `iobewi-esp` / futurs adaptateurs RP2350, Teensy | Implémentations matérielles des services demandés par l'agent, IOBEWI OTA et ConfigSpace. |
| Firmware de plateforme | Initialisation des périphériques, choix de l'adaptateur et assemblage du binaire pour la cible. |

IOBEWI OTA et ConfigSpace définissent leurs interfaces et ne dépendent pas d'un
adaptateur ESP. `iobewi-esp` fournit leurs implémentations sur ESP : flash,
partitions, boot, NVS, watchdog, Wi-Fi et TLS. Le choix de la plateforme est
statique à la compilation ; chaque cible conserve sa toolchain, son linker,
son plan de flash et son bootloader propres.

L'authentification et les routes applicatives de l'agent se branchent sur le
serveur `iobewi-http`. Le même service porte les échanges HTTP sortants
du heartbeat et le protocole WebSocket des logs. `iobewi-https` impose le handshake TLS.
`iobewi-esp-https` fournit le listener ESP, `iobewi-esp-tls` porte MbedTLS.
Les endpoints de provisionnement sont fournis par `iobewi-tls` sous les
chemins relatifs `/tls/cert` et `/tls/ca`. L’agent choisit le préfixe
`/v1alpha1`, fournit l’autorisation Bearer et monte ces routes sur son
routeur HTTPS. Les URL publiques restent `/v1alpha1/tls/cert` et
`/v1alpha1/tls/ca`.
`iobewi-ota` fournit aussi son routeur `/prepare`, `/write`, `/activate` ;
l’agent le monte sous `/v1alpha1/ota` et fournit le redémarrage différé.
`iobewi-log-stream` collecte les logs et les transmet par WebSocket ;
`iobewi-esp-log-stream` fournit TLS, l’aléa matériel et la sortie console.
L’agent fournit uniquement son URL de contrôle, son token et ses métadonnées.
`iobewi-ntp` synchronise l’horloge Unix par SNTP ; l’agent configure le serveur,
les délais et le seuil de validité de l’époque.
Une plateforme supplémentaire doit pouvoir fournir ses capacités sans
modifier la logique métier de l'agent.

## Migration en cours

- **IOBEWI OTA :** moteur transactionnel, métadonnées, session d'écriture,
  reprise, préflight, réponses et téléversement HTTP en flux, réconciliation
  au boot et validation bornée dans le temps.
- **IOBEWI ESP :** flash, ConfigSpace/NVS, slots, EWBT, validation de l'image,
  watchdog, radio Wi-Fi, cryptographie TLS et redémarrage matériel.
- **Agent :** identité et autorisation des requêtes, état applicatif,
  vérifications préalables à la confirmation, injection de pannes de test,
  montage des routes et composition du firmware ESP.

Un adaptateur de test indépendant de l'ESP servira à vérifier le même service
OTA avant un portage RP2350 ou Teensy. La compilation seule ne prouve pas
encore ce portage ni la validation sur carte après cette migration.

La branche [firmware-c](https://github.com/iobewi/embewi-agent/tree/firmware-c)
conserve l'ancienne implémentation ESP-IDF/C comme référence fonctionnelle.

## Compiler la cible ESP actuelle

Cloner aussi le contrat :

```sh
git clone --recursive https://github.com/iobewi/embewi-agent.git
cd embewi-agent
# Pour un clone déjà présent : git submodule update --init
```

Le devcontainer fournit la toolchain Xtensa `esp`. Le contrôle de compilation
utilisé par la CI pour l'ESP32-S3 est :

```sh
cargo +esp check --locked -Z build-std=core,alloc --target xtensa-esp32s3-none-elf
```

Pour construire l'image flashable avec le bootloader iobewi-esp :

```sh
scripts/build-boot.sh
```

Les images produites dans `web/firmware/<chip>/` ne sont pas commitées. Le
chemin ROM → bootloader → `embewi-init` → agent, le cycle OTA positif, le rejet
d'une image invalide, le rollback et le watchdog matériel ont été validés
sur ESP32-S3 réel. Chaque changement du chemin OTA ou du boot nécessite une
nouvelle vérification matérielle ; la compilation CI ne remplace pas cet essai.

## Installation ESP depuis le navigateur

Le devcontainer sert `web/` sur le port `8080` sous le label « ESP Web Tools ».
Cette page utilise Web Serial pour flasher les images générées, sans toolchain
sur le poste client. Voir [`web/README.md`](web/README.md) pour la génération
des images, le provisionnement et la récupération.
