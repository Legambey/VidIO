# VidIO

Moniteur vidéo et audio à faible latence pour cartes d'acquisition USB — pensé
pour jouer sur une console rétro branchée en HDMI/composite sur un PC, sans
subir le retard d'un logiciel de capture généraliste.

## État

Fonctionnel de bout en bout : capture V4L2, décodage, rendu GPU, filtres CRT,
overlay de réglages et transit audio. Les sous-commandes de diagnostic restent
disponibles à côté de la fenêtre.

## Dépendances

Arch Linux :

```
sudo pacman -S --needed rust alsa-lib
```

`v4l-utils` n'est **pas** nécessaire : la crate `v4l` est utilisée avec sa
feature `v4l2` par défaut, qui parle directement aux ioctls du noyau sans passer
par `libv4l2`. Le paquet reste utile pour diagnostiquer (`v4l2-ctl`), rien de
plus. `alsa-lib` est déjà présent sur toute machine avec PipeWire.

L'utilisateur doit appartenir au groupe `video` pour ouvrir `/dev/videoN`.

## Utilisation

```bash
cargo run --release --                # ouvre la fenêtre sur le dernier périphérique
cargo run --release -- run 0          # ou sur celui qu'on désigne
cargo run -- list                    # périphériques vidéo et audio
cargo run -- formats 0               # formats, résolutions, cadences
cargo run -- controls 0              # contrôles matériels du capteur
cargo run -- set 0 brightness -8     # écrit et mémorise dans le profil
cargo run -- bench 0 --secs 5        # cadence réelle, pertes, coût du décodage
cargo run -- audio --secs 10 --gain 0.5   # transit entrée → sortie
cargo run -- config                  # chemin et contenu de la configuration
```

Un périphérique se désigne par son index, son nœud (`/dev/video0`), sa clé
stable ou un fragment de son nom.

## Mesures relevées

Sur une webcam UVC (HP HD Camera), profil `release` :

| Mode | Cadence obtenue | Décodage CPU |
|---|---|---|
| MJPG 1280x720 | 29,8 /s (max annoncé : 30) | 10,8 ms/trame, 32 % d'un cœur |
| YUYV 640x480 | 29,5 /s, 18 Mo/s | aucun |

Le transit audio se verrouille sur sa cible : tampon stable à 20,0 ms, dérive
résiduelle sous 100 ppm, aucun manque ni débordement.

Ces chiffres disent l'essentiel : le décodage MJPEG coûte déjà un tiers de cœur
en 720p30. En 1080p60 il dépasserait le cœur entier — d'où la préférence donnée
au format non compressé chaque fois que la carte le permet.

## Choix des périphériques

Le panneau (`Tab`) propose trois listes déroulantes : la source vidéo, son
format, et les entrée/sortie audio. Tout se change à chaud, sans quitter.

Au démarrage, VidIO reprend le dernier périphérique utilisé. S'il a été
débranché, il prend le premier disponible plutôt que d'échouer. Même règle pour
l'audio, mais côté par côté : si la sortie mémorisée ne s'ouvre pas, l'entrée
choisie est conservée, et l'écran le dit. Un périphérique demandé explicitement
en ligne de commande, lui, doit exister — se rabattre en silence sur un autre
serait pire que l'erreur.

« Par défaut » veut dire « ce que le système désigne, s'il s'ouvre ». Le
`default` d'ALSA ne mène pas forcément quelque part : sans le fichier qui le
redirige vers le serveur de son (`/etc/alsa/conf.d/99-pipewire-default.conf`,
fourni par `pipewire-alsa`), il pointe sur la carte que le serveur tient déjà
ouverte et renvoie « périphérique occupé ». VidIO prend alors le premier PCM
qui s'ouvre — le serveur de son d'abord, le matériel ensuite.

Chaque source garde son propre profil : format, colorimétrie, filtre CRT,
contrôles matériels et choix audio. Changer de carte recharge le sien.

Chaque section du panneau se remet à ses valeurs d'origine d'un bouton — les
contrôles du capteur reprennent les défauts annoncés par le pilote, pas des
valeurs choisies par VidIO. Le bouton « tout réinitialiser » du bas remet le
profil entier du périphérique courant, et demande confirmation.

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

Chaque changement s'affiche deux secondes au bas de l'écran. Les réglages sont
enregistrés en quittant, dans le profil du périphérique.

## Architecture

```
src/
├── main.rs                 sous-commandes de diagnostic
├── config.rs               TOML persistant, un profil par périphérique
├── device.rs               identité stable des périphériques
├── camera/
│   ├── mod.rs              trait Camera, formats, plomberie de trames
│   └── v4l2.rs             backend Linux (ioctls V4L2 directs)
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

### Décisions structurantes

**Clé de périphérique stable.** `/dev/video0` n'identifie rien : l'ordre change
au boot et une carte expose plusieurs nœuds. La clé vient de
`/dev/v4l/by-id`, dont le nom contient le numéro de série — les réglages
survivent donc à un changement de port USB, et deux exemplaires du même modèle
gardent des profils distincts.

**La dernière trame gagne.** Le thread de capture n'attend jamais le
consommateur : si le rendu décroche, la trame en attente est écrasée. Accumuler
un retard qu'on ne rattrapera jamais est pire que sauter une image. Les tampons
circulent entre les deux threads via un pool pour ne pas allouer 60 fois par
seconde.

**Le matériel avant le shader.** Un capteur UVC expose luminosité, contraste,
gamma, exposition et balance des blancs. Ces réglages sont appliqués côté
appareil : ni CPU, ni latence, ni perte de précision. Le shader ne sert que pour
ce que le matériel ne sait pas faire.

**Non compressé de préférence.** À résolution et cadence égales, la négociation
choisit le YUYV plutôt que le MJPEG : il part sur le GPU sans décodage. Le MJPEG
n'est retenu que lorsque la bande passante USB ne laisse pas le choix — la
commande `bench` mesure ce que son décodage coûte réellement.

**Le YUYV ne touche jamais le CPU.** Une trame YUYV part sur le GPU telle
quelle, envoyée dans une texture RGBA de demi-largeur : un texel porte
`(Y0, U, Y1, V)`, soit deux pixels voisins partageant leur chrominance. Le
dépaquetage et la conversion en RGB ont lieu dans le shader. La matrice
(BT.601 ou BT.709) et la plage (complète ou réduite) sont lues dans ce que
rapporte le pilote — s'y tromper verdit les images ou délave les noirs.

**Les limites du GPU, pas celles de wgpu.** Le périphérique est ouvert avec
les limites que l'adaptateur annonce. Celles par défaut de wgpu plafonnent les
textures à 2048 pixels de côté — un héritage du web qui rejetait ici tout
format au-delà du 1080p, alors que le moindre GPU intégré tient le 16384. La
liste des formats est filtrée par cette limite : une taille que le GPU ne peut
pas recevoir n'est pas proposée, et si elle arrive quand même — profil
enregistré, ligne de commande — elle est refusée avec un message plutôt qu'en
tuant le processus, wgpu traitant ses erreurs de validation en dehors de tout
`Result`.

**On dessine quand une trame arrive.** Le thread de capture réveille la boucle
d'évènements ; on ne dessine pas au rythme de l'écran en espérant qu'une image
soit prête. Le mode de présentation vise `Immediate`, et la surface est
configurée pour une seule trame en vol.

**Le décodage MJPEG vit sur le thread de capture**, qui a tout le temps d'une
trame pour le faire, et non sur le thread de rendu où ces millisecondes se
paieraient en retard à l'affichage.

**Une entrée audio par matériel, pas par nom ALSA.** ALSA n'expose pas des
périphériques mais des noms de PCM, et cpal y ajoute les siens : la même entrée
ressort jusqu'à cinq fois (`hw:`, `plughw:`, `default:CARD=`, `sysdefault:`,
`front:`), avec un descriptif identique parce que cpal n'en garde que la
première ligne. La liste regroupe ces routes par PCM réel — la correspondance
entre `CARD=sofhdadsp` et `CARD=0` se lit dans `/proc/asound`, qui fournit au
passage le nom du sous-périphérique — et garde `hw:`, la route la plus courte :
sans conversion ni mixage, puisque le transit fait déjà les deux lui-même. Le
serveur (`pipewire`) reste proposé à part, pour le cas où la carte est occupée.

**Les scanlines sont moyennées, pas échantillonnées.** Le profil de faisceau
est intégré sur la hauteur que le pixel couvre réellement, et non lu en un
point de la sinusoïde. Sans cela, dès qu'une ligne source occupe moins d'un
pixel — partout en agrandissement ×1, et localement dès qu'on bombe la dalle,
la courbure comprimant les lignes vers les bords — le motif bat sous le pas
d'échantillonnage : de larges franges de moiré au lieu de scanlines. Moyenné,
il s'efface de lui-même là où l'écran ne peut pas le rendre. Le faisceau est en
outre décalé d'un quart de ligne, sans quoi l'agrandissement ×2 (une source
480p sur un écran 1080p) partage le faisceau à parts égales entre ses deux
pixels et n'affiche aucune ligne.

**Dérive des horloges audio.** Le quartz de la carte d'acquisition et celui du
DAC ne comptent pas à la même vitesse ; quelques dizaines de ppm suffisent à
faire déborder ou se vider un tampon en quelques minutes. Le ratio de
rééchantillonnage est asservi en continu sur le remplissage du tampon, dans une
plage de ±0,5 % — inaudible, là où jeter ou dupliquer des paquets s'entend.
