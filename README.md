# Claude Session Monitor

Petite fenêtre Windows, toujours au premier plan, qui montre en direct l'état des sessions Claude Code lancées depuis l'onglet Code de Claude Desktop. L'idée : réduire Claude Desktop et ne garder à l'écran que ce qui compte, c'est-à-dire la session qui t'attend, sinon un simple résumé.

Les sessions sont regroupées par projet, et les projets qui ont besoin de toi passent en premier. Chaque carte tient en une ou deux lignes :

- le personnage de la session, dont la pose dit l'état et ce qu'il tient l'outil en cours (voir **États**), puis l'état et sa durée (« attend · 3 h », « en cours · 12 s », « non vue · 5 min »), avec la PR si elle est ouverte, colorée selon la CI, et une petite flèche circulaire quand l'Auto-fix de Desktop la surveille ;
- une seconde ligne seulement quand elle sert : la question posée si la session t'attend (« Plan à approuver : … » pour un plan), l'erreur, ce qu'elle attend une fois son tour fini (« Attend une commande : cargo test »), l'avancement de la liste de tâches (`3/7 · Écrire les tests`) pendant le travail, ou le bilan d'une revue de code qui vient de se terminer (« Revue : 13 résultats dont 5 confirmés »).

Le reste est dans l'infobulle de la carte : branche, fichiers non commités, résumé du dernier tour, dernier outil, session parente, messages entre sessions.

Claude Desktop notifie déjà les fins de tour. Le moniteur n'envoie donc que des **rappels** : une session qui t'attend depuis plus de 5 minutes sans réponse. Il ne le fait pas quand la session est visible dans un volet de Desktop au premier plan, ni pour une simple « question probable ». Plusieurs rappels dus en même temps forment une seule notification (« 3 sessions t'attendent toujours »).

Il prévient aussi quand le quota du forfait passe un seuil : la fenêtre de 5 heures à 80 % puis à 95 %, la semaine à 90 %, avec le délai avant la réinitialisation (« Quota 5 h à 82 % · Réinitialisation dans 1 h 20 »). Chaque seuil n'est annoncé qu'une fois par fenêtre, et un seuil déjà dépassé au lancement du moniteur ne l'est pas. Ces alertes supposent que Claude Desktop relève le quota, ce qu'il ne fait que quelques heures après un clic droit sur son icône (voir **Quota** plus bas). Une notification prévient donc quand il a cessé : une session travaille, tu es devant l'ordinateur, et aucun relevé depuis 20 minutes (« Quota : relevé arrêté »). Une pause de plus de 10 minutes sans toucher clavier ni souris relance ce décompte, car Desktop saute lui aussi ses relevés quand l'ordinateur est inactif ou verrouillé. Elle n'est envoyée qu'une fois par arrêt, et le réglage « Notifier le quota » la coupe aussi.

## Prérequis

- Windows 10 ou 11, avec le runtime WebView2 (déjà présent sur Windows 11).
- Claude Desktop avec l'onglet Code. Testé avec Claude Desktop 4.4212 et Claude Code 2.1.309.
- `git` dans le PATH pour les branches et les fichiers non commités.
- `gh` connecté (`gh auth login`) pour l'état des PR. Sans `gh`, le reste fonctionne.

Pour compiler : Rust stable (MSVC), les Build Tools de Visual Studio, Node.js et pnpm.

## Compiler et lancer

```bash
pnpm install
```

```bash
pnpm tauri build --no-bundle
```

L'exécutable est alors dans `src-tauri\target\release\claude-session-monitor.exe`. Il se lance tel quel, sans installation. `pnpm tauri build` sans `--no-bundle` produit en plus un installateur, mais ce chemin n'a pas encore été essayé.

En développement, avec rechargement de l'interface :

```bash
pnpm tauri dev
```

## Utilisation

- **En-tête** : le résumé (« 1 t'attend · 4 au travail · 2 non vues »), puis trois boutons : mode compact, épingler au premier plan, masquer. Faire glisser l'en-tête déplace la fenêtre.
- **Quota** : sous l'en-tête, une jauge par fenêtre du forfait, 5 heures et 7 jours. La barre fine montre la part utilisée (grise, ambre dès 75 %, rouge dès 90 %), la zone teintée de la même couleur derrière elle la part du temps écoulé dans la fenêtre : une barre qui dépasse sa zone consomme plus vite qu'un rythme régulier. Suivent le pourcentage et l'heure (ou le jour) de réinitialisation estimée. L'infobulle donne le détail et l'heure du relevé. Si Desktop n'a rien relevé depuis plus de 20 minutes, une ligne sous les jauges le dit et indique comment relancer le relevé (« Pas de relevé depuis 08:17 : clic droit sur l'icône de Claude Desktop pour le relancer ») ; une fenêtre réinitialisée depuis affiche « — ».
- **Mode compact** : la fenêtre se réduit à une ligne de 44 px qui montre la session qui t'attend, sinon le résumé, avec le personnage de la session la plus urgente. Dès qu'une fenêtre du quota atteint 80 %, une pastille l'indique à droite (« 5 h 82 % »).
- **Sessions imbriquées** : comme dans la barre latérale de Desktop, une session lancée depuis une autre (session parallèle, copie, ou carte « New task » lancée) s'affiche sous elle, en retrait, reliée par un trait. Tout est au même niveau sous la première session affichée de la lignée, même une petite-fille, et la famille va dans le groupe du projet de cette session, à la place de son membre le plus urgent : un enfant qui t'attend fait remonter son parent. Si aucune session de la lignée n'est affichée, la session reste seule, avec « Lancée depuis … » dans l'infobulle. Le chevron après le titre replie la famille : les sessions qui t'attendent ou sont en erreur restent visibles, une ligne résume les autres (« 2 sessions imbriquées · 1 en cours ») et un clic dessus déplie. Le repli est mémorisé dans la fenêtre.
- **Tâches proposées** : les cartes « New task » qu'une session a proposées et que tu n'as encore ni lancées ni écartées apparaissent sous cette session quand elle est affichée, sinon dans le groupe de leur projet après ses sessions, en pointillés (« proposée · 2 h »), avec leur argumentaire en seconde ligne. Une seule à la fois demande ton attention, par famille de sessions ou par projet : la plus récente. Les autres attendent derrière une ligne (« 2 tâches proposées ») qu'un clic déplie. Une tâche y passe aussi quand tu as écrit deux fois à sa session depuis qu'elle a été proposée sans la lancer : elle ne compte alors plus dans les tâches annoncées. Un clic sur la carte ouvre la session qui l'a proposée : c'est là, dans Desktop, qu'on lance ou écarte la carte, aucun lien ne permet de le faire de l'extérieur. La croix (« Pas pour moi ») masque la tâche dans le moniteur seulement, Desktop la garde. Ce choix est mémorisé dans la fenêtre. Une carte n'est montrée que pendant 7 jours après sa proposition : c'est la durée de vie que Desktop donne à ses propositions, et le moniteur l'applique aussi aux anciennes cartes, que Desktop garde sans limite (l'infobulle dit laquelle des deux règles s'applique).
- **Clic sur une carte** : ouvre cette session dans Claude Desktop (lien `claude://code/continue?session=local_…`, le même que celui de la liste de raccourcis de Desktop) et la marque comme vue.
- **Clic sur la pastille de PR** : ouvre la PR dans le navigateur. Quand l'Auto-fix de Desktop surveille la PR (flèche circulaire), Desktop relance lui-même la session si la CI échoue, en cas de conflit ou de commentaire de revue. Tant que la session travaille sur une CI rouge ou un conflit, l'infobulle dit « Auto-fix s'en charge ». Desktop ne la relance qu'une fois par échec : si elle t'attend, s'arrête sur une erreur ou finit sans avoir réglé le problème, c'est à toi de voir. L'état vient de la carte de PR que Desktop garde pour la session, avec la même règle que lui (PR ouverte, sur GitHub, ni écartée ni héritée d'une session copiée), et n'est montré que si Desktop tourne.
- **Icône de la zone de notification** : sa couleur suit la session la plus urgente, et son infobulle donne le résumé, puis le quota sur une seconde ligne. Un clic affiche ou masque la fenêtre. Le menu permet de changer les réglages, de lancer le moniteur au démarrage de Windows et de quitter. Masquer la fenêtre ne ferme pas l'application.

### États

| Personnage | État | Signification |
|---|---|---|
| Pince levée, « ? » ambre | Attend | La session est bloquée sur toi : permission, question, boîte de dialogue, ou tour terminé sur une question (« Décision attendue »). Le « ? » est pâle quand la question est seulement devinée. |
| Contrarié, « ! » rouge | Erreur | Le dernier tour s'est arrêté sur une erreur (celle que Desktop affiche, ou une erreur d'API). |
| Drapeau bleu | Non vue | Le tour est fini et tu n'as pas regardé la session depuis. |
| Sur ses pattes, un outil en main | Au travail | Claude travaille, ou a fini son tour et attend une tâche de fond qui le relancera. |
| Gris, les yeux fermés | Inactive | Rien en cours, déjà vue. |

Au travail, le personnage tient ce que fait la session : un stylo pour une modification, une clé pour une commande, une loupe pour une lecture ou une recherche, un globe pour le web, et un petit crabe l'accompagne quand elle lance un sous-agent. Il fait un pas à chaque appel d'outil et ne bouge pas entre deux : il n'y a aucune animation en continu, qui ferait travailler WebView2 toute la journée. Le pas est supprimé si Windows demande de réduire les animations.

Comment ces états sont décidés :

- **Attend** vient d'abord des tâches demandées : quand tu demandes une tâche à part, Desktop bloque la session jusqu'à ce que tu choisisses « Nouvelle session » ou « Ici ». Le registre la montre alors au travail ; le moniteur l'affiche « Attend » avec « Nouvelle session pour « … », ou ici ? ». Puis vient le registre de Claude Code (permissions, outil de question). Quand la permission demandée est l'approbation d'un plan (`ExitPlanMode`), la carte dit « Plan à approuver » avec le titre du plan. Une session qui finit son tour sur une question est aussi « Attend » : question posée avec des boutons (`ask` de `wrap_up`) ou session classée « bloquée » par Desktop, affichée « Décision attendue ». Un dernier message qui se termine par « ? » donne « Question dans le dernier message », un signal moins sûr. Seul ton prochain message efface ces attentes : les regarder ne suffit pas.
- **Au travail** vient du registre pendant un tour. Celui-ci repasse à `idle` dès la fin du tour, même si du travail continue en arrière-plan. Le moniteur suit donc ce travail dans le transcript, jusqu'à la notification de fin : commandes de fond (y compris celles déplacées en fond à leur délai), sous-agents, workflows, moniteurs et réveils planifiés (`ScheduleWakeup`). La carte dit ce que la session attend, et l'état « Non vue » n'arrive qu'à la fin de ce travail. Les questions et les erreurs restent prioritaires. Ce travail meurt avec le processus : celui d'un processus remplacé ne compte plus, et une session en pause n'attend rien. Une session attend aussi les sessions parallèles qu'elle a lancées tant qu'elles travaillent, car Desktop la relance ensuite (« Attend 2 sessions ») ; pas une session détachée, ni une session lancée depuis une carte « New task ».
- **Erreur** suit les champs d'erreur de Desktop sur 7 jours. Un simple redémarrage de Desktop ne compte pas. Elle s'efface à ton prochain message.
- **Non vue** se remet à zéro quand tu cliques la carte, ou quand la session est visible dans un volet de Claude Desktop (vue partagée comprise) resté au premier plan au moins une seconde. Les sessions déjà vues sont mémorisées dans `acks.json` et survivent à un redémarrage de l'application. Un tour lancé par une autre session ne rend pas une session « non vue », sauf s'il a produit un résultat.
- La liste de tâches reprend exactement les statuts affichés par Desktop.
- **Revue de code** : quand une session rend le résultat d'une revue (outil `ReportFindings`), le moniteur compte les résultats, ceux confirmés et ceux pas encore traités. Le bilan s'affiche tant qu'il en reste à traiter et que tu n'as pas écrit à la session depuis ; en seconde ligne d'une session non vue, dans l'infobulle sinon.
- **Quota** : Claude Desktop relève l'usage du forfait auprès de claude.ai et garde 30 jours d'historique. Depuis sa version 3.6556, il ne le fait qu'à son ouverture, puis toutes les 5 à 15 minutes pendant les 5 heures qui suivent un clic droit sur son icône dans la zone de notification (durée réglée à distance par Desktop). Sans ce clic droit, la jauge de 5 h passe à « — » au plus tard 5 heures après le dernier relevé, et celle de 7 jours garde l'ancienne valeur. L'historique ne contient que des pourcentages, sans heure de réinitialisation : le moniteur la déduit des baisses. La fenêtre de 5 h commence au premier usage après la fin de la précédente ; son heure n'est donnée que si l'historique la situe à 30 minutes près. La fenêtre de 7 jours se réinitialise à heure fixe chaque semaine : les baisses des semaines précédentes, décalées d'une semaine, se recoupent et précisent ce créneau (une réinitialisation exceptionnelle hors calendrier est ignorée) ; il n'est donné qu'à 6 heures près.
- **Sessions en pause** : Claude Desktop ne garde qu'un nombre limité de processus en vie (6 au minimum, davantage selon la mémoire) et arrête les sessions inactives au-delà, ou quand la mémoire manque. Une session sans processus reste listée tant qu'elle attend, est en erreur ou non vue, si son dernier tour ou sa dernière activité date de moins de 7 jours. Son état est déduit du fichier de Desktop et du transcript, comme pour les autres. Son infobulle indique « En pause », et un clic l'ouvre dans Desktop comme d'habitude. Une session en pause et inactive n'est pas listée.

## D'où viennent les données

Tout est lu sur la machine, en lecture seule. Le seul accès réseau passe par `gh`, pour l'état des PR.

| Source | Ce qu'on en tire | Format documenté ? |
|---|---|---|
| `~\.claude\sessions\<pid>.json` | Sessions vivantes, état, raison de l'attente (une session Desktop sans fichier ici est en pause) | Non (même contenu que `claude agents --json`, qui est officiel) |
| `%APPDATA%\Claude\claude-code-sessions\…\local_<id>.json` | Titre, archivage, dernier passage, résumé du tour, PR liée par Desktop et son Auto-fix, messages entre sessions, session parente, cartes « New task » en attente | Non |
| `~\.claude\projects\…\<id>.jsonl` | Outil en cours, tâches de fond, liste de tâches, question de fin de tour, erreurs d'API, branche, dernier message, plan à approuver, résultats de revue, tes messages depuis une tâche proposée | Non |
| `%APPDATA%\Claude\claude_desktop_config.json` | Sessions affichées dans les volets de Desktop | Non |
| `%APPDATA%\Claude\plan-usage-history.json` | Quota du forfait (fenêtres de 5 h et de 7 jours), tel que Desktop l'a relevé | Non |
| `git` dans le dossier de la session | Branche, fichiers non commités | Oui |
| `gh pr view` / `gh pr list --head` | État de la PR, CI, relecture | Oui |

Ces formats internes changent avec les versions de Claude Code et de Desktop. Les lecteurs tolèrent les champs manquants et les fichiers en cours d'écriture, mais une mise à jour peut casser une partie de l'affichage. Pour vérifier ce que l'application voit, sans fenêtre :

```bash
cargo run --manifest-path crates/monitor-core/Cargo.toml --bin dump -- --summary
```

`--watch` affiche chaque changement en continu ; sans option, la commande affiche l'instantané complet en JSON.

## Réglages

Ils sont modifiables depuis le menu de l'icône et enregistrés dans `%APPDATA%\local.claude-session-monitor\settings.json` :

| Clé | Défaut | Effet |
|---|---|---|
| `notifyOnWaiting` | `true` | Rappel quand une session t'attend depuis 5 minutes |
| `notifyOnFinished` | `false` | Notification immédiate quand une session termine son tour (Desktop le fait déjà) |
| `notifyOnError` | `true` | Rappel quand une erreur reste sans réponse 5 minutes |
| `notifyOnQuota` | `true` | Notification quand le quota passe 80 % puis 95 % (5 heures) ou 90 % (semaine) |
| `includeCliSessions` | `false` | Affiche aussi les sessions lancées hors de Claude Desktop |
| `prPollSecs` | `60` | Intervalle de rafraîchissement des PR |
| `messageWindowHours` | `6` | Au-delà, un message sans réponse n'est plus signalé |
| `compact` | `false` | Mode compact |
| `alwaysOnTop` | `true` | Fenêtre toujours au premier plan |

## Ce que l'application écrit sur la machine

- `%APPDATA%\local.claude-session-monitor\` : `settings.json`, `acks.json` (sessions déjà vues) et la position de la fenêtre.
- `HKCU\Software\Classes\AppUserModelId\local.claude-session-monitor` et `%LOCALAPPDATA%\local.claude-session-monitor\toast-icon.png` : le nom et l'icône des notifications, pour un exécutable sans installateur. Supprimer les deux suffit à annuler.
- Seulement si « Lancer au démarrage de Windows » est coché : la valeur `local.claude-session-monitor` de `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, qui contient le chemin de l'exécutable. C'est elle que liste « Applications au démarrage » dans les paramètres de Windows. Désactiver l'application là (ou dans le Gestionnaire des tâches) décoche aussi la case du menu ; la recocher efface ce réglage de Windows (la valeur du même nom sous `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run`). Décocher la case supprime les deux ; si l'exécutable a été déplacé, la valeur est remise sur son nouveau chemin au lancement suivant. La version de développement (`pnpm tauri dev`) ne propose pas cette case : elle dépend du serveur de développement.

Elle ne modifie rien dans `~\.claude` ni dans les dossiers de Claude Desktop.

## Limites connues

- Desktop n'enregistre pas l'onglet affiché. Quand il est au premier plan sur l'onglet Chat, une session visible dans un volet Code compte comme visible, et aucun rappel n'est envoyé pour elle tant que c'est le cas.
- Une attente qui change de nature sans passer par un autre état (une « question probable » confirmée ensuite par Desktop, par exemple) ne relance pas de rappel.
- Les questions détectées par Desktop ou par le « ? » final disparaissent quand une autre session ou une tâche relance celle-ci. Les questions à boutons, elles, restent jusqu'à ton message.
- Une commande de fond compte 2 heures au plus (les autres tâches, 24 heures). Un serveur de développement lancé en fond garde donc sa session verte 2 heures, et une très longue commande passe en inactive ou non vue avant sa fin. Sur 520 commandes mesurées, 3 ont dépassé 2 heures.
- Les tâches planifiées avec `CronCreate` ne comptent pas comme une attente : le moniteur ne les suit pas. Une commande lancée dans le panneau Terminal de Desktop non plus : sa fin ne relance pas la session.
- Les sessions cloud lancées depuis Desktop n'ont pas de trace locale et n'apparaissent pas.
- Une session en pause dont le dernier tour date de plus de 7 jours n'apparaît plus, même si sa question est restée sans réponse.
- Le quota n'est relevé que quand Claude Desktop est ouvert, au plus toutes les 5 minutes : la jauge peut retarder de quelques minutes sur l'usage réel. Ses heures de réinitialisation sont des estimations tirées de l'historique ; elles manquent tant que l'historique ne montre pas de réinitialisation assez nette.
- Les builds de développement utilisent l'identifiant `local.claude-session-monitor.dev` et peuvent tourner à côté de la version release.

## Structure

```
crates/monitor-core/   Logique, sans dépendance à Tauri
  src/registry.rs      Registre des processus et vérification qu'ils vivent
  src/desktop.rs       Fichiers de session de Claude Desktop
  src/transcript.rs    Lecture incrémentale des transcripts
  src/git_pr.rs        git et gh, sur un fil d'exécution à part
  src/aggregate.rs     Fusion des sources, règles d'état, tri
  src/usage.rs         Historique du quota et estimation des réinitialisations
  src/engine.rs        Surveillance des dossiers et reconstruction de l'instantané
  src/bin/dump.rs      Outil de diagnostic en ligne de commande
src-tauri/             Fenêtre, icône, notifications, commandes
src/                   Interface (TypeScript sans framework)
```

`crates/monitor-core/src/model.rs` définit le contrat de données, recopié à la main dans `src/types.ts`.

## Tests

```bash
cargo test --manifest-path crates/monitor-core/Cargo.toml
```

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

Hors de Tauri, `pnpm dev` sert l'interface sur http://localhost:1420 avec des données fictives (`src/mock.ts`), utile pour travailler le design.
