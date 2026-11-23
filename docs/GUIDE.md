# 2moon — guide détaillé

Présentation, téléchargements et démarrage rapide : voir le [README](../README.md).

`2moon` est un **hôte** (`moon-server`) et un **client**
(`moon-client`) écrits en Rust, compatibles sur le réseau avec le protocole
GameStream :

| Combinaison | Appairage | Vidéo | Audio | Contrôle / entrées |
|---|---|---|---|---|
| Client GameStream → `moon-server` | ✅ | ✅ H.264 / HEVC / AV1, 1080p60 | ✅ stéréo, 5.1, 7.1 (chiffré) | ✅ (chiffré) |
| `moon-client` → `moon-server` | ✅ | ✅ H.264 / HEVC / AV1 | ✅ stéréo, 5.1, 7.1 | ✅ |

Aucun en-tête de développement n'est nécessaire : FFmpeg (NVENC), CUDA/NVDEC,
PipeWire, PulseAudio, Opus et xkbcommon sont chargés dynamiquement à l'exécution.

## Compilation

```sh
cargo build --release
# binaires : target/release/moon-server et target/release/moon-client
```

## Hôte (`moon-server`)

```sh
# KDE Plasma, y compris headless : capture d'écran KWin sans boîte de dialogue
moon-server --install-kwin-permission      # une seule fois (crée ~/.local/share/applications/2moon-server.desktop)
moon-server --video kwin --audio desktop

# Autres sources vidéo
moon-server --video portal                 # portail XDG ScreenCast (dialogue la 1re fois, jeton mémorisé)
moon-server --video pipewire --pw-node 42  # nœud PipeWire existant (id ou nom)
moon-server --video test --audio sine      # mire et tonalité de test
```

Options utiles : `--port` (47989 par défaut ; les autres ports en découlent),
`--encoder auto|nvenc|vaapi|amf|qsv|software|<encodeur ffmpeg>`, `--output <écran>` (pour `kwin`),
`--no-input` (n'injecte pas
les entrées), `--no-mdns`, `--no-mic`, `--no-remote`, `--list-clients`, `--name`.

**Micro du client** (extension 2moon, absente de GameStream) : si le client l'active
(réglage « Micro envoyé à l'hôte » ou `moon-client stream --mic`), il envoie le micro
choisi (celui par défaut, ou un micro précis choisi dans les Réglages) en
Opus mono 48 kHz, chiffré avec la clé de session, sur le port UDP base + 12 (48001).
L'hôte crée alors un micro virtuel « 2moon Microphone » (PipeWire/PulseAudio, via
`pactl`), le met comme entrée par défaut pendant le stream, puis restaure l'ancienne
entrée et le supprime à la fin. Ctrl+Alt+Maj+M (Ctrl+Option+Cmd+M sur macOS) coupe/réactive le micro. Les autres clients
GameStream ignorent l'extension. `--no-mic` la désactive sur l'hôte.

**Encodeur** : `auto` choisit NVIDIA (NVENC) s'il est là, sinon **AMD ou Intel via
VAAPI** sous Linux (`h264_vaapi`, `hevc_vaapi`, `av1_vaapi` du FFmpeg fourni ; les images
sont copiées dans des surfaces GPU avant l'encodage ; `MOON_VAAPI_DEVICE=/dev/dri/renderD129`
choisit un autre GPU), sinon OpenH264 (H.264 logiciel). NVENC est piloté directement (en-tête `nvEncodeAPI.h` de nv-codec-headers,
licence MIT, dans `crates/moon-media/csrc/`), avec repli sur NVENC via FFmpeg puis sur
OpenH264. L'accès direct permet l'**invalidation d'images de référence (RFI)** : quand un
client signale des images perdues, l'hôte envoie une simple image P qui ne s'appuie plus
dessus, au lieu d'une image clé complète (H.264 et HEVC ; en AV1, NVENC répond par une
image clé). Les clients l'utilisent automatiquement.

Chaque image capturée est encodée dès son arrivée, sur une grille régulière à la
fréquence du stream. Les paquets vidéo partent à 300 Mbit/s (ou 3× le débit du stream) :
une image de 40 Ko part en ~1 ms. `MOON_PACING_MBPS=<n>` change ce débit si le Wi-Fi
perd des paquets.

**Service systemd** (démarre avec la session Plasma, redémarre en cas d'échec) :

```sh
scripts/install-service.sh --port 57989 --encoder nvenc   # arguments passés à moon-server
systemctl --user status 2moon-server
journalctl --user -u 2moon-server -f      # stats vidéo toutes les 5 s (fps, débit, IDR)
```

**Appairage** : quand un client demande à s'appairer, tapez le PIN affiché par le
client dans la console de l'hôte, ou `curl 'http://127.0.0.1:47989/pin?pin=1234'`.

**Panneau d'administration web** : `http://127.0.0.1:47990/` sur l'hôte, ou
`http://<hôte>:47990/` depuis le réseau local (port de base + 1,
`--admin-port`, `--no-admin` pour le désactiver). À la première ouverture, créez le
compte administrateur avec le jeton affiché dans le journal du serveur. On y suit la
session en direct (images/s, débit, temps d'encodage), on saisit le PIN d'appairage
dans quatre cases, on révoque les appareils appairés et on lit le journal. Même thème
que le client. Jamais joignable par l'accès à distance.

Fonctionnalités : HTTP/HTTPS GameStream avec certificats épinglés, RTSP (clair ou
chiffré `rtspenc`), flux de contrôle ENet chiffré AES-GCM, vidéo RTP avec FEC
Reed-Solomon, audio Opus multistream RTP avec FEC et chiffrement AES-CBC, H.264
(NVENC ou OpenH264), HEVC et AV1 (NVENC), annonce mDNS, injection uinput (clavier,
souris relative/absolue, molette haute résolution, manettes Xbox virtuelles, saisie
de texte Unicode selon la disposition clavier de l'hôte, ex. AZERTY).

## Client (`moon-client`)

**Installation sous Linux** : installez le paquet de votre distribution (client et hôte,
dans `/opt/2moon`, entrée « 2moon » dans le menu, commandes `moon-client` et `moon-server`) :
`sudo apt install ./2moon_<version>-1_amd64.deb` (Debian, Ubuntu…),
`sudo dnf install ./2moon-<version>-1.x86_64.rpm` (Fedora, openSUSE avec `zypper`…) ou
`sudo pacman -U 2moon-<version>-1-x86_64.pkg.tar.zst` (Arch, Manjaro, omarchy…).

Sans droits root, l'archive portable `2moon-<version>-linux-x86_64.tar.gz` : extrayez-la
puis lancez `./install.sh`. 2moon est installé dans `~/.local/share/2moon/client`,
avec l'entrée « 2moon » dans le menu des applications et un raccourci sur le Bureau, tous
deux avec le logo (`--no-desktop` pour ne pas créer le raccourci), plus la commande
`moon-client` (et `moon-server`) dans `~/.local/bin`. Pour une mise à jour, relancez `./install.sh` depuis la
nouvelle archive. Pour désinstaller : `~/.local/share/2moon/client/install.sh --uninstall`
(les réglages et les hôtes appairés restent dans `~/.config/2moon-client` ; le service de
l'hôte installé par l'application est retiré). L'archive `2moon-server-…` contient l'hôte seul,
pour les machines sans écran.

**Interface graphique** : lancez simplement `moon-client` (sans argument). Thème « nuit
lunaire » (ciel étoilé, lune, un accent citron), le même que le panneau d'administration
de l'hôte.

- Accueil : hôtes trouvés sur le réseau (mDNS) et hôtes enregistrés, avec leur état
  (en ligne, en jeu, hors ligne) ; ajout manuel par code.
- Appairage en un clic : le PIN s'affiche en grand, à saisir sur l'hôte (panneau
  d'administration 2moon).
- Applications de l'hôte : Lancer, Reprendre, Quitter l'application.
- Réglages mémorisés : résolution, fps, débit (avec valeur conseillée), codec, 4:4:4,
  HDR, audio 2/6/8 canaux, micro envoyé à l'hôte (désactivé, micro par défaut ou un
  micro précis), décodage GPU, plein écran, statistiques, affichage des images
  (fluide, latence minimale ou sans déchirement).
- Le stream s'affiche dans la même fenêtre (texture GPU) ; Ctrl+Alt+Shift+Q (Ctrl+Option+Cmd+Q sur macOS) revient à l'accueil.

En ligne de commande :

```sh
moon-client discover                       # trouve les hôtes du réseau (mDNS)
moon-client pair 192.168.1.10              # affiche un PIN à saisir sur l'hôte
moon-client list 192.168.1.10
moon-client stream 192.168.1.10 --width 1920 --height 1080 --fps 60 --bitrate 20000
moon-client quit 192.168.1.10
```

Options de `stream` : `--codec auto|h264|hevc|av1`, `--software-decode`,
`--audio-channels 2|6|8`, `--app <id>`, `--quit-on-exit`, `--headless --seconds N`
(sans fenêtre, affiche des statistiques).

Raccourcis dans la fenêtre : **Ctrl+Alt+Shift+Q** revenir à l'accueil, **+Z** libérer / recapturer
la souris (capturée par défaut : mode relatif, pour les jeux), **+F** plein écran, **+S** statistiques,
**+M** couper le micro. Les lettres sont celles inscrites sur les touches (AZERTY
compris) ; les touches envoyées à l'hôte gardent leur position physique.
Sur macOS, les raccourcis sont **Ctrl+Option+Cmd+lettre** (Ctrl+Option+Cmd+Q, +Z, +F, +S, +M) ;
la touche Cmd seule est envoyée à l'hôte comme touche Windows.

Décodage : NVDEC (GPU NVIDIA) pour H.264/HEVC/AV1, sinon OpenH264 (H.264) ou dav1d (AV1).

## Jouer depuis l'extérieur

**Accès à distance (client 2moon et hôte 2moon, activé par défaut)** : rien à ouvrir
sur le routeur. Le client et l'hôte se joignent par une connexion QUIC
([iroh](https://www.iroh.computer)) qui traverse le NAT et le CGNAT (connexion directe
par « hole punching »), avec un serveur relais en dernier recours (un peu plus de
latence). Tout le trafic GameStream (HTTP(S), RTSP, vidéo, audio, contrôle, micro)
passe dans cette connexion.

- L'hôte a un **code** (64 caractères, stable), affiché dans le panneau web et dans
  le mode Hôte (Réglages › Accès à distance).
- Un appareil appairé sur le réseau local reçoit ce code tout seul : hors de chez
  vous, il bascule sur l'accès à distance quand l'adresse locale ne répond pas.
- Depuis l'extérieur, saisissez le code dans « Ajouter un hôte » (ou
  `moon-client pair <code>`) : l'appairage demande toujours le PIN sur l'hôte.
- Les paquets vidéo sont réduits pour tenir dans un datagramme QUIC.
- Les serveurs publics d'iroh (n0) servent à la mise en relation et au relais ; ils
  limitent le débit relayé : le relais dépanne, la connexion directe est la règle.
- `--no-remote` sur l'hôte (ou l'interrupteur) le désactive. Les autres clients
  GameStream n'utilisent pas ce tunnel : il faut alors rediriger soi-même les ports
  (47984, 47989, 48010 en TCP ; 47998 à 48001 en UDP) sur le routeur.

## 2moon unifié (client et hôte)

L'application `moon-client` contient aussi l'hôte : le sélecteur **Client / Hôte** en haut
à droite bascule entre les deux (les deux lunes échangent leur place). Le mode choisi est
mémorisé.

Le mode **Hôte** pilote l'hôte (`moon-server`) de cet ordinateur, qui tourne en service et
continue de fonctionner application fermée. On y retrouve les pages du panneau web :
Session (stream en cours, code PIN d'appairage), Appareils, Applications, Hôte, Journal et
Réglages, avec les mêmes réglages (clavier, nom, qualité d'encodage NVENC, arrêt
automatique, résolution native, accès à distance, mot de passe). Si l'hôte n'est pas installé ou arrêté,
l'application propose de l'installer ou de le démarrer (service utilisateur systemd sous
Linux — capture KWin sous KDE Plasma, portail de capture d'écran ailleurs —, LaunchAgent
`com.2moon.server` sur macOS ; sous Windows, l'installateur « 2moon Host »).

Paquets (`scripts/package-release.sh <version>`, aussi depuis un Mac avec Docker, par exemple Colima),
une seule application 2moon par système :
- Windows : l'installateur `2moon-<version>-windows-x86_64-setup.exe` (hôte en service +
  application 2moon, raccourcis menu Démarrer et Bureau) ;
- Linux : `.deb`, `.rpm` et `.pkg.tar.zst` (Arch), plus l'archive portable `.tar.gz` ;
- macOS (lancé sur un Mac) : `2moon-<version>-macos-arm64.dmg`, l'application `2moon.app`
  à glisser dans Applications. Signée ad hoc, non notarisée : la première fois, ouvrez-la
  avec clic droit › Ouvrir (ou Réglages Système › Confidentialité et sécurité › Ouvrir quand même).

Le client n'affiche pas l'hôte de son propre ordinateur dans la liste des ordinateurs
(le mode Hôte s'en occupe).

Accès local : au démarrage, l'hôte écrit `local-admin.json` (jeton aléatoire et port,
lisible par son seul utilisateur) dans son dossier d'état ; l'application l'envoie dans
l'en-tête `X-2moon-Token`, accepté uniquement depuis 127.0.0.1. Quand ce fichier n'est pas
lisible (hôte Windows en service SYSTEM), l'application demande l'identifiant et le mot de
passe du panneau web. `MOON_SERVER_STATE_DIR` désigne un autre dossier d'état (hôte lancé
avec `--state-dir`). Les vignettes des applications se changent depuis le panneau web.

## Hôte macOS

```sh
scripts/build-ffmpeg-macos.sh             # FFmpeg (VideoToolbox) + Opus pour macOS
cp -a target/ffmpeg-bundle/macos-$(uname -m)/lib target/release/
moon-server --video screen --audio desktop
```

- Capture de l'écran et du son par ScreenCaptureKit (macOS 13+) ; `--output N` choisit
  l'écran N (vide : l'écran principal).
- Encodage H.264 et HEVC par VideoToolbox (pas d'AV1 : les Mac ne l'encodent pas).
- Clavier et souris par événements CoreGraphics.
- Autorisations à donner à `moon-server` dans Réglages Système › Confidentialité et
  sécurité : **Enregistrement de l'écran** (image et son) et **Accessibilité** (clavier,
  souris). Un binaire non signé perd ces autorisations à chaque recompilation : signez-le
  avec un certificat local stable (`codesign -f -s "2moon local" target/release/moon-server`)
  pour qu'elles restent valables d'une version à l'autre.
- ScreenCaptureKit livre les images à la taille du stream (mise à l'échelle sur le GPU) et
  seulement quand l'écran change.
- Pas encore de micro du client ni de manettes virtuelles sur macOS.
- Dossier d'état : `~/Library/Application Support/2moon-server`.

## Architecture

```
crates/
  moon-proto   protocole pur, sans E/S : crypto & appairage, XML, RTSP (+chiffré),
               paquets vidéo + FEC Reed-Solomon (Cauchy), audio RTP + FEC, contrôle & entrées
  moon-media   codecs : OpenH264, NVENC/FFmpeg, NVDEC, dav1d, Opus, PulseAudio, conversions YUV
  moon-server  hôte : HTTP(S), RTSP, contrôle ENet, pipelines vidéo/audio, capture
               (KWin, portail, PipeWire), uinput, mDNS
  moon-client  client : HTTP(S)/appairage, RTSP, contrôle, réception, fenêtre winit
  moon-tunnel  accès à distance : GameStream dans une connexion iroh (QUIC, NAT, relais)
```

Les détails du format filaire impossibles à connaître de mémoire ont été vérifiés
contre de vraies captures réseau : matrice FEC vidéo de Cauchy (coefficients
résolus sur trafic réel), IV GCM du contrôle et du RTSP chiffré (validés par le tag
d'authentification), schéma d'IV de l'audio chiffré, en-tête de trame (latence hôte).

## Couleurs 4:4:4

Option « Couleurs 4:4:4 » du client (`moon-client stream --yuv444`) : la chrominance
n'est plus sous-échantillonnée, le texte et les petits détails colorés restent nets.
Négociée par les bits 4:4:4 de `ServerCodecModeSupport`,
`x-ss-video[0].chromaSamplingType`). Hôte : NVENC direct,
H.264 et HEVC. Client : NVDEC pour le HEVC (GPU Turing ou plus récent), sinon le processeur.
Plus de débit pour la même qualité ; repli automatique en 4:2:0 si l'un des deux côtés
ne sait pas faire.

## HDR et 10 bits

Option « Accepter la HDR / 10 bits » du client (`moon-client stream --hdr`), en HEVC
ou AV1. Dans GameStream le 10 bits n'existe qu'en HDR : l'hôte place alors le bureau
SDR dans un signal HDR10 (primaires BT.2020, courbe PQ, blanc SDR à 203 nits selon
BT.2408), encodé en 10 bits par NVENC (moins d'effets d'escalier dans les dégradés), et
envoie les métadonnées HDR au client. Le client
décode le 10 bits (NVDEC, VAAPI ou processeur) et, sa fenêtre étant SDR, reconvertit
la HDR en SDR dans son shader (hautes lumières compressées au-delà de 75 % du blanc).
Pas de HDR « réelle » tant
que l'écran capturé par l'hôte n'est pas lui-même en HDR.

## Affichage sans copie (copie zéro)

Dans l'interface graphique, les images décodées par le GPU ne repassent pas par la
mémoire centrale :
- **NVDEC** (NVIDIA) : copie GPU → GPU dans des textures OpenGL enregistrées auprès de
  CUDA (partage CUDA/OpenGL) ;
- **VAAPI** (Intel/AMD) : la surface décodée est exportée en DMA-BUF et importée comme
  texture via EGL (FFmpeg fourni uniquement).

Au moindre échec (pas d'EGL, GPU d'affichage différent du GPU de décodage...), le client
revient automatiquement à la copie. `MOON_ZERO_COPY=0` désactive la copie zéro.

## Hôte Windows

Installateur unique `2moon-<version>-windows-x86_64-setup.exe` (client et hôte) : installe l'hôte dans
`C:\Program Files\2moon Host`, le **service Windows « 2moon »**, la règle du pare-feu,
le pilote **ViGEmBus** (manettes, facultatif) et les raccourcis du menu Démarrer
(panneau d'administration, journaux), ainsi que l'application 2moon (client et mode Hôte,
raccourci « 2moon » sur le Bureau). Le service tourne en SYSTEM et lance
l'hôte dans la session ouverte : il capture aussi les fenêtres UAC et l'écran de
connexion, et suit les changements de session.

- Capture : **DXGI Desktop Duplication** (l'écran principal ; `--output DISPLAY2` pour un
  autre), pointeur de la souris dessiné par l'hôte.
- Encodage : **NVIDIA NVENC**, **AMD AMF** ou **Intel Quick Sync**, choisi automatiquement
  (FFmpeg fourni, compilé avec nv-codec-headers, les en-têtes AMF et libvpl), sinon OpenH264.
- Son : bouclage **WASAPI** de la sortie par défaut. Entrées : **SendInput** (codes de
  touches matériels, texte Unicode) et manettes Xbox 360 virtuelles via **ViGEmBus**
  (vibrations comprises).
- Applications lancées avec le compte de l'utilisateur connecté, arrêtées avec tout ce
  qu'elles ont lancé (objet job).
- Données : `C:\ProgramData\2moon` (SYSTEM et administrateurs uniquement) ; journaux dans
  `logs\` ; options supplémentaires de l'hôte dans `host-args.txt` (une par ligne).
- Le premier compte du panneau d'administration se crée sans jeton depuis le PC hôte
  lui-même (`http://127.0.0.1:47990/`).
- `moon-server.exe --install-service` / `--uninstall-service` (ce que fait l'installateur).
- Pare-feu : l'hôte est autorisé sur les réseaux **privés et de domaine** ; le composant
  « Autoriser sur les réseaux publics » l'étend aux réseaux publics (coché d'office lors
  d'une mise à jour depuis une version qui les autorisait déjà).
- Installation silencieuse : `2moon-…-setup.exe /S [/D=C:\chemin]` ;
  désinstallation : `uninstall.exe /S` (garde `C:\ProgramData\2moon`).

Compilé depuis Linux : `scripts/build-windows-deps.sh`, `scripts/build-windows.sh`, puis
`scripts/build-windows-installer.sh <version>` (NSIS, dans un conteneur Debian 12).

## Client Windows

Le client fait partie de l'installateur unique `2moon-<version>-windows-x86_64-setup.exe`
(voir « Hôte Windows ») : l'application 2moon est installée dans `C:\Program Files\2moon Host`,
avec le raccourci « 2moon » dans le menu Démarrer et sur le Bureau ; Mesa (OpenGL logiciel)
n'est ajouté que sur les machines sans pilote OpenGL 2.0. `moon-client.exe` s'utilise aussi en
ligne de commande (`pair`, `stream`...).

- Décodage : NVDEC (NVIDIA), sinon **D3D11VA** (tous GPU, DXVA2 sur les systèmes plus
  anciens), sinon le processeur. Copie zéro NVDEC → OpenGL comme sous Linux.
- Son et micro : WASAPI (via cpal), avec adaptation automatique au format de la carte son.
- Réglages et identité : `%APPDATA%\2moon-client`.

Compilé depuis Linux : `scripts/build-windows-deps.sh` (DLL FFmpeg / Opus / SDL3, MinGW-w64)
puis `scripts/build-windows.sh` ; `scripts/build-windows-installer.sh <version>` produit
l'installateur, que `scripts/package-release.sh` produit avec les autres paquets.

## FFmpeg fourni

Les archives Linux du client et de l'hôte contiennent leur propre FFmpeg (`lib/`), réduit
au nécessaire : décodage H.264, HEVC et AV1 (décodeurs FFmpeg et dav1d) avec l'accélération
VAAPI, et les encodeurs VAAPI de l'hôte (AMD / Intel). Pas besoin d'un FFmpeg complet sur
la machine. Il est compilé par `scripts/build-ffmpeg.sh` à partir des archives officielles
(sommes SHA-256 figées), sous Debian 12, et intégré par `scripts/package-release.sh`.
`MOON_FFMPEG=system` force l'usage du FFmpeg du système. Sous Windows, les DLL FFmpeg
contiennent en plus les encodeurs NVENC, AMF et Quick Sync (`scripts/build-windows-deps.sh`).

## Manettes

Le client lit les manettes avec **SDL3** : pilotes HIDAPI (PlayStation 4/5, Switch
Pro, Xbox, manettes Steam) et base de correspondances SDL, vibrations comprises. Jusqu'à
quatre manettes ; l'hôte les voit comme des manettes Xbox 360. Les manettes virtuelles
de Steam Input sont ignorées, sauf si Steam a lancé le client
(`SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD=1`).

SDL3 est fourni avec le client (`lib/libSDL3.so.0` sous Linux, `SDL3.dll` sous Windows,
`lib/libSDL3.0.dylib` dans `2moon.app` sur macOS), compilé par `scripts/build-sdl.sh`
(`scripts/build-sdl-macos.sh` sur Mac ; contrôleurs uniquement), sinon celui du système est
utilisé ; `MOON_SDL=<chemin>` en impose un autre. `cargo run --example pads 10` liste les
manettes vues par SDL et affiche les boutons pressés pendant 10 s.

## Tests

```sh
cargo test --release --workspace   # tests unitaires (protocole, FEC, crypto, codecs, NVENC/NVDEC si présents)
scripts/e2e.sh                     # 25 vérifications bout en bout client ↔ hôte en local
```

`MOON_VIDEO_CAPTURE=<fichier>` active les tests de rejeu d'une capture réelle
(produite par `moon-client probe`).

## Limites connues

- Capture : KWin, portail XDG ou PipeWire (Wayland). Pas de capture X11/KMS.
- Hôte macOS : pas de micro du client ni de manettes virtuelles.
- Hôte Windows : capture et conversion des couleurs par le processeur (pas encore de
  copie zéro), pas de micro du client, pas de 4:4:4 ni de HDR, pas de RFI.
- Hôte AMD / Intel (VAAPI, AMF, Quick Sync) : pas de RFI, de 4:4:4 ni de HDR (NVENC
  direct uniquement). HEVC/AV1 selon le GPU. Côté client : NVDEC (NVIDIA), VAAPI
  (Intel/AMD) ou le processeur.
- Le micro n'existe qu'entre un client et un hôte 2moon.
- Pas de 4:4:4 en AV1 (NVENC ne le propose pas) ; pas de HDR combinée au 4:4:4.
- Le client affiche toujours en SDR (conversion HDR → SDR) : egui/OpenGL ne présente pas de surface HDR.
- Les caractères absents de la disposition clavier de l'hôte (touches mortes) ne sont pas saisis.
