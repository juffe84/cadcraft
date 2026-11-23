# Customizing and automating CADCraft

What you can use today to automate CADCraft or bring over add-ons written for other CAD programs,
and what is not there yet.

| You have | In CADCraft | More |
|---|---|---|
| LISP routines (`.lsp`) | Run with APPLOAD or `(load "name")`. The core language, `command`, entity lists, selection sets, `get…` input and `c:` commands work. | [autolisp.md](autolisp.md) |
| Startup LISP (`acad.lsp`, `acaddoc.lsp`) | Loaded from the support folder at startup and for each drawing. | [autolisp.md](autolisp.md#at-startup) |
| Scripts (`.scr`) | SCRIPT (Tools > Run Script...), `cadcraft-cli run --script-file`, and script text over MCP and the control channel. | below |
| Command aliases (`.pgp`) | ALIASEDIT `{file}` loads an alias file and keeps it; the aliases and the file survive restarts. REINIT reads it again. | below |
| Compiled plug-ins (C++, .NET) | Not supported. A plug-in API is planned, for plug-ins written in Rust and compiled to WebAssembly. | [gaps.md](gaps.md) (E4) |
| Programs that drive the CAD program from outside | The JSON control channel, the MCP server and `cadcraft-cli`. Any language that can open a TCP socket or start a process can use them. | below |

## LISP routines

Most routines that only draw, edit and query objects run as they are. Not yet supported: Visual
LISP ActiveX functions (`vlax-…`, `vla-…`), DCL dialogs and reactors ([gaps.md](gaps.md) E1). A
routine that uses them needs those parts rewritten, for example with `command` calls and `get…`
prompts instead of a dialog.

Put routines you want in every session in the support folder (`support` in the settings folder;
paths in [autolisp.md](autolisp.md#at-startup)) and load them from `acad.lsp` or `acaddoc.lsp`, or
with `(autoload "file" '("CMD"))` so they load the first time their command is typed.

## Scripts

A `.scr` file is what you would type at the command line, one line at a time. Run it with SCRIPT
in the app, or headless:

```sh
cadcraft-cli run drawing.dxf --script-file setup.scr --save out.dxf
```

## Command aliases

ALIASEDIT reads a `.pgp` alias file (`ALIAS, *COMMAND` lines); its aliases replace the ones you
had. Lines that would start external programs are skipped. From a script or the control channel:
`aliasedit {"file": "/path/to/aliases.pgp"}`. The Command Aliases dialog's **Import…** button picks
a `.pgp` file, loads it the same way, shows its path and warnings, and remembers the folder it came
from; on the web (no native file dialog) type `ALIASEDIT` with the file's contents in its `text`
parameter instead.

Aliases are user settings, never part of a drawing. The desktop and web apps keep them, and the
file REINIT reads, with your other preferences, so aliases added, edited, imported or removed in
the Command Aliases dialog (or with ALIASEDIT's `set`, `remove` and `clear`) are there after a
restart, and REINIT reads the same file again. The headless CLI keeps no preferences: there,
aliases last for that run.

## Driving CADCraft from another program

Every command has a JSON form that runs without dialogs. `cadcraft-cli commands [FILTER]` prints
the catalog: each command's id and its parameters.

- **Headless batch jobs:** `cadcraft-cli run FILE --cmd 'id {json}' --save OUT --export OUT.png`.
  A `--cmd` parameter value of exactly `{last}` is replaced with the last object handle an earlier
  `--cmd` in the same run returned, so `viewport.set {"handle":"{last}","scale":1}` can follow
  `mview`. Every other value is passed unchanged.
- **The running app:** start it with `--control PORT` and send JSON lines to `127.0.0.1:PORT`.
  See [control-protocol.md](control-protocol.md).

  ```sh
  printf '%s\n' '{"id":1,"method":"cmdline.input","params":{"text":"circle 0,0 5"}}' | nc -w 2 127.0.0.1 7979
  ```

- **AI agents:** `cadcraft-cli mcp`, headless or connected to the app. See [mcp.md](mcp.md).

## LAS and LAZ point clouds

Use **Open** or drop a `.las` or `.laz` file into CADCraft. The file opens in its own tab with
the original coordinates and scan colors, when present. CADCraft does not reproject coordinates.

On desktop, clouds with more than 25 million points first show a sparse preview. CADCraft
indexes the source blocks in the background. When you zoom in, it decompresses overlapping
blocks in parallel, shows additional points as batches finish, and reuses recent views while
you move the camera. The native GPU renderer keeps point buffers resident while panning,
zooming or orbiting. The source stays on disk, and the preview remains bounded in memory. The desktop accepts up to one billion input points. The browser
accepts up to two million and keeps them in memory.

Use **Point size** to change how large the points appear, and **Snap to cloud points** to attach
a drawing point to a visible sample in plan view. Object snaps must be enabled (F3). The drawing
uses the chosen point's XY; the command `pointcloud.snap` can report the original XYZ.

Center the area of interest in plan view, then select **3D Orbit** (or run `3DORBIT`) to inspect
elevations. Drag with the middle button to rotate around the point under the cursor, drag with
the right button to pan, and scroll to zoom toward the cursor. The focused view also rotates
with the arrow keys. Select **3D Orbit** again or press Escape with the view focused to return
to plan view. Drafting and snapping operate in plan view.

Use `cadcraft-cli info scan.laz` for the source point count and XY extents. The preview supports
viewing. Saving or exporting the cloud is not supported.
