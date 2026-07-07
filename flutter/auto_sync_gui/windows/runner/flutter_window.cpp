#include "flutter_window.h"

#include <WebView2.h>
#include <shlobj.h>
#include <shellapi.h>
#include <windows.h>
#include <wrl.h>

#include <cstdlib>
#include <fstream>
#include <optional>
#include <regex>
#include <utility>

#include "flutter/generated_plugin_registrant.h"
#include "resource.h"

namespace {
constexpr UINT_PTR kRetryNavigateTimer = 18765;
constexpr UINT kTrayMessage = WM_APP + 1;
constexpr UINT kTrayId = 1;
constexpr UINT kMenuShow = 1001;
constexpr UINT kMenuQuit = 1002;
}

FlutterWindow::FlutterWindow(const flutter::DartProject& project,
                             std::wstring target_url,
                             std::wstring config_path)
    : project_(project),
      target_url_(std::move(target_url)),
      config_path_(std::move(config_path)) {}

FlutterWindow::~FlutterWindow() {}

bool FlutterWindow::OnCreate() {
  if (!Win32Window::OnCreate()) {
    return false;
  }

  RECT frame = GetClientArea();

  // The size here must match the window dimensions to avoid unnecessary surface
  // creation / destruction in the startup path.
  flutter_controller_ = std::make_unique<flutter::FlutterViewController>(
      frame.right - frame.left, frame.bottom - frame.top, project_);
  // Ensure that basic setup of the controller was successful.
  if (!flutter_controller_->engine() || !flutter_controller_->view()) {
    return false;
  }
  RegisterPlugins(flutter_controller_->engine());
  SetChildContent(flutter_controller_->view()->GetNativeWindow());

  flutter_controller_->engine()->SetNextFrameCallback([&]() {
    this->Show();
    AddTrayIcon();
    CreateWebView();
  });

  // Flutter can complete the first frame before the "show window" callback is
  // registered. The following call ensures a frame is pending to ensure the
  // window is shown. It is a no-op if the first frame hasn't completed yet.
  flutter_controller_->ForceRedraw();

  return true;
}

void FlutterWindow::OnDestroy() {
  KillTimer(GetHandle(), kRetryNavigateTimer);
  webview_ = nullptr;
  webview_controller_ = nullptr;
  if (flutter_controller_) {
    flutter_controller_ = nullptr;
  }
  RemoveTrayIcon();

  Win32Window::OnDestroy();
}

void FlutterWindow::AddTrayIcon() {
  if (tray_added_) {
    return;
  }
  NOTIFYICONDATA nid{};
  nid.cbSize = sizeof(nid);
  nid.hWnd = GetHandle();
  nid.uID = kTrayId;
  nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
  nid.uCallbackMessage = kTrayMessage;
  nid.hIcon = LoadIcon(GetModuleHandle(nullptr), MAKEINTRESOURCE(IDI_APP_ICON));
  wcscpy_s(nid.szTip, L"auto_sync");
  tray_added_ = Shell_NotifyIcon(NIM_ADD, &nid) == TRUE;
}

void FlutterWindow::RemoveTrayIcon() {
  if (!tray_added_) {
    return;
  }
  NOTIFYICONDATA nid{};
  nid.cbSize = sizeof(nid);
  nid.hWnd = GetHandle();
  nid.uID = kTrayId;
  Shell_NotifyIcon(NIM_DELETE, &nid);
  tray_added_ = false;
}

void FlutterWindow::ShowFromTray() {
  ShowWindow(GetHandle(), SW_SHOWNORMAL);
  SetForegroundWindow(GetHandle());
}

void FlutterWindow::QuitFromTray() {
  quit_requested_ = true;
  DestroyWindow(GetHandle());
}

bool FlutterWindow::ShouldCloseToTray() const {
  if (config_path_.empty()) {
    return true;
  }
  std::ifstream file(config_path_);
  if (!file) {
    return true;
  }
  std::regex close_pattern(R"(^\s*close_to_tray\s*=\s*(true|false)\s*(#.*)?$)",
                           std::regex::icase);
  std::string line;
  std::smatch match;
  while (std::getline(file, line)) {
    if (std::regex_match(line, match, close_pattern)) {
      std::string value = match[1].str();
      return value != "false" && value != "False" && value != "FALSE";
    }
  }
  return true;
}

void FlutterWindow::CreateWebView() {
  if (webview_controller_) {
    return;
  }

  std::wstring user_data_folder;
  wchar_t* local_app_data = nullptr;
  size_t local_app_data_len = 0;
  if (_wdupenv_s(&local_app_data, &local_app_data_len, L"LOCALAPPDATA") == 0 &&
      local_app_data != nullptr) {
    user_data_folder = std::wstring(local_app_data) + L"\\auto_sync\\webview2";
    free(local_app_data);
    SHCreateDirectoryExW(nullptr, user_data_folder.c_str(), nullptr);
  }

  HRESULT hr = CreateCoreWebView2EnvironmentWithOptions(
      nullptr, user_data_folder.empty() ? nullptr : user_data_folder.c_str(),
      nullptr,
      Microsoft::WRL::Callback<ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler>(
          [this](HRESULT result, ICoreWebView2Environment* environment)
              -> HRESULT {
            if (FAILED(result) || environment == nullptr) {
              SetTimer(GetHandle(), kRetryNavigateTimer, 2000, nullptr);
              return S_OK;
            }
            environment->CreateCoreWebView2Controller(
                GetHandle(),
                Microsoft::WRL::Callback<
                    ICoreWebView2CreateCoreWebView2ControllerCompletedHandler>(
                    [this](HRESULT result,
                           ICoreWebView2Controller* controller) -> HRESULT {
                      if (FAILED(result) || controller == nullptr) {
                        SetTimer(GetHandle(), kRetryNavigateTimer, 2000,
                                 nullptr);
                        return S_OK;
                      }
                      webview_controller_ = controller;
                      webview_controller_->get_CoreWebView2(&webview_);
                      if (flutter_controller_ && flutter_controller_->view()) {
                        ShowWindow(flutter_controller_->view()->GetNativeWindow(), SW_HIDE);
                      }
                      ResizeWebView();
                      if (webview_) {
                        EventRegistrationToken token = {};
                        webview_->add_NavigationCompleted(
                            Microsoft::WRL::Callback<
                                ICoreWebView2NavigationCompletedEventHandler>(
                                [this](ICoreWebView2* sender,
                                       ICoreWebView2NavigationCompletedEventArgs*
                                           args) -> HRESULT {
                                  BOOL success = FALSE;
                                  if (args) {
                                    args->get_IsSuccess(&success);
                                  }
                                  if (!success) {
                                    SetTimer(GetHandle(), kRetryNavigateTimer,
                                             2000, nullptr);
                                  }
                                  return S_OK;
                                })
                                .Get(),
                            &token);
                      }
                      NavigateToTarget();
                      return S_OK;
                    })
                    .Get());
            return S_OK;
          })
          .Get());
  if (FAILED(hr)) {
    SetTimer(GetHandle(), kRetryNavigateTimer, 2000, nullptr);
  }
}

void FlutterWindow::ResizeWebView() {
  if (!webview_controller_) {
    return;
  }
  RECT bounds = GetClientArea();
  webview_controller_->put_Bounds(bounds);
}

void FlutterWindow::NavigateToTarget() {
  if (!webview_) {
    CreateWebView();
    return;
  }
  KillTimer(GetHandle(), kRetryNavigateTimer);
  webview_->Navigate(target_url_.c_str());
}

LRESULT
FlutterWindow::MessageHandler(HWND hwnd, UINT const message,
                              WPARAM const wparam,
                              LPARAM const lparam) noexcept {
  // Give Flutter, including plugins, an opportunity to handle window messages.
  if (flutter_controller_) {
    std::optional<LRESULT> result =
        flutter_controller_->HandleTopLevelWindowProc(hwnd, message, wparam,
                                                      lparam);
    if (result) {
      return *result;
    }
  }

  switch (message) {
    case WM_CLOSE:
      if (!quit_requested_ && ShouldCloseToTray()) {
        ShowWindow(hwnd, SW_HIDE);
        return 0;
      }
      break;
    case WM_COMMAND:
      switch (LOWORD(wparam)) {
        case kMenuShow:
          ShowFromTray();
          return 0;
        case kMenuQuit:
          QuitFromTray();
          return 0;
      }
      break;
    case kTrayMessage:
      if (lparam == WM_LBUTTONUP || lparam == WM_LBUTTONDBLCLK) {
        ShowFromTray();
        return 0;
      }
      if (lparam == WM_RBUTTONUP || lparam == WM_CONTEXTMENU) {
        POINT point;
        GetCursorPos(&point);
        HMENU menu = CreatePopupMenu();
        AppendMenu(menu, MF_STRING, kMenuShow, L"Show auto_sync");
        AppendMenu(menu, MF_STRING, kMenuQuit, L"Quit");
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd,
                       nullptr);
        DestroyMenu(menu);
        return 0;
      }
      break;
    case WM_GETMINMAXINFO: {
      auto info = reinterpret_cast<MINMAXINFO*>(lparam);
      info->ptMinTrackSize.x = 860;
      info->ptMinTrackSize.y = 620;
      return 0;
    }
    case WM_SIZE:
      ResizeWebView();
      break;
    case WM_TIMER:
      if (wparam == kRetryNavigateTimer) {
        NavigateToTarget();
        return 0;
      }
      break;
    case WM_FONTCHANGE:
      flutter_controller_->engine()->ReloadSystemFonts();
      break;
  }

  return Win32Window::MessageHandler(hwnd, message, wparam, lparam);
}
