# Herdr DB

Explorateur de bases de données pour [Herdr](https://github.com/herdrdev/herdr), sur le modèle du *Database tool window* d'IntelliJ. Arbre navigable au clavier, grille de données en lecture seule, DDL, Quick Documentation et console SQL, sans quitter le terminal. PostgreSQL et MySQL.

Herdr DB n'a pas de fenêtre à lui : ce sont des panes Herdr. L'arbre reste ouvert en split à gauche ; chaque action ouvre la vue qu'elle demande (grille en split ou en onglet, DDL en onglet, console sous la grille, documentation en popup). On les déplace, zoome ou ferme avec les commandes habituelles de Herdr.

## Installation

```sh
herdr plugin install samirsid-dev/herdr-db
```

- Herdr 0.9.3 ou plus récent, macOS ou Linux (x86_64, ARM64).
- Aucune toolchain Rust : l'étape de build télécharge le binaire de la release correspondant à la version du manifest et vérifie son SHA-256.
- Mise à jour : action **Mettre à jour Herdr DB** dans la palette de Herdr. L'arbre signale une nouvelle version stable au plus une fois par jour ; les panes encore ouverts sur l'ancien binaire indiquent qu'il faut les relancer.

### Afficher l'arbre

L'action **Toggle database tree** affiche ou masque l'arbre dans l'onglet courant. L'arbre s'ouvre à gauche du pane focalisé, sur environ un quart de la largeur : Herdr ne permet pas d'insérer un pane à la racine d'un onglet. Pour lier l'action à une touche, ajoutez dans `~/.config/herdr/config.toml` :

```toml
[[keys.command]]
key = "prefix+alt+d"
type = "shell"
command = "herdr plugin action invoke toggle-tree --plugin herdr-db"
```

`prefix+shift+d` (« prefix+D ») ferme le workspace dans la configuration par défaut de Herdr : choisissez une autre touche ou réaffectez `close_workspace`.

## Configuration

Deux fichiers au même format, qui ne contiennent jamais de secret :

| Fichier | Rôle |
| --- | --- |
| `herdr-db.toml` à la racine du repo, commité | Data sources partagées par l'équipe : dossiers, environnement, schémas, pré-connexion |
| `config.toml` dans `herdr plugin config-dir herdr-db` | Connexions perso et surcharges champ par champ (utilisateur, port local, schémas), réglages, touches |

Le fichier d'équipe est cherché en remontant depuis le worktree ou le répertoire du pane focalisé. Exemple complet : [`herdr-db.example.toml`](herdr-db.example.toml).

```toml
[[folders]]
name = "Hellocare"

[[sources]]
id = "db_prod"
folder = "Hellocare"
engine = "postgres"              # postgres ou mysql
environment = "production"       # local, development, staging, production
host = "localhost"
port = 15432
database = "app"
schemas = ["public"]
read_only = true                 # défaut en production
pre_connect = ["kubectl", "port-forward", "svc/postgres", "15432:5432", "-n", "prod"]
password_command = ["infisical", "secrets", "get", "DB_PASSWORD", "--env=prod", "--path=/db", "--plain"]
```

Dans le fichier personnel, une entrée `[[sources]]` portant un `id` d'équipe ne surcharge que les champs donnés :

```toml
[[sources]]
id = "db_prod"
user = "samir_ro"

[settings]
page_size = 200          # lignes par page de la grille
console_max_rows = 1000  # plafond de lecture de la console
grid_placement = "split" # ou "tab"
icons = "auto"           # auto, nerd-font, ascii

[keys]
edit_data = "e"
execute = "ctrl+r, f5"
```

`herdr-db doctor` affiche les chemins utilisés, les fichiers trouvés et les sources résolues.

### Mots de passe

Ordre de résolution : `password_command`, puis le trousseau du système (Keychain sur macOS, Secret Service sur Linux), puis une saisie dans le pane avec proposition d'enregistrement dans le trousseau. Hors interface : `herdr-db secret set --source db_prod`.

`password_command` s'exécute à la connexion, sous la session du développeur (Infisical, 1Password CLI, Vault…). Sa sortie n'est jamais stockée ni journalisée.

### Environnements et garde-fous

Chaque environnement a sa couleur (local vert, development bleu, staging jaune, production rouge), reprise sur le nœud de l'arbre et dans le bandeau de chaque pane. En production, le bandeau ne peut pas être masqué.

- **Lecture seule par le moteur** : sur une source `read_only`, la session est ouverte en lecture seule (`SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` sur PostgreSQL, `SET SESSION TRANSACTION READ ONLY` sur MySQL). Le moteur refuse toute écriture, quelle que soit la forme de la requête.
- **Mode écriture temporaire** : dans la console, `ctrl+w` puis retaper le nom de la source. Le bandeau passe au rouge clignotant tant que le mode est actif.
- **Timeout** : `statement_timeout` (PostgreSQL) ou `max_execution_time` (MySQL, SELECT uniquement), 30 s par défaut en staging et production.
- **Confirmation** avant `UPDATE`/`DELETE` sans `WHERE`, `TRUNCATE` et `DROP` sur une session qui peut écrire.

Ce sont des garde-fous, pas une frontière de sécurité : un `SET` peut être annulé à la main. La vraie barrière est un rôle en lecture seule. `herdr-db admin readonly-role --source db_prod --user alice_ro --owner app` affiche le script SQL adapté au moteur (rôle par personne, `GRANT SELECT`, `ALTER DEFAULT PRIVILEGES` sur PostgreSQL) sans l'exécuter.

### Pré-connexion

`pre_connect` est lancé avant la connexion, partagé par tous les panes d'une même source et arrêté quand le dernier pane qui l'utilise se ferme. Le plugin attend que le port local réponde (15 s au plus). Si le port est déjà pris par un autre processus, la connexion échoue plutôt que de viser la mauvaise base.

## Raccourcis

Toutes les touches sont configurables dans `[keys]`. `?` affiche l'aide dans chaque pane.

**Arbre**

| Touche | Action |
| --- | --- |
| `j` `k`, flèches | Naviguer |
| `l` `h`, Entrée | Déplier / replier |
| `/` | Speed search |
| `e` | Edit Data (grille en lecture seule) |
| `d` | Go to DDL |
| `K` | Quick Documentation |
| `c` | Console SQL sur la data source |
| `y` | Copy Reference (nom qualifié) |
| `r` / `R` | Refresh / Force Refresh |
| `s` | Sélecteur de schémas |
| `n` | Nouvelle data source |

**Grille** : `]` `[` page suivante et précédente, `{` `}` première et dernière, `s` tri sur la colonne, `f` filtre `WHERE`, Entrée inspecteur de ligne, `F` ligne référencée par la clé étrangère, `y` cellule, `Y` lignes en CSV, `J` lignes en JSON, `v` sélection, `C` `COUNT(*)` exact, `c` console sous la grille, `ctrl+c` annuler.

**Console** : `ctrl+r` (ou `F5`) exécute l'instruction sous le curseur, `ctrl+c` annule, Tab bascule éditeur/résultats, `ctrl+p` `ctrl+n` historique, `ctrl+w` mode écriture, `ctrl+q` fermer.

## Sécurité

- Les plugins Herdr tournent avec les droits de l'utilisateur, **sans sandbox**.
- Un mot de passe n'existe qu'en mémoire, le temps d'ouvrir la connexion : jamais dans un fichier, un log ou un message d'erreur.
- Le state dir est en `0700`, ses fichiers en `0600`. Le cache ne contient que des métadonnées : aucune donnée de ligne n'est écrite sur disque.
- Aucune télémétrie. La seule requête réseau hors bases est la vérification quotidienne de la dernière release sur GitHub (`check_updates = false` pour la couper).

## Développement

Workspace Cargo de quatre crates : `herdr-db-core` (modèle, config, SQL, sans I/O), `herdr-db-drivers` (adapters PostgreSQL et MySQL), `herdr-db-store` (cache SQLite), `herdr-db` (binaire et interface ratatui).

```sh
cargo test --workspace                  # tests unitaires et rendus
scripts/test-databases.sh up            # PostgreSQL et MySQL dans Docker
export HERDR_DB_TEST_POSTGRES="..."     # variables affichées par le script
export HERDR_DB_TEST_MYSQL="..."
cargo test --workspace                  # + tests d'intégration et aller-retour DDL
herdr plugin link .                     # brancher le plugin local sur Herdr
```

Sans release correspondante, `scripts/fetch-binary.sh` compile avec `cargo` s'il est présent. Une release se publie en poussant un tag `vX.Y.Z` : la CI vérifie que le tag, `Cargo.toml` et `herdr-plugin.toml` portent la même version, puis cargo-dist publie les quatre binaires statiques et leurs empreintes.

Licence MIT.
