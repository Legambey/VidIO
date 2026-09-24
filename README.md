# VidIO

Moniteur vidéo et audio à faible latence pour cartes d'acquisition USB — pensé
pour jouer sur une console rétro branchée en HDMI ou composite sur un PC, sans
subir le retard d'un logiciel de capture généraliste.

OBS et consorts sont faits pour enregistrer et diffuser : ils accumulent des
trames d'avance, et ce tampon se paie en manette. VidIO ne fait qu'une chose —
afficher ce qui arrive, le plus vite possible — et jette une image plutôt que de
prendre du retard.

Fonctionnel de bout en bout sur **Linux** (V4L2) et **Windows** (Media
Foundation) : capture, décodage, rendu GPU, filtres CRT, panneau de réglages et
transit audio.

---

## Installation

Les binaires sont fournis dans la
[dernière release](https://github.com/Legambey/VidIO/releases/latest) — rien à
compiler.

### Linux

Télécharger `vidio`, puis :

```bash
chmod +x vidio
sudo install -m755 vidio /usr/local/bin/
```

Trois bibliothèques doivent être présentes, ce qui est le cas sur toute machine
de bureau : `alsa-lib`, `libxkbcommon` et le chargeur Vulkan
(`vulkan-icd-loader` sur Arch, `libvulkan1` sur Debian et Ubuntu), plus un
pilote Vulkan pour la carte graphique — `mesa` couvre Intel et AMD.

`v4l-utils` n'est **pas** nécessaire : VidIO parle directement aux ioctls du
noyau, sans passer par `libv4l2`. Le paquet reste pratique pour diagnostiquer
avec `v4l2-ctl`, rien de plus.

Il faut en revanche appartenir au groupe `video` pour ouvrir `/dev/videoN` :

```bash
sudo usermod -aG video $USER      # puis se reconnecter
```

### Windows

Télécharger `vidio.exe` et le lancer. Rien à installer : Media Foundation,
WASAPI et Direct3D 12 font partie du système.

Le binaire n'ouvre pas de fenêtre de terminal à côté de la sienne. Les
sous-commandes de diagnostic restent utilisables : lancées depuis un terminal,
elles y écrivent normalement.

Si la carte est détectée mais refuse de s'ouvrir, c'est presque toujours le
réglage de confidentialité :
voir [Quand ça ne marche pas](#quand-ça-ne-marche-pas).

### Compiler soi-même

`cargo build --release` suffit sous Linux. Sous Windows, il faut en plus la
chaîne d'outils MSVC pour l'éditeur de liens. Le dépôt fournit aussi un
`PKGBUILD` pour Arch.

---

## Premiers pas

Branchez la carte, puis :

```bash
vidio list          # qu'est-ce que le système voit ?
vidio run           # ouvrir la fenêtre
```

Sans argument, `vidio run` rouvre le dernier périphérique utilisé — ou le
premier disponible si celui-là a été débranché. Une fois la fenêtre ouverte,
`Tab` fait apparaître le panneau de réglages : source vidéo, format, entrée et
sortie audio, couleurs, filtre CRT. Tout se change à chaud.

Si l'image n'apparaît pas, `vidio formats 0` dit ce que la carte annonce
vraiment, et la section [Quand ça ne marche pas](#quand-ça-ne-marche-pas) couvre
les causes courantes.

---

## Les commandes

| Commande | À quoi elle sert |
|---|---|
| `vidio run [périph]` | ouvre la fenêtre — commande par défaut |
| `vidio list` | périphériques vidéo et audio détectés |
| `vidio formats <périph>` | formats, résolutions et cadences annoncés |
| `vidio controls <périph>` | contrôles matériels du capteur, avec leurs plages |
| `vidio set <périph> <ctrl> <val>` | écrit un contrôle et le mémorise dans le profil |
| `vidio bench <périph>` | mesure la cadence réelle, les pertes, le coût du décodage |
| `vidio audio` | fait transiter l'entrée audio vers la sortie, sans vidéo |
| `vidio config` | chemin et contenu de la configuration |

Quelques options utiles :

```bash
vidio run 0 --width 1920 --height 1080 --fps 60
vidio run 0 --fourcc YUYV          # forcer un format de pixel
vidio run 0 --no-audio             # vidéo seule
vidio bench 0 --secs 10 --fourcc MJPG
vidio audio --secs 30 --gain 0.5 --latency-ms 15
vidio set 0 brightness -8          # les valeurs négatives passent telles quelles
```

Un format demandé avec `--fourcc` est honoré ou refusé : si la carte ne l'annonce
pas, VidIO s'arrête en listant ce qu'elle offre réellement, plutôt que d'en
servir un autre en silence. Un format simplement *mémorisé* dans un profil, lui,
cède la place si l'appareil ne le propose plus.

### Désigner un périphérique

Au choix : son index (`0`), son nœud (`/dev/video0` sous Linux, le lien
symbolique sous Windows), sa clé stable, ou un fragment de son nom. C'est le
fragment de nom qui se retient le mieux — `vidio run Video` suffit si la carte
s'annonce « USB3.0 Video ».

Un périphérique nommé en ligne de commande doit exister : se rabattre en silence
sur un autre serait pire que l'erreur. C'est seulement quand rien n'est demandé
que VidIO se débrouille tout seul.

---

## Raccourcis clavier

| Touche | Effet |
|---|---|
| `Tab` ou `F1` | panneau de réglages |
| `F` | plein écran · `Échap` en sort, puis quitte |
| `[` `]` | luminosité · `;` `'` contraste |
| `,` `.` | gamma · `-` `=` saturation |
| `C` | filtre CRT · `S` préréglage suivant |
| `I` | agrandissement entier ou proportionnel |
| `↑` `↓` | volume · `M` muet |
| `R` | remise à zéro de l'image · `Q` quitter |

Chaque changement s'affiche deux secondes au bas de l'écran. Les préréglages CRT
défilent dans l'ordre : aucun, léger, moniteur, arcade.

---

## Profils et configuration

Un seul fichier, dont `vidio config` donne le chemin :

- Linux : `~/.config/vidio/config.toml`
- Windows : `%APPDATA%\vidio\config.toml`

Il contient les réglages généraux — plein écran, agrandissement entier, mode de
présentation, dernier périphérique — puis **un profil par périphérique** :
format vidéo, contrôles matériels, couleurs, filtre CRT, choix et volume audio.

Changer de carte recharge le sien. Les profils sont indexés sur une clé stable
dérivée du numéro de série, pas sur `/dev/video0` : ils survivent donc à un
changement de port USB, et deux exemplaires du même modèle gardent des réglages
distincts.

Les réglages sont enregistrés en quittant. Chaque section du panneau se remet à
ses valeurs d'origine d'un bouton — les contrôles du capteur reprennent les
défauts annoncés par le pilote, pas des valeurs choisies par VidIO. Le bouton
« tout réinitialiser » du bas remet le profil entier, et demande confirmation.

---

## Obtenir la latence la plus basse

**Préférez un format non compressé.** À résolution et cadence égales, YUYV part
sur le GPU sans décodage ; le MJPEG coûte un tiers de cœur en 720p30, et
dépasserait le cœur entier en 1080p60. VidIO choisit déjà le non compressé
quand il a le choix — `vidio bench` mesure la différence sur votre machine.

**Surveillez les trames perdues.** Le panneau affiche capturées, écrasées et
perdues par le pilote. Des pertes côté pilote sur USB veulent dire bande
passante insuffisante : baissez la résolution, ou changez de port — idéalement
un port racine USB 3, pas un concentrateur partagé.

**Laissez le mode de présentation sur `Immediate`.** C'est la latence minimale,
au prix d'un peu de tearing. VidIO retombe automatiquement sur ce qui est
disponible si le pilote ne l'offre pas.

**L'audio se règle séparément.** `--latency-ms` fixe la cible du tampon de
transit, 20 ms par défaut. Descendre plus bas s'entend en craquements dès que la
machine est chargée.

---

## Quand ça ne marche pas

**La carte apparaît dans `vidio list` mais refuse de s'ouvrir — Windows.**
Paramètres › Confidentialité et sécurité › Caméra : l'accès doit être autorisé,
et surtout « Autoriser les applications de bureau à accéder à votre caméra » doit
être activé. VidIO le dit explicitement au lieu de relayer le code d'erreur brut.

**Permission refusée sur `/dev/videoN` — Linux.** Il manque le groupe `video`
(voir l'installation), et il faut se reconnecter pour que ça prenne effet.

**Aucun son, ou « périphérique occupé » — Linux.** Le `default` d'ALSA ne mène
pas forcément quelque part : sans le fichier qui le redirige vers le serveur de
son (`/etc/alsa/conf.d/99-pipewire-default.conf`, fourni par `pipewire-alsa`),
il pointe sur la carte que le serveur tient déjà ouverte. VidIO prend alors le
premier PCM qui s'ouvre — le serveur d'abord, le matériel ensuite. Le panneau
permet de choisir explicitement.

**Un format listé est refusé à l'ouverture.** La liste est filtrée par la taille
maximale de texture du GPU ; une taille qu'il ne peut pas recevoir n'est pas
proposée. Si elle arrive quand même, par un profil enregistré ou la ligne de
commande, elle est refusée avec un message.

**Voir ce qui se passe.** Le journal passe par `RUST_LOG` :

```bash
RUST_LOG=debug vidio run          # Linux
$env:RUST_LOG="debug"; vidio run  # Windows, PowerShell
```

---

## Comment ça marche

```
carte USB ──► V4L2 / Media Foundation ──► [thread de capture]
                                               │  décodage MJPEG si besoin
                                               ▼
                                     dernière trame publiée
                                               │  réveille la boucle
                                               ▼
              écran ◄── wgpu ◄── shader YUV + CRT ◄── [thread de rendu]
```

Deux threads, une seule trame entre eux. Le thread de capture n'attend jamais le
rendu : si celui-ci décroche, la trame en attente est écrasée par la plus
récente. Accumuler un retard qu'on ne rattrapera jamais est pire que sauter une
image. Les tampons circulent entre les deux via un pool, pour ne pas allouer
soixante fois par seconde.

### Organisation du code

```
src/
├── main.rs                 sous-commandes de diagnostic
├── config.rs               TOML persistant, un profil par périphérique
├── device.rs               identité stable des périphériques
├── camera/
│   ├── mod.rs              trait Camera, formats, plomberie de trames
│   ├── v4l2.rs             backend Linux (ioctls V4L2 directs)
│   └── mediafoundation.rs  backend Windows (lecteur de source MF)
├── audio/
│   ├── mod.rs              asservissement de dérive, interpolateur, mixage
│   ├── alsa_catalog.rs     inventaire des PCM réels, lu dans /proc/asound
│   └── cpal_backend.rs     flux cpal et tampon circulaire temps réel
├── video/
│   ├── decode.rs           décodage MJPEG vers RGBA
│   ├── renderer.rs         wgpu : upload, agrandissement, présentation
│   └── shader.wgsl         YUV, colorimétrie, scanlines, masque, courbure
├── app.rs                  fenêtre, cadencement, clavier
└── ui.rs                   panneau egui et affichage à l'écran
```

Les deux backends de capture sont derrière le même trait et exposent les mêmes
contrôles matériels. Tout le reste — négociation de format, rendu, shaders,
profils, audio, interface — est partagé. Aucune dépendance Windows n'est
compilée sous Linux, et réciproquement : les deux crates spécifiques (`v4l`,
`windows`) sont déclarées par cible.

### Décisions structurantes

**Aucun convertisseur, des deux côtés.** Même parti pris, obtenu autrement selon
le système. Sous Linux, la crate `v4l` est utilisée sans `libv4lconvert`. Sous
Windows, le lecteur de source reçoit `MF_READWRITE_DISABLE_CONVERTERS` : sans
cette ligne, Media Foundation « rend service » en insérant un décodeur et un
convertisseur de couleurs, et livre du RGB32 converti sur le CPU — une copie et
quelques millisecondes par trame, sur un chemin où l'on compte les deux. Le
corollaire est qu'il faut savoir lire ce que la carte émet vraiment : YUYV,
UYVY, NV12, GREY et MJPEG sont tous dépaquetés dans le shader, le NV12 en
particulier parce que c'est ce que Windows expose nativement pour beaucoup de
périphériques.

**Le YUV ne touche jamais le CPU.** Une trame YUYV part sur le GPU telle quelle,
dans une texture RGBA de demi-largeur : un texel porte `(Y0, U, Y1, V)`, soit
deux pixels voisins partageant leur chrominance. L'UYVY est le même arrangement
dans l'autre ordre. Le NV12, lui, est planaire : sa texture est une bande
d'octets où le plan de chrominance, entrelacé et de demi-résolution, est posé
sous le plan de luminance — une seule texture, un seul transfert. Le dépaquetage
et la conversion en RGB ont lieu dans le shader. La matrice (BT.601 ou BT.709) et
la plage (complète ou réduite) sont lues dans ce que rapporte le pilote — s'y
tromper verdit les images ou délave les noirs.

**Le matériel avant le shader.** Un capteur UVC expose luminosité, contraste,
gamma, exposition et balance des blancs. Ces réglages sont appliqués côté
appareil : ni CPU, ni latence, ni perte de précision. Le shader ne sert que pour
ce que le matériel ne sait pas faire.

**On dessine quand une trame arrive.** Le thread de capture réveille la boucle
d'évènements ; on ne dessine pas au rythme de l'écran en espérant qu'une image
soit prête. La surface est configurée pour une seule trame en vol. Le décodage
MJPEG vit sur le thread de capture, qui a tout le temps d'une trame pour le
faire, et non sur le thread de rendu où ces millisecondes se paieraient en
retard à l'affichage.

**Les limites du GPU, pas celles de wgpu.** Le périphérique est ouvert avec les
limites que l'adaptateur annonce. Celles par défaut de wgpu plafonnent les
textures à 2048 pixels de côté — un héritage du web qui rejetait ici tout format
au-delà du 1080p, alors que le moindre GPU intégré tient le 16384.

**Les scanlines sont moyennées, pas échantillonnées.** Le profil de faisceau est
intégré sur la hauteur que le pixel couvre réellement, et non lu en un point de
la sinusoïde. Sans cela, dès qu'une ligne source occupe moins d'un pixel —
partout en agrandissement ×1, et localement dès qu'on bombe la dalle, la
courbure comprimant les lignes vers les bords — le motif bat sous le pas
d'échantillonnage : de larges franges de moiré au lieu de scanlines. Moyenné, il
s'efface de lui-même là où l'écran ne peut pas le rendre. Le faisceau est en
outre décalé d'un quart de ligne, sans quoi l'agrandissement ×2 (une source 480p
sur un écran 1080p) partage le faisceau à parts égales entre ses deux pixels et
n'affiche aucune ligne.

**Dérive des horloges audio.** Le quartz de la carte d'acquisition et celui du
DAC ne comptent pas à la même vitesse ; quelques dizaines de ppm suffisent à
faire déborder ou se vider un tampon en quelques minutes. Le ratio de
rééchantillonnage est asservi en continu sur le remplissage du tampon, dans une
plage de ±0,5 % — inaudible, là où jeter ou dupliquer des paquets s'entend.

**Une entrée audio par matériel, pas par nom ALSA.** ALSA n'expose pas des
périphériques mais des noms de PCM, et cpal y ajoute les siens : la même entrée
ressort jusqu'à cinq fois (`hw:`, `plughw:`, `default:CARD=`, `sysdefault:`,
`front:`), avec un descriptif identique parce que cpal n'en garde que la première
ligne. La liste regroupe ces routes par PCM réel — la correspondance entre
`CARD=sofhdadsp` et `CARD=0` se lit dans `/proc/asound` — et garde `hw:`, la
route la plus courte, sans conversion ni mixage puisque le transit fait déjà les
deux. Le serveur de son reste proposé à part, pour le cas où la carte est
occupée. WASAPI, lui, nomme ses points de terminaison une fois chacun : la liste
y est directement lisible.

**Arrêter une attente qui n'a pas de fin.** Le thread de capture est arrêté par
un drapeau qu'il teste entre deux trames — encore faut-il qu'il en reçoive une.
V4L2 se plafonne à l'ioctl près, mais `ReadSample` de Media Foundation attend
sans limite de temps : une console éteinte, un câble débranché, et fermer la
fenêtre attendrait pour toujours une trame qui ne viendra pas. Le backend dépose
donc sur la capture de quoi faire échouer l'attente en cours — côté Windows,
éteindre la source. Ce n'est pas garanti d'aboutir, alors l'attente elle-même est
plafonnée : au-delà d'une seconde et demie, le thread est abandonné en le
signalant, plutôt que de figer la fenêtre. La solution de fond est un lecteur de
source asynchrone, où rien ne bloque ; elle reste à faire.

---

## Mesures relevées

Sur une webcam UVC (HP HD Camera), profil `release` :

| Mode | Cadence obtenue | Décodage CPU |
|---|---|---|
| MJPG 1280x720 | 29,8 /s (max annoncé : 30) | 10,8 ms/trame, 32 % d'un cœur |
| YUYV 640x480 | 29,5 /s, 18 Mo/s | aucun |

Le transit audio se verrouille sur sa cible : tampon stable à 20,0 ms, dérive
résiduelle sous 100 ppm, aucun manque ni débordement.

Ces chiffres disent l'essentiel : le décodage MJPEG coûte déjà un tiers de cœur
en 720p30. D'où la préférence donnée au format non compressé chaque fois que la
carte le permet.

---

## Licence

GPL-3.0-or-later. Voir [LICENSE](LICENSE).
