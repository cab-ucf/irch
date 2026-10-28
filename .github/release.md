## Install

Each of these keeps itself up to date.

| | |
|---|---|
| **Windows** | In PowerShell: `irm https://github.com/cab-ucf/irch/releases/latest/download/install.ps1 \| iex` |
| **macOS, Linux** | In a terminal: `curl -fsSL https://github.com/cab-ucf/irch/releases/latest/download/install.sh \| sh` |
| **NixOS** | `programs.irch.enable = true;` with [irch's module](https://github.com/cab-ucf/irch#nixos) |
| **Android** | On the phone, open [irch.apk](https://github.com/cab-ucf/irch/releases/latest/download/irch.apk) and allow your browser to install it, once. Or add `https://github.com/cab-ucf/irch` to [Obtainium](https://obtainium.imranr.dev). |
| **iPhone** | Install [irch.ipa](https://github.com/cab-ucf/irch/releases/latest/download/irch.ipa) with [AltStore](https://altstore.io) or [SideStore](https://sidestore.io), which re-sign it weekly; new versions install the same way. |

**An always-on storage node**, so your spaces wait for you while everyone is offline:
- In the app: a space's menu → Storage Nodes → *Run one on this computer*.
- From a terminal: add `-s -- --server` after `sh` above, or on Windows run `& ([scriptblock]::Create((irm https://github.com/cab-ucf/irch/releases/latest/download/install.ps1))) -Server`.
- On NixOS: `services.irch.enable = true;`.

The installers fetch the files below. The `.dmg` and `.msi` work too, but a browser marks them as downloaded from the internet. macOS then asks you to allow irch in System Settings → Privacy & Security, and Windows asks you to choose *More info → Run anyway*. `irch-<platform>` is the terminal client and storage node; `.sig` files are irch's signatures, checked by every update.
