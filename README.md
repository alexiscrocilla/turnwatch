# Claude Session Monitor

Petite fenêtre Windows, toujours au premier plan, qui montre en direct l'état des sessions Claude Code lancées depuis l'onglet Code de Claude Desktop. L'idée : réduire Claude Desktop et ne garder à l'écran que ce qui compte, c'est-à-dire la session qui t'attend, sinon un simple résumé.

Les sessions sont regroupées par projet, et les projets qui ont besoin de toi passent en premier. Chaque carte tient en une ou deux lignes :

- l'état et sa durée (« attend · 3 h », « en cours · 12 s », « non vue · 5 min »), avec la PR si elle est ouverte, colorée selon la CI ;
- une seconde ligne seulement quand elle sert : la question posée si la session t'attend, l'erreur, ou l'avancement de la liste de tâches (`3/7 · Écrire les tests`) pendant le travail.

Le reste est dans l'infobulle de la carte : branche, fichiers non commités, résumé du dernier tour, dernier outil, session parente, messages entre sessions.

Claude Desktop notifie déjà les fins de tour. Le moniteur n'envoie donc que des **rappels** : une session qui t'attend depuis plus de 5 minutes sans réponse. Il ne le fait pas quand la session est visible dans un volet de Desktop au premier plan, ni pour une simple « question probable ». Plusieurs rappels dus en même temps forment une seule notification (« 3 sessions t'attendent toujours »).

## Prérequis

- Windows 10 ou 11, avec le runtime WebView2 (déjà présent sur Windows 11).
- Claude Desktop avec l'onglet Code. Testé avec Claude Desktop 3.1623 et Claude Code 2.1.308.
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
- **Mode compact** : la fenêtre se réduit à une ligne de 44 px qui montre la session qui t'attend, sinon le résumé.
- **Clic sur une carte** : ouvre cette session dans Claude Desktop (lien `claude://code/continue?session=local_…`, le même que celui de la liste de raccourcis de Desktop) et la marque comme vue.
- **Clic sur la pastille de PR** : ouvre la PR dans le navigateur.
- **Icône de la zone de notification** : sa couleur suit la session la plus urgente, et son infobulle donne le résumé. Un clic affiche ou masque la fenêtre. Le menu permet de changer les réglages et de quitter. Masquer la fenêtre ne ferme pas l'application.

### États

| Pastille | État | Signification |
|---|---|---|
| Ambre | Attend | La session est bloquée sur toi : permission, question, boîte de dialogue, ou tour terminé sur une question (« Décision attendue »). |
| Rouge | Erreur | Le dernier tour s'est arrêté sur une erreur (celle que Desktop affiche, ou une erreur d'API). |
| Bleue | Non vue | Le tour est fini et tu n'as pas regardé la session depuis. |
| Verte | Au travail | Claude est en train de travailler. |
| Grise | Inactive | Rien en cours, déjà vue. |

Comment ces états sont décidés :

- **Attend** vient d'abord du registre de Claude Code (permissions, outil de question). Une session qui finit son tour sur une question est aussi « Attend » : question posée avec des boutons (`ask` de `wrap_up`) ou session classée « bloquée » par Desktop, affichée « Décision attendue ». Un dernier message qui se termine par « ? » donne « Question dans le dernier message », un signal moins sûr. Seul ton prochain message efface ces attentes : les regarder ne suffit pas.
- **Erreur** suit les champs d'erreur de Desktop sur 7 jours. Un simple redémarrage de Desktop ne compte pas. Elle s'efface à ton prochain message.
- **Non vue** se remet à zéro quand tu cliques la carte, ou quand la session est visible dans un volet de Claude Desktop (vue partagée comprise) resté au premier plan au moins une seconde. Les sessions déjà vues sont mémorisées dans `acks.json` et survivent à un redémarrage de l'application. Un tour lancé par une autre session ne rend pas une session « non vue », sauf s'il a produit un résultat.
- La liste de tâches reprend exactement les statuts affichés par Desktop.
- **Sessions en pause** : Claude Desktop ne garde qu'un nombre limité de processus en vie (6 au minimum, davantage selon la mémoire) et arrête les sessions inactives au-delà, ou quand la mémoire manque. Une session sans processus reste listée tant qu'elle attend, est en erreur ou non vue, si son dernier tour ou sa dernière activité date de moins de 7 jours. Son état est déduit du fichier de Desktop et du transcript, comme pour les autres. Son infobulle indique « En pause », et un clic l'ouvre dans Desktop comme d'habitude. Une session en pause et inactive n'est pas listée.

## D'où viennent les données

Tout est lu sur la machine, en lecture seule. Le seul accès réseau passe par `gh`, pour l'état des PR.

| Source | Ce qu'on en tire | Format documenté ? |
|---|---|---|
| `~\.claude\sessions\<pid>.json` | Sessions vivantes, état, raison de l'attente (une session Desktop sans fichier ici est en pause) | Non (même contenu que `claude agents --json`, qui est officiel) |
| `%APPDATA%\Claude\claude-code-sessions\…\local_<id>.json` | Titre, archivage, dernier passage, résumé du tour, PR liée par Desktop, messages entre sessions, session parente | Non |
| `~\.claude\projects\…\<id>.jsonl` | Outil en cours, liste de tâches, question de fin de tour, erreurs d'API, branche, dernier message | Non |
| `%APPDATA%\Claude\claude_desktop_config.json` | Sessions affichées dans les volets de Desktop | Non |
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
| `includeCliSessions` | `false` | Affiche aussi les sessions lancées hors de Claude Desktop |
| `prPollSecs` | `60` | Intervalle de rafraîchissement des PR |
| `messageWindowHours` | `6` | Au-delà, un message sans réponse n'est plus signalé |
| `compact` | `false` | Mode compact |
| `alwaysOnTop` | `true` | Fenêtre toujours au premier plan |

## Ce que l'application écrit sur la machine

- `%APPDATA%\local.claude-session-monitor\` : `settings.json`, `acks.json` (sessions déjà vues) et la position de la fenêtre.
- `HKCU\Software\Classes\AppUserModelId\local.claude-session-monitor` et `%LOCALAPPDATA%\local.claude-session-monitor\toast-icon.png` : le nom et l'icône des notifications, pour un exécutable sans installateur. Supprimer les deux suffit à annuler.

Elle ne modifie rien dans `~\.claude` ni dans les dossiers de Claude Desktop.

## Limites connues

- Desktop n'enregistre pas l'onglet affiché. Quand il est au premier plan sur l'onglet Chat, une session visible dans un volet Code compte comme visible, et aucun rappel n'est envoyé pour elle tant que c'est le cas.
- Une attente qui change de nature sans passer par un autre état (une « question probable » confirmée ensuite par Desktop, par exemple) ne relance pas de rappel.
- Les questions détectées par Desktop ou par le « ? » final disparaissent quand une autre session ou une tâche relance celle-ci. Les questions à boutons, elles, restent jusqu'à ton message.
- Les sessions cloud lancées depuis Desktop n'ont pas de trace locale et n'apparaissent pas.
- Une session en pause dont le dernier tour date de plus de 7 jours n'apparaît plus, même si sa question est restée sans réponse.
- Les builds de développement utilisent l'identifiant `local.claude-session-monitor.dev` et peuvent tourner à côté de la version release.

## Structure

```
crates/monitor-core/   Logique, sans dépendance à Tauri
  src/registry.rs      Registre des processus et vérification qu'ils vivent
  src/desktop.rs       Fichiers de session de Claude Desktop
  src/transcript.rs    Lecture incrémentale des transcripts
  src/git_pr.rs        git et gh, sur un fil d'exécution à part
  src/aggregate.rs     Fusion des sources, règles d'état, tri
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
