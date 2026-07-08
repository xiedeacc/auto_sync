String defaultApiBaseUrl(String? port) => 'http://127.0.0.1:${port ?? "18765"}';

String? readPortFromConfig(String configPath) => null;

bool get hostPlatformIsWindows => false;

String get hostPathSeparator => '/';

bool localPathIsDirectory(String path) => false;
