#include "flutter_window.h"

#include <shellapi.h>
#include <windows.h>

#include <fstream>
#include <optional>
#include <regex>
#include <utility>

#include "flutter/generated_plugin_registrant.h"
#include "resource.h"

namespace {
constexpr UINT kTrayMessage = WM_APP + 1;
constexpr UINT kTrayId = 1;
constexpr UINT kMenuShow = 1001;
constexpr UINT kMenuQuit = 1002;
}

FlutterWindow::FlutterWindow(const flutter::DartProject& project,
                             std::wstring config_path)
    : project_(project),
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
  });

  // Flutter can complete the first frame before the "show window" callback is
  // registered. The following call ensures a frame is pending to ensure the
  // window is shown. It is a no-op if the first frame hasn't completed yet.
  flutter_controller_->ForceRedraw();

  return true;
}

void FlutterWindow::OnDestroy() {
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
    case WM_FONTCHANGE:
      flutter_controller_->engine()->ReloadSystemFonts();
      break;
  }

  return Win32Window::MessageHandler(hwnd, message, wparam, lparam);
}
