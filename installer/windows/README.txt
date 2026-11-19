2moon Host pour Windows
=======================

2moon Host diffuse l'écran, le son et les jeux de ce PC vers le client 2moon
ou Moonlight. Il tourne en service Windows ("2moon") qui démarre avec
l'ordinateur et suit la session ouverte, comme Sunshine.

Premiers pas
------------
1. Ouvrez « Administration 2moon » (menu Démarrer) sur ce PC et créez le
   compte administrateur. Le navigateur signale un certificat inconnu : c'est
   normal, l'hôte utilise son propre certificat.
2. Sur l'appareil client, ajoutez cet hôte (il est détecté sur le réseau
   local) et entrez le code PIN affiché dans le panneau d'administration.
3. Lancez « Desktop » ou une application ajoutée dans le panneau.

Encodage vidéo : NVIDIA (NVENC), AMD (AMF) ou Intel (Quick Sync), choisi
automatiquement ; à défaut, encodage logiciel H.264.

Manettes : nécessitent le pilote ViGEmBus (proposé par l'installateur).

Fichiers
--------
- Configuration, appairages : C:\ProgramData\2moon (accès SYSTEM et
  administrateurs uniquement)
- Journaux : C:\ProgramData\2moon\logs (host.log, service.log)
- Options avancées de l'hôte : C:\ProgramData\2moon\host-args.txt, une option
  par ligne (par exemple « --output » puis « DISPLAY2 » pour un autre écran),
  puis redémarrez le service 2moon.

Ports utilisés : TCP 47984, 47989, 47990 (administration), 48010 ;
UDP 47998 à 48000. Sunshine utilise les mêmes : un seul des deux peut
fonctionner à la fois.

Pare-feu : l'hôte n'est joignable que depuis les réseaux privés (domicile,
travail). Si le client ne trouve pas l'hôte, vérifiez dans Paramètres >
Réseau et Internet que votre réseau est « Privé », ou réinstallez en cochant
« Autoriser sur les réseaux publics ».

Licence : GPL 3.0 ou ultérieure (LICENSE.txt). Bibliothèques incluses :
voir le dossier LICENSES.
