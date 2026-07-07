#include <flutter/dart_project.h>
#include <flutter/flutter_view_controller.h>
#include <windows.h>

#include <string>
#include <vector>

#include "flutter_window.h"
#include "utils.h"

namespace {

std::wstring Utf8ToWide(const std::string& value) {
  if (value.empty()) {
    return L"";
  }
  int size = MultiByteToWideChar(CP_UTF8, 0, value.data(),
                                 static_cast<int>(value.size()), nullptr, 0);
  std::wstring wide(size, L'\0');
  MultiByteToWideChar(CP_UTF8, 0, value.data(), static_cast<int>(value.size()),
                      wide.data(), size);
  return wide;
}

std::string ArgValue(const std::vector<std::string>& args,
                     const std::string& name) {
  const std::string prefix = name + "=";
  for (size_t i = 0; i < args.size(); ++i) {
    if (args[i].rfind(prefix, 0) == 0) {
      return args[i].substr(prefix.size());
    }
    if (args[i] == name && i + 1 < args.size()) {
      return args[i + 1];
    }
  }
  return "";
}

}  // namespace

int APIENTRY wWinMain(_In_ HINSTANCE instance, _In_opt_ HINSTANCE prev,
                      _In_ wchar_t *command_line, _In_ int show_command) {
  // Attach to console when present (e.g., 'flutter run') or create a
  // new console when running with a debugger.
  if (!::AttachConsole(ATTACH_PARENT_PROCESS) && ::IsDebuggerPresent()) {
    CreateAndAttachConsole();
  }

  // Initialize COM, so that it is available for use in the library and/or
  // plugins.
  ::CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);

  flutter::DartProject project(L"data");

  std::vector<std::string> command_line_arguments =
      GetCommandLineArguments();
  std::wstring config_path = Utf8ToWide(ArgValue(command_line_arguments, "--config"));

  project.set_dart_entrypoint_arguments(std::move(command_line_arguments));

  FlutterWindow window(project, config_path);
  Win32Window::Point origin(10, 10);
  Win32Window::Size size(1180, 1000);
  if (!window.Create(L"auto_sync", origin, size)) {
    return EXIT_FAILURE;
  }
  window.SetQuitOnClose(true);

  ::MSG msg;
  while (::GetMessage(&msg, nullptr, 0, 0)) {
    ::TranslateMessage(&msg);
    ::DispatchMessage(&msg);
  }

  ::CoUninitialize();
  return EXIT_SUCCESS;
}
