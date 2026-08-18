# macDirStat

`macDirStat` ist eine schnelle Terminaloberfläche zum Auffinden großer Dateien und
Verzeichnisse auf macOS. Ordnergrößen werden parallel im Hintergrund berechnet,
während die Oberfläche bedienbar bleibt.

## Installation

Voraussetzung ist eine aktuelle Rust-Toolchain.

```bash
cargo install --path . --locked
```

Danach ist das Programm unter seinem exakten Namen aufrufbar:

```bash
macDirStat
macDirStat ~/Library
macDirStat --workers 4 ~/Library
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
tar -xzf macDirStat-v0.1.0-macos-arm64.tar.gz
mkdir -p ~/.local/bin
install -m 755 macDirStat-v0.1.0-macos-arm64/macDirStat ~/.local/bin/macDirStat
```

## Bedienung

| Taste | Aktion |
|---|---|
| `↑` / `↓` | Auswahl bewegen |
| `→` / `Enter` | Ordner öffnen |
| `←` | Ordner schließen beziehungsweise zum Elternordner springen |
| `Home` / `End` | Zum ersten beziehungsweise letzten Eintrag springen |
| `g` | Direkt zum ersten sichtbaren Eintrag springen |
| `PgUp` / `PgDn` | Seitenweise scrollen |
| `Space` | Eintrag für eine Mehrfachaktion markieren oder entmarkieren |
| `x` | Alle markierten Einträge gemeinsam in den Papierkorb verschieben |
| `d` | Ausgewählten Eintrag nach Bestätigung in den Papierkorb verschieben |
| `r` | Den vollständigen Startordner neu scannen |
| `q` / `Ctrl+C` | Beenden |

Eine einzelne oder mehrfache Löschung wird ausschließlich mit `y` bestätigt. `n`
und `Esc` brechen den Dialog ab. Wird ein Ordner markiert, entfernt `macDirStat`
bereits vorhandene Markierungen seiner Kinder, damit kein Pfad doppelt gelöscht
wird. Alle Pfade einer Mehrfachauswahl werden erneut validiert, bevor der erste
Eintrag in den Papierkorb verschoben wird. Nach einer erfolgreichen Löschung
bleiben der übrige Baum und seine Scanresultate erhalten; neu berechnet werden nur
die Größen der übergeordneten Ordner bis zum Startordner.

## Verhalten und Sicherheit

- Größen sind logische Dateigrößen und werden in IEC-Einheiten angezeigt.
- Symlinks werden angezeigt, aber niemals verfolgt. Ihr Ziel wird nicht gescannt.
- Gelöscht wird nicht permanent: `macDirStat` verwendet den macOS-Papierkorb.
- Der Startordner selbst kann nicht gelöscht werden.
- Permission- und andere I/O-Fehler stoppen den übrigen Scan nicht.
- Geschützte Bereiche wie Teile von `~/Library` können in den macOS-Systemeinstellungen
  Festplattenvollzugriff für das verwendete Terminal benötigen. `sudo` ist nicht
  erforderlich und wird nicht automatisch verwendet.
- Versteckte Dateien werden im MVP angezeigt.

## Entwicklung

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## Release erstellen

Der Workflow [`.github/workflows/release.yml`](.github/workflows/release.yml)
erstellt bei jedem gepushten Versionstag automatisch einen GitHub Release mit
nativen macOS-Binaries und SHA-256-Prüfsummen. Der Tag muss dem Format
`vMAJOR.MINOR.PATCH` entsprechen und exakt zur Version in `Cargo.toml` passen.

Beispiel für Version `0.1.0`:

```bash
git tag -a v0.1.0 -m "macDirStat v0.1.0"
git push origin main
git push origin v0.1.0
```
