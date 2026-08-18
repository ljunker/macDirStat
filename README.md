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
