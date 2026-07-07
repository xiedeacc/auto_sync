#ifndef RUNNER_FLUTTER_WINDOW_H_
#define RUNNER_FLUTTER_WINDOW_H_

#include <flutter/dart_project.h>
#include <flutter/flutter_view_controller.h>
#include <wrl/client.h>

#include <memory>
#include <string>

#include "win32_window.h"

struct ICoreWebView2;
struct ICoreWebView2Controller;

// A window that does nothing but host a Flutter view.
class FlutterWindow : public Win32Window {
 public:
  // Creates a new FlutterWindow hosting a Flutter view running |project|.
  FlutterWindow(const flutter::DartProject& project, std::wstring target_url,
                std::wstring config_path);
  virtual ~FlutterWindow();

 protected:
  // Win32Window:
  bool OnCreate() override;
  void OnDestroy() override;
  LRESULT MessageHandler(HWND window, UINT const message, WPARAM const wparam,
                         LPARAM const lparam) noexcept override;

 private:
  void CreateWebView();
  void ResizeWebView();
  void NavigateToTarget();
  void AddTrayIcon();
  void RemoveTrayIcon();
  void ShowFromTray();
  void QuitFromTray();
  bool ShouldCloseToTray() const;

  // The project to run.
  flutter::DartProject project_;
  std::wstring target_url_;
  std::wstring config_path_;
  bool tray_added_ = false;
  bool quit_requested_ = false;

  // The Flutter instance hosted by this window.
  std::unique_ptr<flutter::FlutterViewController> flutter_controller_;
  Microsoft::WRL::ComPtr<ICoreWebView2Controller> webview_controller_;
  Microsoft::WRL::ComPtr<ICoreWebView2> webview_;
};

#endif  // RUNNER_FLUTTER_WINDOW_H_
