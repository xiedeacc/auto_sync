# auto_sync_gui

Flutter Windows desktop shell for auto_sync.

The app intentionally reuses the existing `src/ui` HTML/CSS/JavaScript served by
the Rust `auto_sync` backend. The Windows runner hosts a WebView2 child window
without Flutter plugins, avoiding Windows symlink/Developer Mode requirements
while preserving the existing UI pixels and HTTP behavior.

```powershell
flutter pub get
flutter build windows --release
```
