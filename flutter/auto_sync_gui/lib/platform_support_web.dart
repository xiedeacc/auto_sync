String defaultApiBaseUrl(String? port) {
  final origin = Uri.base.origin;
  if (origin != 'null' && origin.isNotEmpty) {
    return origin;
  }
  return 'http://127.0.0.1:${port ?? "18765"}';
}

String? readPortFromConfig(String configPath) => null;

bool get hostPlatformIsWindows => false;

String get hostPathSeparator => '/';

bool localPathIsDirectory(String path) => false;
