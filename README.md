# macDirStat

`macDirStat` ist eine schnelle Terminaloberfläche zum Auffinden großer Dateien und
Verzeichnisse auf macOS. Ordnergrößen werden parallel im Hintergrund berechnet,
während Navigation, Suche und Filter bedienbar bleiben.

## Installation

Voraussetzung ist Rust 1.85 oder neuer.

```bash
cargo install --path . --locked
```

Danach ist das Programm unter seinem exakten Namen aufrufbar:

```bash
macDirStat
macDirStat ~/Library
macDirStat --workers 4 --size-mode physical ~/Library
```

Für einen Start direkt aus dem Quellverzeichnis:

```bash
cargo run --release -- ~/Library
```

### Fertige macOS-Binaries

GitHub Releases enthalten getrennte Archive für Apple Silicon (`arm64`) und
Intel-Macs (`x86_64`). Nach dem Download kann das passende Archiv beispielsweise
so installiert werden:

```bash
tar -xzf macDirStat-v0.3.0-macos-arm64.tar.gz
mkdir -p ~/.local/bin
install -m 755 macDirStat-v0.3.0-macos-arm64/macDirStat ~/.local/bin/macDirStat
```

## Bedienung

Mit `?` öffnet sich jederzeit die vollständige, scrollbare Hilfe.

| Taste | Aktion |
|---|---|
| `Tab` / `Shift+Tab`, `1`…`5` | Zwischen Tree, Largest, Types, Duplicates und Changes wechseln |
| `↑` / `↓`, `k` / `j` | Auswahl bewegen |
| `→` / `l` | Ordner öffnen |
| `←` / `h` | Ordner schließen beziehungsweise zum Parent-Node springen |
| `Enter` | Ordner auf-/zuklappen oder Analysepfad im Tree aufdecken |
| `g` / `G` | Zum ersten beziehungsweise letzten sichtbaren Eintrag springen |
| `Home` / `End`, `PgUp` / `PgDn` | Direkt oder seitenweise navigieren |
| `Backspace` | Parent-Verzeichnis als neuen Scan-Root öffnen |
| `/`, `n` / `N` | Geladene Nodes suchen und Treffer wechseln |
| `f` / `F` | Kombinierten Filter setzen beziehungsweise löschen |
| `s` / `S` | Sortiermodus wechseln beziehungsweise Richtung umkehren |
| `z` | Zwischen logischer und physischer Größe wechseln |
| `i` | Adaptives Detailpanel ein- oder ausblenden |
| `t` | Theme wechseln |
| `m` | Maussteuerung ein- oder ausschalten |
| `w` | Nativen Dateisystem-Watcher ein- oder ausschalten |
| `e` | Vollständigen Analyseindex als JSON exportieren |
| `Esc` | Laufende Scan-Jobs abbrechen |
| `Space` | Eintrag für eine Mehrfachaktion markieren oder entmarkieren |
| `x` | Alle markierten Einträge gemeinsam in den Papierkorb verschieben |
| `d` | Ausgewählten Eintrag nach Bestätigung in den Papierkorb verschieben |
| `r` | Root vollständig und ohne Cache neu scannen |
| `q` / `Ctrl+C` | Beenden |

Die normale Tree-Ansicht bleibt schnell und scannt Verzeichnisse bei Bedarf. Beim
ersten Öffnen eines Analyse-Tabs baut macDirStat einmalig im Hintergrund einen
vollständigen Index auf. `Largest` zeigt einzelne Dateien, `Types` gruppiert nach
Kategorie, Erweiterung und Alter, `Duplicates` verifiziert Kandidaten exakt mit
BLAKE3 und `Changes` vergleicht den Index mit dem letzten vollständigen Lauf.
Hardlinks werden bei der Duplikatsuche als Aliase derselben Datei behandelt;
leere Dateien sind keine Duplikatkandidaten.

Filterprädikate werden mit AND verknüpft. Neben Namen und in Anführungszeichen
gesetzten Phrasen sind beispielsweise `>1GiB`, `size>500MB`, `age>30d`,
`ext:log`, `ext:none` und `type:image` möglich. Kategorien sind `image`, `video`,
`audio`, `archive`, `document`, `code` und `other`. Suche und Filter im Tree
arbeiten ausschließlich auf geladenen Nodes; Analyse-Tabs filtern den bereits
erstellten Index. Das Detailpanel erscheint auf breiten Terminals rechts und auf
schmaleren Terminals unterhalb der Liste.

Die Maussteuerung ist standardmäßig deaktiviert. Nach `m` wählt ein Klick eine
Zeile aus, ein Klick auf die Checkbox markiert sie, ein Doppelklick klappt einen
Ordner um und das Mausrad bewegt die Auswahl.

## Größen, Mountpoints und Cache

Ein Scan erfasst gleichzeitig:

- logische Größe anhand der Dateilänge,
- physische Größe anhand der belegten Dateisystemblöcke,
- Anzahl regulärer Dateien je Ordner.

Symlinks werden angezeigt und ihre eigenen Metadaten gezählt, aber niemals
verfolgt. Hardlinks werden pro sichtbarem Pfadeintrag gezählt; es findet keine
globale Inode-Deduplizierung statt.

Standardmäßig bleibt ein Scan auf dem Dateisystem des Startpfads. Darunter
eingebundene Volumes bleiben als Mountpoints sichtbar, werden aber nicht
durchlaufen. Mit `--cross-filesystems` lässt sich dieses Verhalten bewusst
abschalten.

Fertige Verzeichniswerte werden standardmäßig 24 Stunden unter
`~/Library/Caches/macDirStat/scan-cache-v1.json` gespeichert. Cachetreffer sind in
der Baumansicht mit `≈` und im Detailpanel als `cached` markiert. Die Validierung
verwendet Pfad, Device, Inode und Änderungszeit; tiefe Inhaltsänderungen können
daher bis zum Ablauf des Cacheeintrags veraltet erscheinen. `r` verwirft den Cache
für den aktuellen Root und erzwingt einen frischen Scan. Nach Trash-Operationen
werden nur entfernte Pfade invalidiert und die betroffenen Ancestors neu gescannt.

Der mit `w` oder `--watch` aktivierbare native Watcher verwendet auf macOS
FSEvents. Ereignisse werden standardmäßig 750 ms gesammelt; nur betroffene
geladene Teilbäume und ihre Ancestors werden erneuert. Bei einem Overflow oder
mehr als 1.000 Ereignissen wird genau ein Root-Refresh ausgeführt. Es gibt keinen
periodischen Hintergrundscan.

Vollständige Analysen schreiben standardmäßig einen rollierenden Vergleich unter
`~/Library/Application Support/macDirStat/snapshots/`. Der Schlüssel enthält den
verlustfreien Root-Pfad und die Mount-Policy. Ein unvollständiger oder
abgebrochener Lauf ersetzt die Baseline nicht; unzugängliche Teilbäume werden
nicht fälschlich als entfernt gewertet. `--no-snapshots` schaltet das aus.

## Konfiguration

Die optionale Konfiguration liegt auf macOS unter:

```text
~/Library/Application Support/macDirStat/config.toml
```

Beispiel:

```toml
workers = 6
size_mode = "logical"
one_file_system = true
cache = true
cache_ttl_hours = 24
mouse = false
theme = "default"
detail_panel = true
sort = "size"
sort_direction = "descending"
watch = false
watch_debounce_ms = 750
snapshots = true
```

Mögliche Themes sind `default`, `monochrome` und `high-contrast`. Sortiert werden
kann nach `size`, `name`, `files` oder `kind`, jeweils `ascending` oder
`descending`.

Die Priorität lautet: eingebaute Defaults, TOML-Datei, CLI-Argumente. Ein anderer
Konfigurationspfad kann mit `--config PATH` gewählt werden. Alle Optionen zeigt:

```bash
macDirStat --help
```

Unter anderem stehen `--one-file-system`/`--cross-filesystems`,
`--cache`/`--no-cache`, `--mouse`/`--no-mouse`,
`--details`/`--no-details`, `--watch`/`--no-watch`,
`--snapshots`/`--no-snapshots`, `--theme`, `--sort` und `--sort-direction` bereit.

Ein vollständiger JSON-Export ohne TUI ist ebenfalls möglich. Diagnosen und die
Kurzstatistik gehen nach stderr, damit stdout bei `-` gültiges JSON bleibt:

```bash
macDirStat --export-json scan.json ~/Library
macDirStat --export-json - --detect-duplicates ~/Library >scan.json
macDirStat --export-json changes.json --compare-snapshot scan.json ~/Library
```

Das Schema enthält Dateien, Verzeichnisse, beide Größen, Datei- und
Altersstatistiken, Fehler, optionale Duplikatgruppen und Veränderungen. Pfade
werden sowohl lesbar als auch verlustfrei als Base64-kodierte OS-Bytes abgelegt.

## Löschen und Sicherheit

Eine einzelne oder mehrfache Löschung wird ausschließlich mit `y` bestätigt. `n`
und `Esc` brechen den Dialog ab. Wird ein Ordner markiert, entfernt `macDirStat`
bereits vorhandene Markierungen seiner Kinder, damit kein Pfad doppelt gelöscht
wird. Alle Pfade einer Mehrfachauswahl werden erneut validiert, bevor der erste
Eintrag in den Papierkorb verschoben wird.

- Gelöscht wird nicht permanent: `macDirStat` verwendet den macOS-Papierkorb.
- Der Startordner selbst kann nicht gelöscht werden.
- Symlinks werden weder beim Scan noch bei der Löschvalidierung verfolgt.
- Permission- und andere I/O-Fehler stoppen den übrigen Scan nicht.
- Geschützte Bereiche können Festplattenvollzugriff für das verwendete Terminal
  benötigen. `sudo` ist nicht erforderlich und wird nicht automatisch verwendet.
- Dateinamen müssen kein valides UTF-8 sein; Cachepfade werden verlustfrei abgelegt.

## Entwicklung

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

## Release erstellen

Der Workflow [`.github/workflows/release.yml`](.github/workflows/release.yml)
erstellt bei jedem gepushten Versionstag automatisch einen GitHub Release mit
nativen macOS-Binaries und SHA-256-Prüfsummen. Der Tag muss dem Format
`vMAJOR.MINOR.PATCH` entsprechen und exakt zur führenden Version in [`VERSION`](VERSION)
passen.

Für eine neue Version zuerst `VERSION` und den von Cargo benötigten Spiegel in
`Cargo.toml` ändern. Das Build-Skript verhindert Builds mit abweichenden Werten.
Anschließend `Cargo.lock` mit einem normalen Cargo-Aufruf aktualisieren und die
Prüfungen ausführen.
