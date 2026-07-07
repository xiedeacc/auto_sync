# auto_sync_gui

Native Flutter Windows desktop UI for auto_sync.

The app renders the desktop interface with Flutter widgets and talks directly to
the Rust `auto_sync` HTTP API. It does not embed a browser view or reuse the
HTML/CSS/JavaScript UI.

```powershell
flutter pub get
flutter build windows --release
```
