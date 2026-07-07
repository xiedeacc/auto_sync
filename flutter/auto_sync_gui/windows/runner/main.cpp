#include <flutter/dart_project.h>
#include <flutter/flutter_view_controller.h>
#include <windows.h>

#include <chrono>
#include <fstream>
#include <regex>
#include <sstream>
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

std::string ReadPortFromConfig(const std::string& config_path) {
  if (config_path.empty()) {
    return "";
  }
  std::ifstream file(config_path);
  if (!file) {
    return "";
  }
  std::regex port_pattern(R"(^\s*port\s*=\s*([0-9]+)\s*(#.*)?$)");
  std::string line;
  std::smatch match;
  while (std::getline(file, line)) {
    if (std::regex_match(line, match, port_pattern)) {
      return match[1].str();
    }
  }
  return "";
}

std::wstring ResolveTargetUrl(const std::vector<std::string>& args) {
  std::string url = ArgValue(args, "--url");
  if (url.empty()) {
    std::string port = ArgValue(args, "--port");
    if (port.empty()) {
      port = ReadPortFromConfig(ArgValue(args, "--config"));
    }
    if (port.empty()) {
      port = "18765";
    }
    url = "http://127.0.0.1:" + port + "/";
  }
  const auto stamp = std::chrono::duration_cast<std::chrono::milliseconds>(
                         std::chrono::system_clock::now().time_since_epoch())
                         .count();
  url += (url.find('?') == std::string::npos ? "?" : "&");
  url += "b=" + std::to_string(stamp);
  return Utf8ToWide(url);
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
  std::wstring target_url = ResolveTargetUrl(command_line_arguments);
  std::wstring config_path = Utf8ToWide(ArgValue(command_line_arguments, "--config"));

  project.set_dart_entrypoint_arguments(std::move(command_line_arguments));

  FlutterWindow window(project, target_url, config_path);
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
