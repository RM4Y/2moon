# 2moon — streaming de jeu GameStream/Moonlight réécrit en Rust

`2moon` est un **hôte** (`moon-server`, équivalent de Sunshine) et un **client**
(`moon-client`, équivalent de Moonlight) écrits en Rust, compatibles sur le réseau
avec les implémentations officielles :

| Combinaison | Appairage | Vidéo | Audio | Contrôle / entrées |
|---|---|---|---|---|
| Moonlight 6.1 → `moon-server` | ✅ | ✅ H.264 / HEVC / AV1, 1080p60 | ✅ stéréo, 5.1, 7.1 (chiffré) | ✅ (chiffré) |
| `moon-client` → Sunshine | ✅ | ✅ H.264 1080p60 | ✅ (chiffré) | ✅ |
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
`--encoder auto|nvenc|software|<encodeur ffmpeg>`, `--output <écran>` (pour `kwin`),
`--nvenc-preset p1..p7` (qualité NVENC, `p4` par défaut), `--no-input` (n'injecte pas
les entrées), `--no-mdns`, `--list-clients`, `--name`.

Les paquets vidéo sont envoyés par un thread de *pacing* qui étale chaque image sur
60 % de l'intervalle entre images (utile à 120 fps / haut débit, surtout en Wi-Fi).

**Service systemd** (démarre avec la session Plasma, redémarre en cas d'échec) :

```sh
scripts/install-service.sh --port 57989 --nvenc-preset p7   # arguments passés à moon-server
systemctl --user status 2moon-server
journalctl --user -u 2moon-server -f      # stats vidéo toutes les 5 s (fps, débit, IDR)
```

**Appairage** : quand un client demande à s'appairer, tapez le PIN affiché par le
client dans la console de l'hôte, ou `curl 'http://127.0.0.1:47989/pin?pin=1234'`.

Fonctionnalités : HTTP/HTTPS GameStream avec certificats épinglés, RTSP (clair ou
chiffré `rtspenc`), flux de contrôle ENet chiffré AES-GCM, vidéo RTP avec FEC
Reed-Solomon, audio Opus multistream RTP avec FEC et chiffrement AES-CBC, H.264
(NVENC ou OpenH264), HEVC et AV1 (NVENC), annonce mDNS, injection uinput (clavier,
souris relative/absolue, molette haute résolution, manettes Xbox virtuelles, saisie
de texte Unicode selon la disposition clavier de l'hôte, ex. AZERTY).

## Client (`moon-client`)

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

Raccourcis dans la fenêtre : **Ctrl+Alt+Shift+Q** quitter, **+Z** capturer la souris
(mode relatif, pour les jeux), **+F** plein écran, **+S** statistiques dans le titre.

Décodage : NVDEC (GPU NVIDIA) pour H.264/HEVC/AV1, sinon OpenH264 (H.264) ou dav1d (AV1).

## Architecture

```
crates/
  moon-proto   protocole pur, sans E/S : crypto & appairage, XML, RTSP (+chiffré),
               paquets vidéo + FEC Reed-Solomon (Cauchy), audio RTP + FEC, contrôle & entrées
  moon-media   codecs : OpenH264, NVENC/FFmpeg, NVDEC, dav1d, Opus, PulseAudio, conversions YUV
  moon-server  hôte : HTTP(S), RTSP, contrôle ENet, pipelines vidéo/audio, capture
               (KWin, portail, PipeWire), uinput, mDNS
  moon-client  client : HTTP(S)/appairage, RTSP, contrôle, réception, fenêtre winit
```

Les détails du format filaire impossibles à connaître de mémoire ont été vérifiés
contre de vraies captures Sunshine/Moonlight : matrice FEC vidéo de Cauchy (coefficients
résolus sur trafic réel), IV GCM du contrôle et du RTSP chiffré (validés par le tag
d'authentification), schéma d'IV de l'audio chiffré, en-tête de trame (latence hôte).

## Tests

```sh
cargo test --release --workspace   # tests unitaires (protocole, FEC, crypto, codecs, NVENC/NVDEC si présents)
scripts/e2e.sh                     # 25 vérifications bout en bout client ↔ hôte en local
```

`MOON_VIDEO_CAPTURE=<fichier>` active les tests de rejeu d'une capture réelle
(produite par `moon-client probe`).

## Limites connues

- Capture : KWin, portail XDG ou PipeWire (Wayland). Pas de capture X11/KMS.
- HEVC/AV1 exigent un GPU NVIDIA côté hôte (NVENC) ; HEVC exige NVDEC côté client.
- Pas de HDR / 10 bits, ni de 4:4:4.
- Les caractères absents de la disposition clavier de l'hôte (touches mortes) ne sont pas saisis.
