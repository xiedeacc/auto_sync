import 'dart:io';

String defaultApiBaseUrl(String? port) => 'http://127.0.0.1:${port ?? "18765"}';

String? readPortFromConfig(String configPath) {
  try {
    final text = File(configPath).readAsStringSync();
    final match = RegExp(
      r'^\s*port\s*=\s*([0-9]+)\s*(#.*)?$',
      multiLine: true,
    ).firstMatch(text);
    return match?.group(1);
  } catch (_) {
    return null;
  }
}

bool get hostPlatformIsWindows => Platform.isWindows;

String get hostPathSeparator => Platform.pathSeparator;

bool localPathIsDirectory(String path) {
  try {
    return FileSystemEntity.typeSync(path) == FileSystemEntityType.directory;
  } catch (_) {
    return false;
  }
}
