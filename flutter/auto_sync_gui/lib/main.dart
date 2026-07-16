import 'dart:async';
import 'dart:convert';
import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;

import 'platform_support.dart';

void main(List<String> args) {
  runApp(AutoSyncNativeApp(api: AutoSyncApi.fromArgs(args)));
}

class AutoSyncApi {
  AutoSyncApi(this.baseUrl);

  final String baseUrl;

  factory AutoSyncApi.fromArgs(List<String> args) {
    String? valueOf(String name) {
      final prefix = '$name=';
      for (var i = 0; i < args.length; i += 1) {
        final arg = args[i];
        if (arg.startsWith(prefix)) {
          return arg.substring(prefix.length);
        }
        if (arg == name && i + 1 < args.length) {
          return args[i + 1];
        }
      }
      return null;
    }

    final directUrl = valueOf('--url');
    if (directUrl != null && directUrl.isNotEmpty) {
      return AutoSyncApi(_trimSlash(directUrl));
    }
    var port = valueOf('--port');
    final configPath = valueOf('--config');
    if ((port == null || port.isEmpty) && configPath != null) {
      port = readPortFromConfig(configPath);
    }
    return AutoSyncApi(_trimSlash(defaultApiBaseUrl(port)));
  }

  static String _trimSlash(String value) =>
      value.endsWith('/') ? value.substring(0, value.length - 1) : value;

  Future<dynamic> _request(
    String method,
    String path, {
    Map<String, String>? query,
    Object? body,
  }) async {
    final uri = Uri.parse('$baseUrl$path').replace(queryParameters: query);
    final headers = <String, String>{};
    Object? requestBody;
    if (body != null) {
      headers['Content-Type'] = 'application/json';
      requestBody = jsonEncode(body);
    }
    late final http.Response response;
    switch (method) {
      case 'GET':
        response = await http.get(uri, headers: headers);
      case 'POST':
        response = await http.post(uri, headers: headers, body: requestBody);
      case 'DELETE':
        response = await http.delete(uri, headers: headers, body: requestBody);
      default:
        throw ArgumentError('Unsupported method: $method');
    }
    if (response.statusCode < 200 || response.statusCode >= 300) {
      throw Exception(
        response.body.isEmpty ? 'HTTP ${response.statusCode}' : response.body,
      );
    }
    if (response.body.trim().isEmpty) {
      return null;
    }
    return jsonDecode(response.body);
  }

  Future<String> text(String path) async {
    final uri = Uri.parse('$baseUrl$path');
    final response = await http.get(uri);
    if (response.statusCode < 200 || response.statusCode >= 300) {
      throw Exception(
        response.body.isEmpty ? 'HTTP ${response.statusCode}' : response.body,
      );
    }
    return response.body;
  }

  Future<Map<String, dynamic>> getConfig() async =>
      _map(await _request('GET', '/api/config'));
  Future<void> saveConfig(Map<String, dynamic> cfg) async =>
      _request('POST', '/api/config', body: cfg);
  Future<List<dynamic>> getStatus() async =>
      _list(await _request('GET', '/api/status'));
  Future<Map<String, dynamic>> getRuntimeStatus() async =>
      _map(await _request('GET', '/api/runtime-status'));
  Future<Map<String, dynamic>> getSyncActivity() async =>
      _map(await _request('GET', '/api/sync-activity'));
  Future<Map<String, dynamic>> getMachines({bool discover = false}) async =>
      _map(
        await _request(
          'GET',
          discover ? '/api/machines/discover' : '/api/machines',
        ),
      );
  Future<void> addMachine(Map<String, dynamic> machine) async =>
      _request('POST', '/api/machines', body: machine);
  Future<void> removeMachine(String id) async =>
      _request('DELETE', '/api/machines/${Uri.encodeComponent(id)}');
  Future<void> syncAll() async => _request('POST', '/api/sync-now');
  Future<void> syncSource(String sourceId) async =>
      _request('POST', '/api/sync-source-now', body: {'source_id': sourceId});
  Future<void> syncDestination(
    String sourceId,
    String destinationId,
    String mode,
  ) async => _request(
    'POST',
    '/api/sync-destination-now',
    body: {
      'source_id': sourceId,
      'destination_id': destinationId,
      'mode': mode,
    },
  );
  Future<void> scanDestination(String sourceId, String destinationId) async =>
      _request(
        'POST',
        '/api/scan-destination-now',
        body: {'source_id': sourceId, 'destination_id': destinationId},
      );
  Future<Map<String, dynamic>> scanReport(
    String sourceId,
    String destinationId,
  ) async => _map(
    await _request(
      'GET',
      '/api/scan-report',
      query: {'source_id': sourceId, 'destination_id': destinationId},
    ),
  );
  Future<Map<String, dynamic>> browsePaths({
    required String path,
    required String machineId,
  }) async => _map(
    await _request(
      'GET',
      '/api/browse-paths',
      query: {'path': path, 'machine_id': machineId},
    ),
  );
  Future<void> cancelActivity({
    String? scope,
    String? sourceId,
    String? destinationId,
  }) async => _request(
    'POST',
    '/api/cancel-activity',
    body: {
      'scope': scope,
      'source_id': sourceId,
      'destination_id': destinationId,
      'propagate': true,
    },
  );
  Future<List<dynamic>> getAllTasks({int limit = 100}) async => _list(
    await _request('GET', '/api/all-tasks', query: {'limit': '$limit'}),
  );
  Future<Map<String, dynamic>> collectorConfig() async =>
      _map(await _request('GET', '/api/collector/config'));
  Future<Map<String, dynamic>> saveCollectorConfig(
    Map<String, dynamic> cfg,
  ) async => _map(await _request('POST', '/api/collector/config', body: cfg));
  Future<Map<String, dynamic>> collectorStatus({int? index}) async => _map(
    await _request(
      'GET',
      '/api/collector/status',
      query: index == null ? null : {'index': '$index'},
    ),
  );
  Future<Map<String, dynamic>> collectorRun() async =>
      _map(await _request('POST', '/api/collector/run'));
  Future<Map<String, dynamic>> collectorRunHost(int index) async => _map(
    await _request('POST', '/api/collector/run-host', body: {'index': index}),
  );
  Future<Map<String, dynamic>> collectorBrowse(
    Map<String, dynamic> req,
  ) async => _map(await _request('POST', '/api/collector/browse', body: req));
  Future<Map<String, dynamic>> collectorDeploy(int index) async => _map(
    await _request('POST', '/api/collector/deploy', body: {'index': index}),
  );
  Future<Map<String, dynamic>> collectorDeployScript(int index) async => _map(
    await _request(
      'GET',
      '/api/collector/deploy-script',
      query: {'index': '$index'},
    ),
  );
  Future<Map<String, dynamic>> collectorDeployStatus({int? index}) async =>
      _map(
        await _request(
          'GET',
          '/api/collector/deploy-status',
          query: index == null ? null : {'index': '$index'},
        ),
      );
  Future<Map<String, dynamic>> localFilePreview(String path) async => _map(
    await _request('GET', '/api/local-file/preview', query: {'path': path}),
  );
  Future<void> saveLocalFileText(String path, String text) async => _request(
    'POST',
    '/api/local-file/text',
    body: {'path': path, 'text': text},
  );
}

Map<String, dynamic> _map(dynamic value) =>
    value is Map ? Map<String, dynamic>.from(value) : <String, dynamic>{};

Map<String, dynamic> _mapRef(dynamic value) =>
    value is Map ? value.cast<String, dynamic>() : <String, dynamic>{};

List<Map<String, dynamic>> _mapRefs(dynamic value) => _list(
  value,
).whereType<Map>().map((item) => item.cast<String, dynamic>()).toList();

List<dynamic> _list(dynamic value) => value is List ? value : <dynamic>[];

String _str(dynamic value, [String fallback = '']) =>
    value == null ? fallback : '$value';

List<String> _cleanStringList(Iterable<dynamic> values) {
  final seen = <String>{};
  final cleaned = <String>[];
  for (final value in values) {
    final text = _str(value).trim();
    if (text.isNotEmpty && seen.add(text)) {
      cleaned.add(text);
    }
  }
  cleaned.sort();
  return cleaned;
}

String _excludeButtonLabel(dynamic excludes) {
  final count = _cleanStringList(_list(excludes)).length;
  return count == 0 ? 'Excluded' : 'Excluded ($count)';
}

bool _bool(dynamic value, [bool fallback = false]) =>
    value is bool ? value : fallback;

String _normalizePath(String value) =>
    value.trim().replaceAll(RegExp(r'[\\/]+$'), '');

bool _isStartupConnectionMessage(String value) =>
    value.contains('SocketException') && value.contains('127.0.0.1:18765');

int _int(dynamic value, [int fallback = 0]) {
  if (value is int) {
    return value;
  }
  if (value is num) {
    return value.round();
  }
  return int.tryParse('$value') ?? fallback;
}

class AutoSyncNativeApp extends StatelessWidget {
  const AutoSyncNativeApp({super.key, required this.api, this.autoLoad = true});

  final AutoSyncApi api;
  final bool autoLoad;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      debugShowCheckedModeBanner: false,
      title: 'auto_sync',
      theme: ThemeData(
        useMaterial3: true,
        colorScheme: ColorScheme.fromSeed(
          seedColor: Palette.accent,
          surface: Palette.panel,
        ),
        scaffoldBackgroundColor: Palette.bg,
        fontFamily: 'Segoe UI',
        fontFamilyFallback: const [
          'Microsoft YaHei UI',
          'Microsoft YaHei',
          'Arial',
        ],
        textTheme: const TextTheme(
          bodyMedium: TextStyle(fontSize: 13, color: Palette.text),
          bodySmall: TextStyle(fontSize: 12, color: Palette.muted),
          titleMedium: TextStyle(
            fontSize: 16,
            fontWeight: FontWeight.w700,
            color: Palette.text,
          ),
        ),
        inputDecorationTheme: const InputDecorationTheme(
          isDense: true,
          filled: true,
          fillColor: Colors.white,
          border: OutlineInputBorder(
            borderRadius: BorderRadius.all(Radius.circular(6)),
            borderSide: BorderSide(color: Palette.line),
          ),
          enabledBorder: OutlineInputBorder(
            borderRadius: BorderRadius.all(Radius.circular(6)),
            borderSide: BorderSide(color: Palette.line),
          ),
          contentPadding: EdgeInsets.symmetric(horizontal: 9, vertical: 9),
        ),
      ),
      home: AutoSyncHome(api: api, autoLoad: autoLoad),
    );
  }
}

class Palette {
  static const bg = Color(0xfff6f7f9);
  static const panel = Color(0xffffffff);
  static const line = Color(0xffd9dee7);
  static const text = Color(0xff202733);
  static const muted = Color(0xff667085);
  static const accent = Color(0xff176b87);
  static const green = Color(0xff12805c);
  static const red = Color(0xffd92d20);
  static const warn = Color(0xffa15c07);
}

class AutoSyncHome extends StatefulWidget {
  const AutoSyncHome({super.key, required this.api, required this.autoLoad});

  final AutoSyncApi api;
  final bool autoLoad;

  @override
  State<AutoSyncHome> createState() => _AutoSyncHomeState();
}

class _AutoSyncHomeState extends State<AutoSyncHome> {
  Map<String, dynamic> cfg = {'app': {}, 'machines': [], 'source_groups': []};
  List<dynamic> statuses = [];
  List<dynamic> taskMachines = [];
  Map<String, dynamic> runtimeStatus = {};
  Map<String, dynamic> syncActivity = {};
  Map<String, dynamic> machineStatus = {};
  Map<String, dynamic> latestDestinationSchedule =
      _defaultDestinationSchedule();
  bool loading = true;
  bool busy = false;
  bool saving = false;
  bool configLoaded = false;
  String message = '';
  Timer? statusTimer;
  Timer? runtimeTimer;

  @override
  void initState() {
    super.initState();
    if (widget.autoLoad) {
      _loadAll();
      statusTimer = Timer.periodic(
        const Duration(seconds: 5),
        (_) => _loadStatusOnly(),
      );
      runtimeTimer = Timer.periodic(
        const Duration(seconds: 1),
        (_) => _loadRuntimeOnly(),
      );
    } else {
      loading = false;
    }
  }

  @override
  void dispose() {
    statusTimer?.cancel();
    runtimeTimer?.cancel();
    super.dispose();
  }

  Future<void> _loadAll() async {
    setState(() {
      loading = true;
      message = '';
    });
    final errors = <String>[];
    try {
      cfg = await widget.api.getConfig();
      configLoaded = true;
    } catch (error) {
      configLoaded = false;
      errors.add('$error');
    }
    try {
      statuses = await widget.api.getStatus();
    } catch (error) {
      errors.add('$error');
    }
    try {
      runtimeStatus = await widget.api.getRuntimeStatus();
    } catch (error) {
      errors.add('$error');
    }
    try {
      syncActivity = await widget.api.getSyncActivity();
    } catch (_) {}
    try {
      taskMachines = await widget.api.getAllTasks(limit: 80);
    } catch (_) {}
    try {
      machineStatus = await widget.api.getMachines();
    } catch (error) {
      errors.add('$error');
    }
    if (!mounted) {
      return;
    }
    setState(() {
      loading = false;
      message = errors.join(' | ');
    });
  }

  Future<void> _loadStatusOnly({bool force = false}) async {
    if (!mounted || (busy && !force)) {
      return;
    }
    if (!configLoaded) {
      await _loadAll();
      return;
    }
    try {
      final nextStatus = await widget.api.getStatus();
      final nextActivity = await widget.api.getSyncActivity();
      var nextTasks = taskMachines;
      try {
        nextTasks = await widget.api.getAllTasks(limit: 80);
      } catch (_) {}
      if (mounted) {
        setState(() {
          statuses = nextStatus;
          syncActivity = nextActivity;
          taskMachines = nextTasks;
          if (_isStartupConnectionMessage(message)) {
            message = '';
          }
        });
      }
    } catch (_) {}
  }

  Future<void> _loadRuntimeOnly() async {
    if (!mounted) {
      return;
    }
    try {
      final next = await widget.api.getRuntimeStatus();
      Map<String, dynamic>? nextActivity;
      try {
        nextActivity = await widget.api.getSyncActivity();
      } catch (_) {}
      if (mounted) {
        setState(() {
          runtimeStatus = next;
          if (nextActivity != null) {
            syncActivity = nextActivity;
          }
          if (_isStartupConnectionMessage(message)) {
            message = '';
          }
        });
      }
    } catch (_) {}
  }

  Future<void> _run(String label, Future<void> Function() action) async {
    if (busy) {
      return;
    }
    setState(() {
      busy = true;
      message = '$label...';
    });
    try {
      await action();
      await _loadStatusOnly(force: true);
      try {
        runtimeStatus = await widget.api.getRuntimeStatus();
      } catch (_) {}
      if (mounted) {
        final stillActive = _hasAnyLiveRuntimeActivity(
          syncActivity,
          runtimeStatus,
        );
        final isAsyncSync =
            label == 'Sync all' ||
            label.startsWith('Sync source ') ||
            label.startsWith('Sync ');
        setState(
          () => message = stillActive
              ? ''
              : isAsyncSync
              ? '$label requested'
              : '$label done',
        );
      }
    } catch (error) {
      if (mounted) {
        setState(() => message = '$label failed: $error');
      }
    } finally {
      if (mounted) {
        setState(() => busy = false);
      }
    }
  }

  Future<void> _saveConfig([String label = 'Saved']) async {
    if (saving) {
      return;
    }
    setState(() {
      saving = true;
      message = 'Saving config...';
    });
    try {
      await widget.api.saveConfig(cfg);
      final next = await widget.api.getConfig();
      if (mounted) {
        setState(() {
          cfg = next;
          configLoaded = true;
          message = label;
        });
      }
    } catch (error) {
      if (mounted) {
        setState(() => message = 'Save failed: $error');
      }
    } finally {
      if (mounted) {
        setState(() => saving = false);
      }
    }
  }

  Future<void> _saveSourceExcludes(String sourceId, List<String> excludes) {
    setState(() {
      for (final source in _mapRefs(cfg['source_groups'])) {
        if (_str(source['id']) == sourceId) {
          source['excludes'] = _cleanStringList(excludes);
          break;
        }
      }
    });
    return _saveConfig('Excludes saved');
  }

  List<Map<String, dynamic>> get sources =>
      _mapRefs(cfg['source_groups'])
        ..sort((a, b) => _int(a['order']).compareTo(_int(b['order'])));

  List<Map<String, dynamic>> get machines => _mapRefs(cfg['machines']);

  Map<String, dynamic> _app() {
    cfg['app'] = _mapRef(cfg['app']);
    return cfg['app'] as Map<String, dynamic>;
  }

  Map<String, dynamic>? _statusFor(String sourceId, String destinationId) {
    for (final item in statuses) {
      final status = _map(item);
      if (_str(status['source_id']) == sourceId &&
          _str(status['destination_id']) == destinationId) {
        return status;
      }
    }
    return null;
  }

  String _machineLabel(String id) {
    if (id.isEmpty || id == 'local') {
      return 'local';
    }
    for (final machine in machines) {
      if (_str(machine['id']) == id) {
        final alias = _str(machine['alias_name']);
        final name = _str(machine['name']);
        return alias.isNotEmpty ? alias : (name.isNotEmpty ? name : id);
      }
    }
    return id;
  }

  String _defaultPathForMachine(String machineId) {
    for (final machine in machines) {
      if (_str(machine['id']) == machineId) {
        return _str(machine['os']).toLowerCase() == 'windows' ? r'C:\' : '/';
      }
    }
    return machineId == 'local' && hostPlatformIsWindows ? r'C:\' : '/';
  }

  String _nextSourceId() {
    var maxId = 0;
    for (final source in sources) {
      final match = RegExp(r'^src_(\d+)$').firstMatch(_str(source['id']));
      if (match != null) {
        maxId = math.max(maxId, int.tryParse(match.group(1)!) ?? 0);
      }
    }
    return 'src_${maxId + 1}';
  }

  String _nextDestinationId(Map<String, dynamic> source) {
    var maxId = 0;
    for (final dst in _mapRefs(source['destinations'])) {
      final match = RegExp(r'^dst_(\d+)$').firstMatch(_str(dst['id']));
      if (match != null) {
        maxId = math.max(maxId, int.tryParse(match.group(1)!) ?? 0);
      }
    }
    return 'dst_${maxId + 1}';
  }

  Future<Map<String, dynamic>?> _pickPath({
    required String startPath,
    required String machineId,
    bool showAddDirectory = false,
    bool addDirectory = false,
    String? Function(Map<String, dynamic> selection)? validate,
  }) {
    return showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) => _PathPickerDialog(
        api: widget.api,
        machineIds: _machineIds(machineId),
        machineLabel: _machineLabel,
        initialMachineId: machineId.isEmpty ? 'local' : machineId,
        initialPath: startPath.isEmpty
            ? _defaultPathForMachine(machineId)
            : startPath,
        showAddDirectory: showAddDirectory,
        initialAddDirectory: addDirectory,
        validate: validate,
      ),
    );
  }

  Future<void> _pickSourcePath(Map<String, dynamic> source) async {
    if (_list(source['destinations']).isNotEmpty) {
      setState(
        () => message = _machinePath(
          _str(source['machine_id'], 'local'),
          _str(source['src']),
        ),
      );
      return;
    }
    final selected = await _pickPath(
      startPath: _str(
        source['src'],
        _defaultPathForMachine(_str(source['machine_id'], 'local')),
      ),
      machineId: _str(source['machine_id'], 'local'),
      showAddDirectory: true,
      addDirectory: _bool(source['add_directory']),
    );
    if (selected == null) return;
    setState(() {
      source['machine_id'] = _str(selected['machine_id'], 'local');
      source['src'] = _str(selected['path']);
      source['add_directory'] = _bool(selected['add_directory']);
    });
    await _saveConfig('Source path saved');
  }

  String? _destinationPathError(
    Map<String, dynamic> source,
    Map<String, dynamic> selection, [
    Map<String, dynamic>? ignoreDst,
  ]) {
    final path = _normalizePath(_str(selection['path']));
    final machineId = _str(selection['machine_id'], 'local');
    for (final dst in _mapRefs(source['destinations'])) {
      if (identical(dst, ignoreDst)) continue;
      if (_str(dst['machine_id'], 'local') == machineId &&
          _normalizePath(_str(dst['path'])) == path) {
        return 'Destination path already exists: $path';
      }
    }
    return null;
  }

  Future<void> _pickDestinationPath(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  ) async {
    final machineId = _str(
      destination['machine_id'],
      _str(source['machine_id'], 'local'),
    );
    final selected = await _pickPath(
      startPath: _str(destination['path'], _defaultPathForMachine(machineId)),
      machineId: machineId,
      validate: (selection) =>
          _destinationPathError(source, selection, destination),
    );
    if (selected == null) return;
    setState(() {
      destination['machine_id'] = _str(selected['machine_id'], 'local');
      destination['path'] = _str(selected['path']);
    });
    await _saveConfig('Destination path saved');
  }

  Future<void> _addDestination(Map<String, dynamic> source) async {
    final machineId = _str(source['machine_id'], 'local');
    final selected = await _pickPath(
      startPath: _defaultPathForMachine(machineId),
      machineId: machineId,
      validate: (selection) => _destinationPathError(source, selection),
    );
    if (selected == null) return;
    setState(() {
      final list = _list(source['destinations']);
      source['destinations'] = list;
      list.add({
        'id': _nextDestinationId(source),
        'machine_id': _str(selected['machine_id'], 'local'),
        'path': _str(selected['path']),
        'enabled': true,
        'schedule': _cloneSchedule(latestDestinationSchedule),
      });
    });
    await _saveConfig('Destination added');
  }

  Future<void> _openDestinationInfo(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  ) async {
    final sourceId = _str(source['id']);
    final destinationId = _str(destination['id']);
    Map<String, dynamic> report = {};
    Map<String, dynamic>? task;
    String error = '';
    try {
      report = await widget.api.scanReport(sourceId, destinationId);
    } catch (err) {
      error = '$err';
    }
    try {
      task = _newestTaskFor(
        await widget.api.getAllTasks(limit: 50),
        sourceId,
        destinationId,
      );
    } catch (_) {}
    if (!mounted) return;
    await showDialog<void>(
      context: context,
      builder: (context) => _DestinationInfoDialog(
        api: widget.api,
        source: source,
        destination: destination,
        status: _statusFor(sourceId, destinationId),
        runtimeStatus: runtimeStatus,
        syncActivity: syncActivity,
        task: task,
        scanReport: report,
        error: error,
      ),
    );
  }

  Future<void> _openDestinationSyncSettings(
    Map<String, dynamic> destination,
  ) async {
    final localSync = Map<String, dynamic>.from(_map(destination['sync']));
    final inherited = localSync.isEmpty;
    final sync = inherited
        ? Map<String, dynamic>.from(_map(_app()['sync']))
        : localSync;
    final result = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) =>
          _SyncSettingsDialog(sync: sync, inherited: inherited),
    );
    if (result != null) {
      setState(() {
        if (result.isEmpty) {
          destination.remove('sync');
        } else {
          destination['sync'] = result;
        }
      });
      await _saveConfig('Destination settings saved');
    }
  }

  Future<void> _openDestinationSchedule(
    Map<String, dynamic> destination,
  ) async {
    final result = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) => _ScheduleDialog(
        schedule: _cloneSchedule(_map(destination['schedule'])),
      ),
    );
    if (result == null) return;
    setState(() {
      final next = _cloneSchedule(result);
      destination['schedule'] = next;
      latestDestinationSchedule = _cloneSchedule(next);
    });
    await _saveConfig('Schedule saved');
  }

  String? _pathRelativeToSource(String sourcePath, String selectedPath) {
    final sourceNorm = _normalizePath(sourcePath).replaceAll('\\', '/');
    final selectedNorm = _normalizePath(selectedPath).replaceAll('\\', '/');
    if (selectedNorm == sourceNorm) return '';
    final prefix = sourceNorm.endsWith('/') ? sourceNorm : '$sourceNorm/';
    if (!selectedNorm.startsWith(prefix)) return null;
    return selectedNorm.substring(prefix.length);
  }

  Future<String?> _pickExcludePath(Map<String, dynamic> source) async {
    final sourcePath = _str(source['src']);
    if (sourcePath.isEmpty) {
      setState(() => message = 'Select source path first');
      return null;
    }
    final machineId = _str(source['machine_id'], 'local');
    final selected = await _pickPath(
      startPath: sourcePath,
      machineId: machineId,
    );
    if (selected == null) return null;
    if (_str(selected['machine_id'], 'local') != machineId) {
      setState(() => message = 'Excluded path must be on the source machine');
      return null;
    }
    final relative = _pathRelativeToSource(sourcePath, _str(selected['path']));
    if (relative == null) {
      setState(() => message = 'Excluded path must be inside source');
      return null;
    }
    if (relative.isEmpty) {
      setState(() => message = 'Choose a file or child folder inside source');
      return null;
    }
    return relative;
  }

  List<String> _machineIds([String current = '']) {
    final ids = <String>{'local'};
    for (final machine in machines) {
      final id = _str(machine['id']);
      if (id.isNotEmpty) {
        ids.add(id);
      }
    }
    if (current.isNotEmpty) {
      ids.add(current);
    }
    return ids.toList()..sort();
  }

  Future<void> _openConfigDialog() async {
    await showDialog<void>(
      context: context,
      builder: (context) => _MasterDialogFrame(
        title: 'Config',
        width: 900,
        maxHeight: 720,
        child: SizedBox(
          height: 640,
          child: _MasterPre(
            text: const JsonEncoder.withIndent('  ').convert(cfg),
            minHeight: 640,
            maxHeight: 640,
          ),
        ),
      ),
    );
  }

  Future<void> _openTasksDialog() async {
    List<dynamic> tasks = [];
    String errorText = '';
    try {
      tasks = await widget.api.getAllTasks(limit: 120);
    } catch (error) {
      errorText = '$error';
    }
    if (!mounted) {
      return;
    }
    await showDialog<void>(
      context: context,
      builder: (context) {
        return _MasterDialogFrame(
          title: 'Tasks',
          width: 980,
          maxHeight: 760,
          child: errorText.isNotEmpty
              ? _IssueSummary(errorText)
              : _TasksTabbedView(machines: _mapRefs(tasks)),
        );
      },
    );
  }

  Future<void> _openReadmeDialog() async {
    await showDialog<void>(
      context: context,
      builder: (context) => const _MasterDialogFrame(
        title: 'Readme',
        width: 860,
        maxHeight: 720,
        child: _ReadmeBody(),
      ),
    );
  }

  Future<void> _openMachinesDialog() async {
    await showDialog<void>(
      context: context,
      builder: (context) => _MachinesDialog(
        api: widget.api,
        machines: machines,
        initialStatus: machineStatus,
        onChanged: () async {
          cfg = await widget.api.getConfig();
          machineStatus = await widget.api.getMachines();
          if (mounted) {
            setState(() {});
          }
        },
      ),
    );
  }

  Future<void> _openCollectorDialog() async {
    await showDialog<void>(
      context: context,
      builder: (context) => _CollectorDialog(api: widget.api),
    );
  }

  // ignore: unused_element
  Future<void> _openSettingsDialog() async {
    final app = Map<String, dynamic>.from(_app());
    final sync = Map<String, dynamic>.from(_map(app['sync']));
    final result = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) => _SettingsDialog(app: app, sync: sync),
    );
    if (result != null) {
      setState(() {
        cfg['app'] = result;
      });
      await _saveConfig('Settings saved');
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: Column(
        children: [
          _Header(
            machineStatus: machineStatus,
            runtimeStatus: runtimeStatus,
            onMachines: _openMachinesDialog,
            onReadme: _openReadmeDialog,
            onCollector: _openCollectorDialog,
            onConfig: _openConfigDialog,
            onTasks: _openTasksDialog,
          ),
          Expanded(
            child: loading
                ? const Center(
                    child: Text(
                      'Loading auto_sync...',
                      style: TextStyle(color: Palette.muted),
                    ),
                  )
                : _MasterSourcePanel(
                    sources: sources,
                    runtimeStatus: runtimeStatus,
                    syncActivity: syncActivity,
                    taskMachines: taskMachines,
                    busy: busy,
                    machineIdsFor: (source) =>
                        _machineIds(_str(source['machine_id'])),
                    machineLabel: _machineLabel,
                    statusFor: _statusFor,
                    onChanged: _saveConfig,
                    onMutate: (mutate) {
                      setState(mutate);
                      _saveConfig();
                    },
                    onAddSource: () => unawaited(_addSource()),
                    onPickSourcePath: _pickSourcePath,
                    onAddDestination: _addDestination,
                    onPickDestinationPath: _pickDestinationPath,
                    onDestinationInfo: _openDestinationInfo,
                    onDestinationSyncSettings: _openDestinationSyncSettings,
                    onDestinationSchedule: _openDestinationSchedule,
                    onPickExcludePath: _pickExcludePath,
                    onExcludesChanged: _saveSourceExcludes,
                    onRemoveSource: _removeSource,
                    onSyncAll: () => _run('Sync all', widget.api.syncAll),
                    onSyncSource: (id) => _run(
                      'Sync source $id',
                      () => widget.api.syncSource(id),
                    ),
                    onSyncDestination: (sourceId, destinationId, mode) => _run(
                      mode == 'scan'
                          ? 'Compare $sourceId -> $destinationId'
                          : 'Sync $sourceId -> $destinationId',
                      () => mode == 'scan'
                          ? widget.api.scanDestination(sourceId, destinationId)
                          : widget.api.syncDestination(
                              sourceId,
                              destinationId,
                              mode,
                            ),
                    ),
                    onScan: (sourceId, destinationId) => _run(
                      'Compare $sourceId -> $destinationId',
                      () => widget.api.scanDestination(sourceId, destinationId),
                    ),
                    onCancel: (sourceId, destinationId) => _run(
                      'Cancel $sourceId -> $destinationId',
                      () => widget.api.cancelActivity(
                        sourceId: sourceId,
                        destinationId: destinationId,
                      ),
                    ),
                  ),
          ),
          _StatusBar(
            message: message,
            runtimeStatus: runtimeStatus,
            activity: syncActivity,
            saving: saving,
            onConfig: _openSettingsDialog,
          ),
        ],
      ),
    );
  }

  Future<void> _addSource() async {
    final machineId = 'local';
    final selected = await _pickPath(
      startPath: _defaultPathForMachine(machineId),
      machineId: machineId,
      showAddDirectory: true,
      addDirectory: false,
    );
    if (selected == null) return;
    final list = _list(cfg['source_groups']);
    cfg['source_groups'] = list;
    final next = _nextSourceId();
    setState(() {
      list.add({
        'id': next,
        'machine_id': _str(selected['machine_id'], 'local'),
        'src': _str(selected['path']),
        'add_directory': _bool(selected['add_directory']),
        'enabled': true,
        'order': list.length,
        'mode': 'mirror',
        'excludes': [],
        'snapshot': {
          'backend': 'auto',
          'prefix': 'auto_sync',
          'reconcile_interval_secs': 900,
          'keep_extra_cycles': 2,
        },
        'destinations': [],
      });
    });
    await _saveConfig('Source added');
  }

  void _removeSource(String sourceId) {
    final list = _list(cfg['source_groups']);
    setState(() {
      list.removeWhere((item) => _str(_mapRef(item)['id']) == sourceId);
    });
    _saveConfig('Source removed');
  }
}

class _Header extends StatelessWidget {
  const _Header({
    required this.machineStatus,
    required this.runtimeStatus,
    required this.onMachines,
    required this.onReadme,
    required this.onCollector,
    required this.onConfig,
    required this.onTasks,
  });

  final Map<String, dynamic> machineStatus;
  final Map<String, dynamic> runtimeStatus;
  final VoidCallback onMachines;
  final VoidCallback onReadme;
  final VoidCallback onCollector;
  final VoidCallback onConfig;
  final VoidCallback onTasks;

  @override
  Widget build(BuildContext context) {
    final online = _int(machineStatus['online'], 0);
    final total = _int(machineStatus['total'], 0);
    final uptimeLabel = _runtimeUptimeClockLabel(runtimeStatus);
    return Container(
      height: 58,
      padding: const EdgeInsets.symmetric(horizontal: 20),
      decoration: const BoxDecoration(
        color: Palette.panel,
        border: Border(bottom: BorderSide(color: Palette.line)),
      ),
      child: Row(
        children: [
          Expanded(
            child: Align(
              alignment: Alignment.centerLeft,
              child: _HeaderTitle(label: uptimeLabel),
            ),
          ),
          OutlinedButton(
            onPressed: onMachines,
            style: OutlinedButton.styleFrom(
              minimumSize: const Size(118, 34),
              padding: const EdgeInsets.symmetric(horizontal: 12),
              foregroundColor: Palette.accent,
              side: const BorderSide(color: Palette.line),
              shape: RoundedRectangleBorder(
                borderRadius: BorderRadius.circular(6),
              ),
              textStyle: const TextStyle(
                fontSize: 13,
                fontWeight: FontWeight.w700,
              ),
            ),
            child: Text(
              total == 0 ? 'Machines -/-' : 'Machines $online/$total',
            ),
          ),
          Expanded(
            child: Align(
              alignment: Alignment.centerRight,
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  MasterButton(label: 'Readme', width: 80, onTap: onReadme),
                  const SizedBox(width: 8),
                  MasterButton(
                    label: 'Collector',
                    width: 104,
                    onTap: onCollector,
                  ),
                  const SizedBox(width: 8),
                  MasterButton(label: 'Config', width: 76, onTap: onConfig),
                  const SizedBox(width: 8),
                  MasterButton(label: 'Tasks', width: 70, onTap: onTasks),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _HeaderTitle extends StatelessWidget {
  const _HeaderTitle({required this.label});

  final String label;

  @override
  Widget build(BuildContext context) {
    return Tooltip(
      message: label.isEmpty ? '' : 'uptime $label',
      child: Text(
        label,
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
        style: const TextStyle(fontSize: 13, color: Palette.text),
      ),
    );
  }
}

String _runtimeUptimeClockLabel(Map<String, dynamic> runtimeStatus) {
  final seconds = _int(runtimeStatus['process_uptime_secs'], -1);
  if (seconds < 0) return '';
  final days = seconds ~/ Duration.secondsPerDay;
  final rem = seconds % Duration.secondsPerDay;
  final hours = rem ~/ Duration.secondsPerHour;
  final minutes = (rem % Duration.secondsPerHour) ~/ Duration.secondsPerMinute;
  final secs = rem % Duration.secondsPerMinute;
  String pad(int part) => part.toString().padLeft(2, '0');
  final clock = '${pad(hours)}:${pad(minutes)}:${pad(secs)}';
  return days > 0 ? '${days}d $clock' : clock;
}

const double _masterRightBlockWidth = 446;
const double _masterControlHeight = 34;
const double _settingsInputHeight = 38;
const double _masterControlGap = 8;
const double _masterLabelControlGap = 4;
const double _masterStatusDotSize = 10;

class _MasterSourcePanel extends StatelessWidget {
  const _MasterSourcePanel({
    required this.sources,
    required this.runtimeStatus,
    required this.syncActivity,
    required this.taskMachines,
    required this.busy,
    required this.machineIdsFor,
    required this.machineLabel,
    required this.statusFor,
    required this.onChanged,
    required this.onMutate,
    required this.onAddSource,
    required this.onPickSourcePath,
    required this.onAddDestination,
    required this.onPickDestinationPath,
    required this.onDestinationInfo,
    required this.onDestinationSyncSettings,
    required this.onDestinationSchedule,
    required this.onPickExcludePath,
    required this.onExcludesChanged,
    required this.onRemoveSource,
    required this.onSyncAll,
    required this.onSyncSource,
    required this.onSyncDestination,
    required this.onScan,
    required this.onCancel,
  });

  final List<Map<String, dynamic>> sources;
  final Map<String, dynamic> runtimeStatus;
  final Map<String, dynamic> syncActivity;
  final List<dynamic> taskMachines;
  final bool busy;
  final List<String> Function(Map<String, dynamic> source) machineIdsFor;
  final String Function(String id) machineLabel;
  final Map<String, dynamic>? Function(String sourceId, String destinationId)
  statusFor;
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
  final VoidCallback onAddSource;
  final Future<void> Function(Map<String, dynamic> source) onPickSourcePath;
  final Future<void> Function(Map<String, dynamic> source) onAddDestination;
  final Future<void> Function(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  )
  onPickDestinationPath;
  final Future<void> Function(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  )
  onDestinationInfo;
  final Future<void> Function(Map<String, dynamic> destination)
  onDestinationSyncSettings;
  final Future<void> Function(Map<String, dynamic> destination)
  onDestinationSchedule;
  final Future<String?> Function(Map<String, dynamic> source) onPickExcludePath;
  final Future<void> Function(String sourceId, List<String> excludes)
  onExcludesChanged;
  final void Function(String sourceId) onRemoveSource;
  final VoidCallback onSyncAll;
  final void Function(String id) onSyncSource;
  final void Function(String sourceId, String destinationId, String mode)
  onSyncDestination;
  final void Function(String sourceId, String destinationId) onScan;
  final void Function(String sourceId, String destinationId) onCancel;

  @override
  Widget build(BuildContext context) {
    final syncDisabled = _SyncDisableState.from(
      sources: sources,
      runtimeStatus: runtimeStatus,
      syncActivity: syncActivity,
      taskMachines: taskMachines,
      busy: busy,
      statusFor: statusFor,
    );
    return LayoutBuilder(
      builder: (context, constraints) {
        final pageWidth = constraints.maxWidth < 860
            ? 860.0
            : constraints.maxWidth;
        return SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: SizedBox(
            width: pageWidth,
            child: SingleChildScrollView(
              padding: const EdgeInsets.fromLTRB(80, 14, 80, 56),
              child: Container(
                width: pageWidth - 160,
                padding: const EdgeInsets.all(14),
                decoration: BoxDecoration(
                  color: Palette.panel,
                  border: Border.all(color: Palette.line),
                  borderRadius: BorderRadius.circular(8),
                ),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    Container(
                      padding: const EdgeInsets.only(bottom: 12),
                      decoration: const BoxDecoration(
                        border: Border(bottom: BorderSide(color: Palette.line)),
                      ),
                      child: Row(
                        children: [
                          Expanded(
                            child: const Text(
                              'Source',
                              style: TextStyle(
                                fontSize: 15,
                                fontWeight: FontWeight.w700,
                                color: Palette.text,
                              ),
                            ),
                          ),
                          MasterButton(
                            label: 'Sync All',
                            primary: true,
                            onTap: syncDisabled.anySyncDisabled
                                ? null
                                : onSyncAll,
                          ),
                          const SizedBox(width: 8),
                          MasterButton(label: 'Add Source', onTap: onAddSource),
                        ],
                      ),
                    ),
                    const SizedBox(height: 14),
                    Column(
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: sources.isEmpty
                          ? const []
                          : [
                              for (var i = 0; i < sources.length; i += 1) ...[
                                _MasterSourceGroup(
                                  source: sources[i],
                                  machineIds: machineIdsFor(sources[i]),
                                  machineLabel: machineLabel,
                                  statusFor: statusFor,
                                  syncDisabled: syncDisabled,
                                  onChanged: onChanged,
                                  onMutate: onMutate,
                                  onPickSourcePath: onPickSourcePath,
                                  onAddDestination: onAddDestination,
                                  onPickDestinationPath: onPickDestinationPath,
                                  onDestinationInfo: onDestinationInfo,
                                  onDestinationSyncSettings:
                                      onDestinationSyncSettings,
                                  onDestinationSchedule: onDestinationSchedule,
                                  onPickExcludePath: onPickExcludePath,
                                  onExcludesChanged: onExcludesChanged,
                                  onRemoveSource: onRemoveSource,
                                  onSyncSource: onSyncSource,
                                  onSyncDestination: onSyncDestination,
                                  onScan: onScan,
                                  onCancel: onCancel,
                                ),
                                if (i != sources.length - 1)
                                  const SizedBox(height: 18),
                              ],
                            ],
                    ),
                  ],
                ),
              ),
            ),
          ),
        );
      },
    );
  }
}

class _MasterSourceGroup extends StatelessWidget {
  const _MasterSourceGroup({
    required this.source,
    required this.machineIds,
    required this.machineLabel,
    required this.statusFor,
    required this.syncDisabled,
    required this.onChanged,
    required this.onMutate,
    required this.onPickSourcePath,
    required this.onAddDestination,
    required this.onPickDestinationPath,
    required this.onDestinationInfo,
    required this.onDestinationSyncSettings,
    required this.onDestinationSchedule,
    required this.onPickExcludePath,
    required this.onExcludesChanged,
    required this.onRemoveSource,
    required this.onSyncSource,
    required this.onSyncDestination,
    required this.onScan,
    required this.onCancel,
  });

  final Map<String, dynamic> source;
  final List<String> machineIds;
  final String Function(String id) machineLabel;
  final Map<String, dynamic>? Function(String sourceId, String destinationId)
  statusFor;
  final _SyncDisableState syncDisabled;
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
  final Future<void> Function(Map<String, dynamic> source) onPickSourcePath;
  final Future<void> Function(Map<String, dynamic> source) onAddDestination;
  final Future<void> Function(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  )
  onPickDestinationPath;
  final Future<void> Function(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  )
  onDestinationInfo;
  final Future<void> Function(Map<String, dynamic> destination)
  onDestinationSyncSettings;
  final Future<void> Function(Map<String, dynamic> destination)
  onDestinationSchedule;
  final Future<String?> Function(Map<String, dynamic> source) onPickExcludePath;
  final Future<void> Function(String sourceId, List<String> excludes)
  onExcludesChanged;
  final void Function(String sourceId) onRemoveSource;
  final void Function(String id) onSyncSource;
  final void Function(String sourceId, String destinationId, String mode)
  onSyncDestination;
  final void Function(String sourceId, String destinationId) onScan;
  final void Function(String sourceId, String destinationId) onCancel;

  @override
  Widget build(BuildContext context) {
    source['destinations'] = _list(source['destinations']);
    source['excludes'] = _list(source['excludes']);
    final sourceId = _str(source['id'], 'source');
    final destinations = _mapRefs(source['destinations']);
    final sourceSyncDisabled = syncDisabled.sourceDisabled(source);
    final latest = _sourceLatestCycle(
      destinations.map((dst) => statusFor(sourceId, _str(dst['id']))),
    );
    return Stack(
      children: [
        const Positioned(
          left: 7,
          top: 12,
          child: Text(
            '⠿',
            style: TextStyle(color: Color(0xff94a3b8), fontSize: 13, height: 1),
          ),
        ),
        Container(
          width: double.infinity,
          padding: const EdgeInsets.fromLTRB(28, 10, 20, 12),
          decoration: BoxDecoration(
            color: const Color(0xfff8fafc),
            borderRadius: BorderRadius.circular(6),
          ),
          child: Column(
            children: [
              _MasterSplitRow(
                rightWidth: _masterRightBlockWidth,
                leftLabels: const [
                  _MasterLabelBox('ID', width: 56),
                  SizedBox(width: 8),
                  _MasterLabelBox('Source Path', width: 160),
                ],
                leftControls: [
                  SizedBox(
                    width: 56,
                    child: _MasterReadOnlyInput(value: sourceId),
                  ),
                  const SizedBox(width: 8),
                  SizedBox(
                    width: 160,
                    child: InkWell(
                      onTap: () => unawaited(onPickSourcePath(source)),
                      child: _MasterReadOnlyInput(
                        value: _machinePath(
                          _str(source['machine_id'], 'local'),
                          _str(source['src']),
                        ),
                      ),
                    ),
                  ),
                ],
                rightLabels: const [
                  _MasterLabelBox('Latest Cycle', width: 100),
                  SizedBox(width: 8),
                  _MasterLabelBox('', width: 112),
                  SizedBox(width: 8),
                  _MasterLabelBox('', width: 58),
                  SizedBox(width: 8),
                  _MasterLabelBox('', width: 34),
                ],
                rightControls: [
                  SizedBox(
                    width: 100,
                    child: _MasterReadOnlyInput(value: latest),
                  ),
                  const SizedBox(width: 8),
                  MasterButton(
                    label: _excludeButtonLabel(source['excludes']),
                    width: 112,
                    onTap: () => _showExcludes(context),
                  ),
                  const SizedBox(width: 8),
                  MasterButton(
                    label: 'Sync',
                    width: 58,
                    onTap: sourceSyncDisabled
                        ? null
                        : () => onSyncSource(sourceId),
                  ),
                  const SizedBox(width: 8),
                  MasterButton(
                    label: 'x',
                    square: true,
                    danger: true,
                    onTap: () => onRemoveSource(sourceId),
                  ),
                ],
              ),
              for (final dst in destinations)
                _MasterDestinationRow(
                  sourceId: sourceId,
                  source: source,
                  destination: dst,
                  destinations: _list(source['destinations']),
                  status: statusFor(sourceId, _str(dst['id'])),
                  syncDisabled: syncDisabled,
                  onChanged: onChanged,
                  onMutate: onMutate,
                  onPickPath: onPickDestinationPath,
                  onInfo: onDestinationInfo,
                  onSyncSettings: onDestinationSyncSettings,
                  onSchedule: onDestinationSchedule,
                  onSync: onSyncDestination,
                  onScan: onScan,
                  onCancel: onCancel,
                ),
              Align(
                alignment: Alignment.centerRight,
                child: MasterButton(
                  label: '+',
                  square: true,
                  accent: true,
                  onTap: () => unawaited(onAddDestination(source)),
                ),
              ),
            ],
          ),
        ),
      ],
    );
  }

  Future<void> _showExcludes(BuildContext context) async {
    await showDialog<void>(
      context: context,
      builder: (context) => _ExcludedDialog(
        sourceId: _str(source['id'], 'source'),
        sourcePath: _machinePath(
          _str(source['machine_id'], 'local'),
          _str(source['src'], '-'),
        ),
        initialItems: _list(source['excludes']).map((item) => '$item').toList(),
        onAddPath: () => onPickExcludePath(source),
        onChanged: (items) => onExcludesChanged(_str(source['id']), items),
      ),
    );
  }
}

class _MasterDestinationRow extends StatelessWidget {
  const _MasterDestinationRow({
    required this.sourceId,
    required this.source,
    required this.destination,
    required this.destinations,
    required this.status,
    required this.syncDisabled,
    required this.onChanged,
    required this.onMutate,
    required this.onPickPath,
    required this.onInfo,
    required this.onSyncSettings,
    required this.onSchedule,
    required this.onSync,
    required this.onScan,
    required this.onCancel,
  });

  final String sourceId;
  final Map<String, dynamic> source;
  final Map<String, dynamic> destination;
  final List<dynamic> destinations;
  final Map<String, dynamic>? status;
  final _SyncDisableState syncDisabled;
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
  final Future<void> Function(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  )
  onPickPath;
  final Future<void> Function(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  )
  onInfo;
  final Future<void> Function(Map<String, dynamic> destination) onSyncSettings;
  final Future<void> Function(Map<String, dynamic> destination) onSchedule;
  final void Function(String sourceId, String destinationId, String mode)
  onSync;
  final void Function(String sourceId, String destinationId) onScan;
  final void Function(String sourceId, String destinationId) onCancel;

  @override
  Widget build(BuildContext context) {
    destination['schedule'] = _mapRef(destination['schedule']);
    final schedule = destination['schedule'] as Map<String, dynamic>;
    final dstId = _str(destination['id'], 'dst');
    final dstSyncDisabled = syncDisabled.destinationDisabled(
      source,
      destination,
      status,
    );
    final diffCount = _int(status?['scan_differences']);
    final showRepair = diffCount > 0;
    final showCancel = syncDisabled.destinationRunning(source, destination);
    final infoCellWidth = showRepair ? 72.0 : 34.0;
    return _MasterSplitRow(
      rightWidth:
          _masterRightBlockWidth +
          (showRepair ? 38 : 0) +
          (showCancel ? 42 : 0),
      leftControlMarker: Container(
        width: _masterStatusDotSize,
        height: _masterStatusDotSize,
        decoration: BoxDecoration(
          color: _statusColor(status),
          shape: BoxShape.circle,
        ),
      ),
      leftLabels: const [
        _MasterLabelBox('ID', width: 56),
        SizedBox(width: 8),
        _MasterLabelBox('Destination Path', width: 160),
      ],
      leftControls: [
        SizedBox(width: 56, child: _MasterReadOnlyInput(value: dstId)),
        const SizedBox(width: 8),
        SizedBox(
          width: 160,
          child: InkWell(
            onTap: () => unawaited(onPickPath(source, destination)),
            child: _MasterReadOnlyInput(
              value: _machinePath(
                _str(destination['machine_id'], 'local'),
                _str(destination['path']),
              ),
            ),
          ),
        ),
      ],
      rightLabels: [
        _MasterLabelBox('', width: infoCellWidth),
        const SizedBox(width: 8),
        const _MasterLabelBox('Schedule', width: 100),
        const SizedBox(width: 8),
        const _MasterLabelBox('Cycle', width: 100),
        const SizedBox(width: 8),
        const _MasterLabelBox('', width: 34),
        const SizedBox(width: 8),
        const _MasterLabelBox('Sync', width: 104),
        const SizedBox(width: 8),
        if (showCancel) ...[
          const _MasterLabelBox('', width: 34),
          const SizedBox(width: 8),
        ],
        const _MasterLabelBox('', width: 34),
      ],
      rightControls: [
        SizedBox(
          width: infoCellWidth,
          child: Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              if (showRepair) ...[
                _RepairScanButton(
                  enabled: !dstSyncDisabled,
                  diffCount: diffCount,
                  onTap: () => onSync(sourceId, dstId, 'repair_scan'),
                ),
                const SizedBox(width: 4),
              ],
              MasterIconButton(
                kind: MasterIconKind.info,
                color: _statusColor(status),
                onTap: () => unawaited(onInfo(source, destination)),
              ),
            ],
          ),
        ),
        const SizedBox(width: 8),
        MasterButton(
          label: _scheduleLabel(schedule),
          width: 100,
          accent: true,
          alignLeft: true,
          onTap: () => unawaited(onSchedule(destination)),
        ),
        const SizedBox(width: 8),
        SizedBox(
          width: 100,
          child: _MasterReadOnlyInput(value: _cycleDisplay(status)),
        ),
        const SizedBox(width: 8),
        MasterIconButton(
          kind: MasterIconKind.gear,
          color: Palette.text,
          onTap: () => unawaited(onSyncSettings(destination)),
        ),
        const SizedBox(width: 8),
        MasterSelectButton(
          value: 'Sync',
          width: 104,
          enabled: !dstSyncDisabled,
          onSelected: (mode) => onSync(sourceId, dstId, mode),
        ),
        const SizedBox(width: 8),
        if (showCancel) ...[
          MasterIconButton(
            kind: MasterIconKind.cancel,
            color: Palette.red,
            tooltip: 'Cancel running task',
            onTap: () => onCancel(sourceId, dstId),
          ),
          const SizedBox(width: 8),
        ],
        MasterButton(
          label: 'x',
          square: true,
          danger: true,
          onTap: () => onMutate(() => destinations.remove(destination)),
        ),
      ],
    );
  }
}

class _MasterSplitRow extends StatelessWidget {
  const _MasterSplitRow({
    required this.leftLabels,
    required this.leftControls,
    required this.rightLabels,
    required this.rightControls,
    required this.rightWidth,
    this.leftControlMarker,
  });

  final List<Widget> leftLabels;
  final List<Widget> leftControls;
  final List<Widget> rightLabels;
  final List<Widget> rightControls;
  final double rightWidth;
  final Widget? leftControlMarker;

  @override
  Widget build(BuildContext context) {
    return Container(
      margin: const EdgeInsets.only(bottom: 6),
      padding: const EdgeInsets.only(bottom: 6),
      decoration: const BoxDecoration(
        border: Border(bottom: BorderSide(color: Palette.line)),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.end,
        children: [
          Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(children: leftLabels),
              const SizedBox(height: _masterLabelControlGap),
              Stack(
                clipBehavior: Clip.none,
                children: [
                  if (leftControlMarker != null)
                    Positioned(
                      left: -16,
                      top: (_masterControlHeight - _masterStatusDotSize) / 2,
                      child: leftControlMarker!,
                    ),
                  Row(children: leftControls),
                ],
              ),
            ],
          ),
          const SizedBox(width: 8),
          Expanded(
            child: Align(
              alignment: Alignment.centerRight,
              child: SizedBox(
                width: rightWidth,
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.end,
                  children: [
                    Row(
                      mainAxisAlignment: MainAxisAlignment.end,
                      children: rightLabels,
                    ),
                    const SizedBox(height: _masterLabelControlGap),
                    Row(
                      mainAxisAlignment: MainAxisAlignment.end,
                      children: rightControls,
                    ),
                  ],
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _MasterLabelBox extends StatelessWidget {
  const _MasterLabelBox(this.text, {required this.width});

  final String text;
  final double width;

  @override
  Widget build(BuildContext context) {
    return SizedBox(width: width, child: _MasterLabel(text));
  }
}

class _MasterReadOnlyInput extends StatelessWidget {
  const _MasterReadOnlyInput({required this.value});

  final String value;

  @override
  Widget build(BuildContext context) {
    return Container(
      height: _masterControlHeight,
      width: double.infinity,
      alignment: Alignment.centerLeft,
      padding: const EdgeInsets.symmetric(horizontal: 9),
      decoration: BoxDecoration(
        color: Colors.white,
        border: Border.all(color: Palette.line),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Text(
        value,
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
        style: const TextStyle(fontSize: 13, color: Palette.text),
      ),
    );
  }
}

class _ExcludedDialog extends StatefulWidget {
  const _ExcludedDialog({
    required this.sourceId,
    required this.sourcePath,
    required this.initialItems,
    required this.onChanged,
    this.onAddPath,
  });

  final String sourceId;
  final String sourcePath;
  final List<String> initialItems;
  final Future<void> Function(List<String> items) onChanged;
  final Future<String?> Function()? onAddPath;

  @override
  State<_ExcludedDialog> createState() => _ExcludedDialogState();
}

class _ExcludedDialogState extends State<_ExcludedDialog> {
  late List<String> items;
  bool saving = false;

  @override
  void initState() {
    super.initState();
    items = _cleanStringList(widget.initialItems);
  }

  Future<void> _persist(List<String> next) async {
    final cleaned = _cleanStringList(next);
    setState(() {
      items = cleaned;
      saving = true;
    });
    try {
      await widget.onChanged(cleaned);
    } finally {
      if (mounted) {
        setState(() => saving = false);
      }
    }
  }

  Future<void> _add() async {
    if (saving) return;
    final picked = await widget.onAddPath?.call();
    if (picked == null && widget.onAddPath != null) {
      return;
    }
    await _persist([...items, picked ?? '']);
  }

  Future<void> _remove(String path) async {
    if (saving) return;
    await _persist(items.where((item) => item != path).toList());
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: 'Excluded',
      width: 780,
      maxHeight: 360,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _IssueSummary('${widget.sourceId}: ${widget.sourcePath}'),
          const SizedBox(height: 8),
          Expanded(
            child: Container(
              decoration: const BoxDecoration(
                border: Border(top: BorderSide(color: Palette.line)),
              ),
              child: items.isEmpty
                  ? const SizedBox.shrink()
                  : ListView.builder(
                      itemCount: items.length,
                      itemBuilder: (context, index) {
                        final path = items[index];
                        return Container(
                          constraints: const BoxConstraints(minHeight: 42),
                          decoration: const BoxDecoration(
                            border: Border(
                              bottom: BorderSide(color: Palette.line),
                            ),
                          ),
                          padding: const EdgeInsets.symmetric(vertical: 4),
                          child: Row(
                            children: [
                              Expanded(
                                child: Text(
                                  path,
                                  maxLines: 1,
                                  overflow: TextOverflow.ellipsis,
                                  style: const TextStyle(
                                    fontFamily: 'Consolas',
                                    fontSize: 12,
                                    color: Palette.text,
                                  ),
                                ),
                              ),
                              const SizedBox(width: _masterControlGap),
                              MasterButton(
                                label: 'x',
                                square: true,
                                danger: true,
                                onTap: saving
                                    ? null
                                    : () => unawaited(_remove(path)),
                              ),
                            ],
                          ),
                        );
                      },
                    ),
            ),
          ),
          const SizedBox(height: 10),
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              MasterButton(
                label: 'Add',
                width: 72,
                primary: true,
                onTap: saving ? null : () => unawaited(_add()),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

class _MasterLabel extends StatelessWidget {
  const _MasterLabel(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: 22,
      child: Align(
        alignment: Alignment.bottomLeft,
        child: Text(
          text,
          style: const TextStyle(color: Palette.muted, fontSize: 12),
        ),
      ),
    );
  }
}

class MasterButton extends StatelessWidget {
  const MasterButton({
    super.key,
    required this.label,
    required this.onTap,
    this.child,
    this.width,
    this.square = false,
    this.danger = false,
    this.accent = false,
    this.primary = false,
    this.alignLeft = false,
  });

  final String label;
  final VoidCallback? onTap;
  final Widget? child;
  final double? width;
  final bool square;
  final bool danger;
  final bool accent;
  final bool primary;
  final bool alignLeft;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: square ? _masterControlHeight : width,
      height: _masterControlHeight,
      child: OutlinedButton(
        onPressed: onTap,
        style: OutlinedButton.styleFrom(
          minimumSize: Size(
            square ? _masterControlHeight : 0,
            _masterControlHeight,
          ),
          maximumSize: Size(width ?? double.infinity, _masterControlHeight),
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          visualDensity: VisualDensity.compact,
          padding: EdgeInsets.symmetric(horizontal: square ? 0 : 12),
          backgroundColor: primary ? Palette.accent : Colors.white,
          disabledBackgroundColor: const Color(0xffe2e8f0),
          foregroundColor: danger
              ? Palette.red
              : primary
              ? Colors.white
              : accent
              ? Palette.accent
              : Palette.text,
          disabledForegroundColor: const Color(0xff64748b),
          side: BorderSide(color: primary ? Palette.accent : Palette.line),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(6)),
          textStyle: const TextStyle(fontSize: 13, fontWeight: FontWeight.w600),
          alignment: alignLeft ? Alignment.centerLeft : Alignment.center,
        ),
        child:
            child ?? Text(label, maxLines: 1, overflow: TextOverflow.ellipsis),
      ),
    );
  }
}

enum MasterIconKind { info, gear, cancel }

class MasterIconButton extends StatelessWidget {
  const MasterIconButton({
    super.key,
    required this.kind,
    required this.color,
    required this.onTap,
    this.tooltip,
  });

  final MasterIconKind kind;
  final Color color;
  final VoidCallback? onTap;
  final String? tooltip;

  @override
  Widget build(BuildContext context) {
    final button = MasterButton(
      label: '',
      square: true,
      onTap: onTap,
      child: switch (kind) {
        MasterIconKind.info => Container(
          width: 18,
          height: 18,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            shape: BoxShape.circle,
            border: Border.all(color: color, width: 2),
          ),
          child: Text(
            'i',
            style: TextStyle(
              color: color,
              fontSize: 12,
              fontWeight: FontWeight.w800,
              height: 1,
            ),
          ),
        ),
        MasterIconKind.gear => Text(
          '\u2699\uFE0E',
          style: TextStyle(
            color: color,
            fontFamily: 'Segoe UI Symbol',
            fontSize: 14,
            fontWeight: FontWeight.w700,
            height: 1,
          ),
        ),
        MasterIconKind.cancel => Icon(
          Icons.stop_rounded,
          size: 18,
          color: color,
        ),
      },
    );
    return tooltip == null ? button : Tooltip(message: tooltip!, child: button);
  }
}

class _RepairScanButton extends StatelessWidget {
  const _RepairScanButton({
    required this.enabled,
    required this.diffCount,
    required this.onTap,
  });

  final bool enabled;
  final int diffCount;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final border = enabled ? const Color(0xffb45309) : const Color(0xff94a3b8);
    final background = enabled
        ? const Color(0x1ab45309)
        : const Color(0xffe2e8f0);
    final foreground = enabled
        ? const Color(0xffb45309)
        : const Color(0xff64748b);
    return Tooltip(
      message:
          'Compare found $diffCount difference${diffCount == 1 ? '' : 's'} - sync just these paths',
      child: SizedBox(
        width: _masterControlHeight,
        height: _masterControlHeight,
        child: OutlinedButton(
          onPressed: enabled ? onTap : null,
          style: OutlinedButton.styleFrom(
            minimumSize: const Size(_masterControlHeight, _masterControlHeight),
            maximumSize: const Size(_masterControlHeight, _masterControlHeight),
            tapTargetSize: MaterialTapTargetSize.shrinkWrap,
            visualDensity: VisualDensity.compact,
            padding: EdgeInsets.zero,
            backgroundColor: background,
            foregroundColor: foreground,
            disabledBackgroundColor: background,
            disabledForegroundColor: foreground,
            side: BorderSide(color: border),
            shape: RoundedRectangleBorder(
              borderRadius: BorderRadius.circular(6),
            ),
            textStyle: const TextStyle(
              fontSize: 14,
              fontWeight: FontWeight.w700,
            ),
          ),
          child: const Text('\u21c6'),
        ),
      ),
    );
  }
}

class MasterSelectButton extends StatelessWidget {
  const MasterSelectButton({
    super.key,
    required this.value,
    required this.width,
    required this.onSelected,
    this.enabled = true,
  });

  final String value;
  final double width;
  final ValueChanged<String> onSelected;
  final bool enabled;

  @override
  Widget build(BuildContext context) {
    final background = enabled ? Colors.white : const Color(0xffe2e8f0);
    final border = enabled ? Palette.line : const Color(0xffcbd5e1);
    final foreground = enabled ? Palette.text : const Color(0xff64748b);
    return SizedBox(
      width: width,
      height: _masterControlHeight,
      child: _MasterMenuSelect(
        value: value,
        options: const ['incremental', 'full', 'scan'],
        labelFor: (option) => switch (option) {
          'incremental' => 'Incremental',
          'full' => 'Full',
          'scan' => 'Compare',
          _ => option,
        },
        enabled: enabled,
        tooltip: 'Sync',
        height: _masterControlHeight,
        background: background,
        border: border,
        foreground: foreground,
        onSelected: onSelected,
      ),
    );
  }
}

class _MasterMenuSelect extends StatefulWidget {
  const _MasterMenuSelect({
    required this.value,
    required this.options,
    required this.onSelected,
    this.labelFor,
    this.enabled = true,
    this.tooltip,
    this.height = _masterControlHeight,
    this.background = Colors.white,
    this.border = Palette.line,
    this.foreground = Palette.text,
    this.openUp = false,
  });

  final String value;
  final List<String> options;
  final String Function(String value)? labelFor;
  final ValueChanged<String> onSelected;
  final bool enabled;
  final String? tooltip;
  final double height;
  final Color background;
  final Color border;
  final Color foreground;
  final bool openUp;

  @override
  State<_MasterMenuSelect> createState() => _MasterMenuSelectState();
}

class _MasterMenuSelectState extends State<_MasterMenuSelect> {
  final MenuController _controller = MenuController();

  String _label(String value) =>
      widget.labelFor == null ? value : widget.labelFor!(value);

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final width = constraints.hasBoundedWidth && constraints.maxWidth > 0
            ? constraints.maxWidth
            : 180.0;
        final menuHeight = widget.height * widget.options.length;
        final menuWidth = width.isFinite ? width : 180.0;
        final menuOffset = widget.openUp
            ? Offset(0, -menuHeight - 2)
            : const Offset(0, 2);
        final menu = MenuAnchor(
          controller: _controller,
          alignmentOffset: menuOffset,
          style: MenuStyle(
            backgroundColor: const WidgetStatePropertyAll(Colors.white),
            surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
            elevation: const WidgetStatePropertyAll(2),
            shadowColor: const WidgetStatePropertyAll(Color(0x33000000)),
            padding: const WidgetStatePropertyAll(EdgeInsets.zero),
            alignment: widget.openUp
                ? AlignmentDirectional.topStart
                : AlignmentDirectional.bottomStart,
            minimumSize: WidgetStatePropertyAll(Size(menuWidth, widget.height)),
            maximumSize: WidgetStatePropertyAll(Size(menuWidth, menuHeight)),
            shape: WidgetStatePropertyAll(
              RoundedRectangleBorder(
                side: const BorderSide(color: Palette.line),
                borderRadius: BorderRadius.circular(6),
              ),
            ),
          ),
          menuChildren: [
            for (final option in widget.options)
              SizedBox(
                width: menuWidth,
                height: widget.height,
                child: MenuItemButton(
                  style: ButtonStyle(
                    padding: const WidgetStatePropertyAll(
                      EdgeInsets.symmetric(horizontal: 12),
                    ),
                    minimumSize: WidgetStatePropertyAll(Size(0, widget.height)),
                    maximumSize: WidgetStatePropertyAll(
                      Size(double.infinity, widget.height),
                    ),
                    visualDensity: VisualDensity.compact,
                  ),
                  onPressed: () {
                    _controller.close();
                    widget.onSelected(option);
                  },
                  child: Align(
                    alignment: Alignment.centerLeft,
                    child: Text(
                      _label(option),
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                    ),
                  ),
                ),
              ),
          ],
          builder: (context, controller, child) {
            return InkWell(
              borderRadius: BorderRadius.circular(6),
              onTap: widget.enabled
                  ? () {
                      controller.isOpen
                          ? controller.close()
                          : controller.open();
                    }
                  : null,
              child: Container(
                height: widget.height,
                padding: const EdgeInsets.symmetric(horizontal: 10),
                decoration: BoxDecoration(
                  color: widget.background,
                  border: Border.all(color: widget.border),
                  borderRadius: BorderRadius.circular(6),
                ),
                child: Row(
                  children: [
                    Expanded(
                      child: Text(
                        _label(widget.value),
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(
                          color: widget.foreground,
                          fontSize: 13,
                          fontWeight: FontWeight.w600,
                        ),
                      ),
                    ),
                    Icon(
                      Icons.arrow_drop_down,
                      size: 18,
                      color: widget.foreground,
                    ),
                  ],
                ),
              ),
            );
          },
        );
        if (widget.tooltip == null) {
          return menu;
        }
        return Tooltip(message: widget.tooltip!, child: menu);
      },
    );
  }
}

class _MasterIconMenuButton extends StatefulWidget {
  const _MasterIconMenuButton({
    required this.icon,
    required this.tooltip,
    required this.options,
    required this.labelFor,
    required this.onSelected,
  });

  final IconData icon;
  final String tooltip;
  final List<String> options;
  final String Function(String value) labelFor;
  final ValueChanged<String> onSelected;

  @override
  State<_MasterIconMenuButton> createState() => _MasterIconMenuButtonState();
}

class _MasterIconMenuButtonState extends State<_MasterIconMenuButton> {
  final MenuController _controller = MenuController();

  @override
  Widget build(BuildContext context) {
    const menuWidth = 116.0;
    return MenuAnchor(
      controller: _controller,
      alignmentOffset: const Offset(0, 2),
      style: MenuStyle(
        backgroundColor: const WidgetStatePropertyAll(Colors.white),
        surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
        elevation: const WidgetStatePropertyAll(2),
        shadowColor: const WidgetStatePropertyAll(Color(0x33000000)),
        padding: const WidgetStatePropertyAll(EdgeInsets.zero),
        minimumSize: const WidgetStatePropertyAll(
          Size(menuWidth, _masterControlHeight),
        ),
        maximumSize: WidgetStatePropertyAll(
          Size(menuWidth, _masterControlHeight * widget.options.length),
        ),
        shape: WidgetStatePropertyAll(
          RoundedRectangleBorder(
            side: const BorderSide(color: Palette.line),
            borderRadius: BorderRadius.circular(6),
          ),
        ),
      ),
      menuChildren: [
        for (final option in widget.options)
          SizedBox(
            width: menuWidth,
            height: _masterControlHeight,
            child: MenuItemButton(
              style: const ButtonStyle(
                padding: WidgetStatePropertyAll(
                  EdgeInsets.symmetric(horizontal: 12),
                ),
                minimumSize: WidgetStatePropertyAll(
                  Size(0, _masterControlHeight),
                ),
                maximumSize: WidgetStatePropertyAll(
                  Size(double.infinity, _masterControlHeight),
                ),
                visualDensity: VisualDensity.compact,
              ),
              onPressed: () {
                _controller.close();
                widget.onSelected(option);
              },
              child: Align(
                alignment: Alignment.centerLeft,
                child: Text(
                  widget.labelFor(option),
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
            ),
          ),
      ],
      builder: (context, controller, child) => IconButton(
        tooltip: widget.tooltip,
        icon: Icon(widget.icon, size: 21),
        onPressed: () {
          controller.isOpen ? controller.close() : controller.open();
        },
      ),
    );
  }
}

String _machinePath(String machineId, String path) {
  final prefix = machineId.isEmpty ? 'local' : machineId;
  return '$prefix: ${_displayPath(path)}';
}

String _displayPath(String value) {
  return value
      .replaceFirst(RegExp(r'^\\\\\?\\UNC\\', caseSensitive: false), r'\\')
      .replaceFirst(RegExp(r'^\\\\\?\\'), '');
}

String _compactStatusPath(String value, int maxChars) {
  final path = _displayPath(value);
  if (path.length <= maxChars) return path;
  final separator = path.contains('\\') ? '\\' : '/';
  var prefix = '';
  var rest = path;
  final drive = RegExp(r'^[A-Za-z]:\\').firstMatch(path);
  if (drive != null) {
    prefix = drive.group(0)!;
    rest = path.substring(prefix.length);
  } else if (path.startsWith('\\\\')) {
    final parts = path.substring(2).split(RegExp(r'[\\\/]+'));
    if (parts.length >= 2) {
      prefix = '\\\\${parts[0]}\\${parts[1]}\\';
      rest = parts.skip(2).join(separator);
    }
  } else if (path.startsWith('/')) {
    prefix = '/';
    rest = path.substring(1);
  }

  final parts = rest
      .split(RegExp(r'[\\\/]+'))
      .where((part) => part.isNotEmpty)
      .toList();
  if (parts.length <= 2) {
    return '${path.substring(0, math.max(0, maxChars - 3))}...';
  }

  var headCount = math.min(4, parts.length - 1);
  var tailCount = math.min(3, parts.length - headCount);
  var compact = _renderCompactPath(
    prefix,
    parts,
    separator,
    headCount,
    tailCount,
  );
  while (compact.length > maxChars && headCount > 1) {
    headCount -= 1;
    compact = _renderCompactPath(
      prefix,
      parts,
      separator,
      headCount,
      tailCount,
    );
  }
  while (compact.length > maxChars && tailCount > 1) {
    tailCount -= 1;
    compact = _renderCompactPath(
      prefix,
      parts,
      separator,
      headCount,
      tailCount,
    );
  }
  if (compact.length <= maxChars) return compact;
  final tail = parts.last;
  final headBudget = math.max(0, maxChars - tail.length - separator.length - 3);
  return '${path.substring(0, headBudget)}...$separator$tail';
}

String _renderCompactPath(
  String prefix,
  List<String> parts,
  String separator,
  int headCount,
  int tailCount,
) {
  final head = parts.take(headCount).join(separator);
  final tail = parts.skip(parts.length - tailCount).join(separator);
  final left = head.isNotEmpty
      ? '$prefix$head'
      : prefix.replaceFirst(RegExp(r'[\\\/]$'), '');
  return '$left$separator...$separator$tail';
}

class _SyncDisableState {
  _SyncDisableState({
    required this.busy,
    required this.unavailableSourceIds,
    required this.machineSyncingKeys,
    required this.runningSourceIds,
    required this.runningDestinationKeys,
    required this.compareKeys,
    required this.overlapRoots,
  });

  final bool busy;
  final Set<String> unavailableSourceIds;
  final Set<String> machineSyncingKeys;
  final Set<String> runningSourceIds;
  final Set<String> runningDestinationKeys;
  final Set<String> compareKeys;
  final List<_PathRoot> overlapRoots;

  bool get anySyncDisabled =>
      busy || machineSyncingKeys.isNotEmpty || overlapRoots.isNotEmpty;

  static _SyncDisableState from({
    required List<Map<String, dynamic>> sources,
    required Map<String, dynamic> runtimeStatus,
    required Map<String, dynamic> syncActivity,
    required List<dynamic> taskMachines,
    required bool busy,
    required Map<String, dynamic>? Function(
      String sourceId,
      String destinationId,
    )
    statusFor,
  }) {
    final machineSyncingKeys = <String>{};
    final unavailableSourceIds = <String>{};
    final runningSourceIds = <String>{};
    final runningDestinationKeys = <String>{};
    final compareKeys = <String>{};
    final overlapRoots = <_PathRoot>[];
    final activities = _activityRows(syncActivity, runtimeStatus);

    for (final source in sources) {
      final sourceId = _str(source['id']);
      for (final dst in _mapRefs(source['destinations'])) {
        if (_destinationUnavailable(statusFor(sourceId, _str(dst['id'])))) {
          unavailableSourceIds.add(sourceId);
          break;
        }
      }
    }

    void collectRuntime(Map<String, dynamic> runtime, String machineId) {
      if (_bool(runtime['syncing'])) {
        machineSyncingKeys.add(_machineKey(machineId));
        final sourceId = _str(runtime['source_id']);
        final destinationId = _str(runtime['destination_id']);
        if (sourceId.isNotEmpty) {
          runningSourceIds.add(sourceId);
          if (destinationId.isNotEmpty) {
            runningDestinationKeys.add('$sourceId/$destinationId');
          }
        }
      }
      for (final scan in _runtimeScans(runtime)) {
        final sourceId = _str(scan['source_id']);
        final destinationId = _str(scan['destination_id']);
        if (sourceId.isEmpty) continue;
        final source = _findSource(sources, sourceId);
        final dst = source == null
            ? null
            : _findDestination(source, destinationId);
        if (_str(scan['kind']) == 'compare') {
          if (destinationId.isNotEmpty) {
            compareKeys.add('$sourceId/$destinationId');
          }
        } else {
          _addOverlapRoots(overlapRoots, source, dst);
        }
      }
      final transfer = _map(runtime['transfer']);
      if (transfer.isNotEmpty) {
        final sourceId = _str(transfer['source_id']);
        final destinationId = _str(transfer['destination_id']);
        final source = _findSource(sources, sourceId);
        final dst = source == null
            ? null
            : _findDestination(source, destinationId);
        _addOverlapRoots(overlapRoots, source, dst);
      }
    }

    for (final activity in activities) {
      final runtime = _bool(activity['local'])
          ? runtimeStatus
          : _map(activity['runtime']);
      if (runtime.isEmpty) continue;
      collectRuntime(
        runtime,
        _bool(activity['local'])
            ? 'local'
            : _str(activity['machine_id'], _str(activity['label'], 'local')),
      );
    }

    for (final task in _runningTaskRows(taskMachines)) {
      final sourceId = _str(task['source_id']);
      if (sourceId.isEmpty) continue;
      final source = _findSource(sources, sourceId);
      if (source == null) continue;
      runningSourceIds.add(sourceId);
      final kind = _str(task['kind']);
      final destinationIds = _taskDestinationIds(task);
      for (final destinationId in destinationIds) {
        final key = '$sourceId/$destinationId';
        runningDestinationKeys.add(key);
        if (kind == 'compare') {
          compareKeys.add(key);
        } else {
          _addOverlapRoots(
            overlapRoots,
            source,
            _findDestination(source, destinationId),
          );
        }
      }
    }

    return _SyncDisableState(
      busy: busy,
      unavailableSourceIds: unavailableSourceIds,
      machineSyncingKeys: machineSyncingKeys,
      runningSourceIds: runningSourceIds,
      runningDestinationKeys: runningDestinationKeys,
      compareKeys: compareKeys,
      overlapRoots: overlapRoots,
    );
  }

  bool sourceDisabled(Map<String, dynamic> source) {
    if (busy) return true;
    if (unavailableSourceIds.contains(_str(source['id']))) return true;
    if (runningSourceIds.contains(_str(source['id']))) return true;
    if (machineSyncingKeys.contains(
      _machineKey(_str(source['machine_id'], 'local')),
    )) {
      return true;
    }
    return _sourceRoots(source).any(_overlapsActiveRoot);
  }

  bool destinationDisabled(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
    Map<String, dynamic>? status,
  ) {
    if (busy) return true;
    if (_destinationUnavailable(status)) return true;
    if (_bool(destination['paused'])) return true;
    final sourceId = _str(source['id']);
    final destinationId = _str(destination['id']);
    if (runningDestinationKeys.contains('$sourceId/$destinationId')) {
      return true;
    }
    if (compareKeys.contains('$sourceId/$destinationId')) return true;
    if (machineSyncingKeys.contains(
      _machineKey(_str(source['machine_id'], 'local')),
    )) {
      return true;
    }
    return _destinationRoots(source, destination).any(_overlapsActiveRoot);
  }

  bool destinationRunning(
    Map<String, dynamic> source,
    Map<String, dynamic> destination,
  ) {
    final key = '${_str(source['id'])}/${_str(destination['id'])}';
    return runningDestinationKeys.contains(key) || compareKeys.contains(key);
  }

  bool _overlapsActiveRoot(_PathRoot root) =>
      overlapRoots.any((active) => root.overlaps(active));
}

class _PathRoot {
  const _PathRoot(this.machineId, this.path);

  final String machineId;
  final String path;

  bool overlaps(_PathRoot other) {
    if (_machineKey(machineId) != _machineKey(other.machineId)) return false;
    if (path.isEmpty || other.path.isEmpty) return false;
    return path == other.path ||
        path.startsWith('${other.path}/') ||
        other.path.startsWith('$path/');
  }
}

List<Map<String, dynamic>> _activityRows(
  Map<String, dynamic> syncActivity,
  Map<String, dynamic> runtimeStatus,
) {
  final rows = _mapRefs(syncActivity['machines']).toList();
  final hasLocal = rows.any(
    (row) =>
        _bool(row['local']) || _machineKey(_str(row['machine_id'])) == 'local',
  );
  if (!hasLocal) {
    rows.add({
      'machine_id': 'local',
      'label': 'local',
      'local': true,
      'runtime': runtimeStatus,
    });
  }
  return rows;
}

List<Map<String, dynamic>> _runtimeScans(Map<String, dynamic> runtime) {
  final scans = _mapRefs(runtime['scans']);
  if (scans.isNotEmpty) return scans;
  final scan = _map(runtime['scan']);
  return scan.isEmpty ? const [] : [scan];
}

List<Map<String, dynamic>> _runningTaskRows(List<dynamic> taskMachines) {
  final rows = <Map<String, dynamic>>[];
  for (final machine in taskMachines) {
    for (final item in _list(_map(machine)['tasks'])) {
      final task = _map(item);
      if (_str(task['status']) == 'running') {
        rows.add(task);
      }
    }
  }
  return rows;
}

List<String> _taskDestinationIds(Map<String, dynamic> task) {
  return _str(
    task['destination_id'],
  ).split(',').map((id) => id.trim()).where((id) => id.isNotEmpty).toList();
}

Map<String, dynamic>? _findSource(
  List<Map<String, dynamic>> sources,
  String sourceId,
) {
  for (final source in sources) {
    if (_str(source['id']) == sourceId) return source;
  }
  return null;
}

Map<String, dynamic>? _findDestination(
  Map<String, dynamic> source,
  String destinationId,
) {
  for (final dst in _mapRefs(source['destinations'])) {
    if (_str(dst['id']) == destinationId) return dst;
  }
  return null;
}

void _addOverlapRoots(
  List<_PathRoot> roots,
  Map<String, dynamic>? source,
  Map<String, dynamic>? destination,
) {
  if (source == null) return;
  roots.addAll(_sourceRoots(source));
  if (destination != null) {
    roots.addAll(_destinationRoots(source, destination));
  }
}

List<_PathRoot> _sourceRoots(Map<String, dynamic> source) {
  return [
    _PathRoot(
      _str(source['machine_id'], 'local'),
      _pathOverlapKey(_str(source['src'])),
    ),
  ];
}

List<_PathRoot> _destinationRoots(
  Map<String, dynamic> source,
  Map<String, dynamic> destination,
) {
  return [
    ..._sourceRoots(source),
    _PathRoot(
      _str(destination['machine_id'], 'local'),
      _pathOverlapKey(_str(destination['path'])),
    ),
  ];
}

String _pathOverlapKey(String value) {
  var path = _displayPath(value).trim().replaceAll('\\', '/');
  path = path.replaceAll(RegExp(r'/+'), '/');
  if (path.length > 1) path = path.replaceFirst(RegExp(r'/+$'), '');
  return path.toLowerCase();
}

String _machineKey(String value) {
  final key = value.trim().toLowerCase();
  return key.isEmpty ? 'local' : key;
}

bool _destinationUnavailable(Map<String, dynamic>? status) {
  if (status == null || _str(status['status']) != 'red') return false;
  final reason = _str(status['status_reason']).toLowerCase();
  return const [
    'source path does not exist',
    'source path is not a directory',
    'source offline',
    'source unavailable',
    'destination path does not exist',
    'destination path is not a directory',
    'destination file path is a directory',
    'destination file path has no parent',
    'destination is not writable',
    'destination offline',
    'destination unavailable',
    'no such file or directory',
    'permission denied',
    'read-only file system',
    'transport endpoint is not connected',
    'stale file handle',
    'input/output error',
  ].any(reason.contains);
}

Map<String, dynamic> _defaultDestinationSchedule() => {
  'mode': 'realtime',
  'time': '19:00',
  'timezone': 'local',
  'weekday': 'monday',
  'every_weeks': 1,
  'anchor_date': '2026-01-05',
  'sync_current_cycle_manually': false,
};

Map<String, dynamic> _cloneSchedule(Map<String, dynamic> schedule) {
  final defaults = _defaultDestinationSchedule();
  final mode = _str(schedule['mode'], _str(defaults['mode']));
  return {
    ...defaults,
    ...schedule,
    'mode': mode == 'weekly' || mode == 'daily' || mode == 'realtime'
        ? mode
        : _str(defaults['mode']),
    'time': _normalizeScheduleTime(
      _str(schedule['time'], _str(defaults['time'])),
    ),
    'timezone': 'local',
    'weekday': _normalizeWeekday(
      _str(schedule['weekday'], _str(defaults['weekday'])),
    ),
    'every_weeks': _normalizeScheduleEveryWeeks(schedule['every_weeks']),
    'anchor_date': _normalizeScheduleAnchorDate(
      _str(schedule['anchor_date'], _str(defaults['anchor_date'])),
    ),
    'sync_current_cycle_manually': false,
  };
}

String _normalizeScheduleTime(String value) {
  final match = RegExp(r'^(\d{1,2}):(\d{2})(?::\d{2})?$').firstMatch(value);
  if (match == null) return '19:00';
  final hour = math.min(23, int.tryParse(match.group(1) ?? '') ?? 19);
  final minute = math.min(59, int.tryParse(match.group(2) ?? '') ?? 0);
  return '${hour.toString().padLeft(2, '0')}:${minute.toString().padLeft(2, '0')}';
}

String _normalizeWeekday(String value) {
  const weekdays = {
    'monday',
    'tuesday',
    'wednesday',
    'thursday',
    'friday',
    'saturday',
    'sunday',
  };
  final lower = value.trim().toLowerCase();
  return weekdays.contains(lower) ? lower : 'monday';
}

int _normalizeScheduleEveryWeeks(dynamic value) {
  final parsed = int.tryParse('$value') ?? 1;
  return parsed.clamp(1, 52);
}

String _normalizeScheduleAnchorDate(String value) {
  final text = value.trim();
  final parsed = DateTime.tryParse(text);
  if (parsed == null) return '2026-01-05';
  return '${parsed.year.toString().padLeft(4, '0')}-'
      '${parsed.month.toString().padLeft(2, '0')}-'
      '${parsed.day.toString().padLeft(2, '0')}';
}

String _scheduleModeLabel(String value) => switch (value) {
  'realtime' => 'Realtime',
  'daily' => 'Daily',
  'weekly' => 'Weekly',
  'manual' => 'Manual',
  _ => value,
};

String _weekdayLabel(String value) => switch (value) {
  'monday' => 'Monday',
  'tuesday' => 'Tuesday',
  'wednesday' => 'Wednesday',
  'thursday' => 'Thursday',
  'friday' => 'Friday',
  'saturday' => 'Saturday',
  'sunday' => 'Sunday',
  _ => value,
};

String _scheduleLabel(Map<String, dynamic> schedule) {
  final next = _cloneSchedule(schedule);
  final mode = _str(next['mode'], 'realtime');
  final time = _str(next['time'], '19:00');
  final weekday = _str(next['weekday'], 'monday');
  if (mode == 'weekly') {
    final every = _normalizeScheduleEveryWeeks(next['every_weeks']);
    const labels = {
      'monday': 'Mon',
      'tuesday': 'Tue',
      'wednesday': 'Wed',
      'thursday': 'Thu',
      'friday': 'Fri',
      'saturday': 'Sat',
      'sunday': 'Sun',
    };
    final prefix = every > 1 ? 'Every ${every}w ' : '';
    return '$prefix${labels[weekday] ?? weekday} $time';
  }
  if (mode == 'realtime') {
    return 'Realtime';
  }
  return time;
}

String _cycleDisplay(Map<String, dynamic>? status) {
  if (status == null) {
    return '-';
  }
  final verified = _str(status['last_verified_cycle_id'], '-');
  final target = _str(
    status['target_cycle_id'],
    _str(status['latest_closed_cycle_id'], '-'),
  );
  return '$verified / $target';
}

String _sourceLatestCycle(Iterable<Map<String, dynamic>?> statuses) {
  final cycles = <int>[];
  for (final status in statuses) {
    final value = status?['latest_closed_cycle_id'];
    if (value is int) {
      cycles.add(value);
    } else if (value is num) {
      cycles.add(value.round());
    } else {
      final parsed = int.tryParse('$value');
      if (parsed != null) {
        cycles.add(parsed);
      }
    }
  }
  if (cycles.isEmpty) {
    return '-';
  }
  cycles.sort();
  return '${cycles.last}';
}

Color _statusColor(Map<String, dynamic>? status) {
  final value = _str(status?['status']).toLowerCase();
  if (value == 'green' || value == 'ok' || value == 'verified') {
    return Palette.green;
  }
  if (value == 'yellow' ||
      value.contains('changing') ||
      value.contains('paused')) {
    return const Color(0xffd99a00);
  }
  return Palette.red;
}

// Kept temporarily while the native UI is being matched against the master
// layout. The master-like renderer above is the active path.
// ignore: unused_element
class _SourceCard extends StatelessWidget {
  const _SourceCard({
    required this.source,
    required this.machineIds,
    required this.machineLabel,
    required this.statusFor,
    required this.onChanged,
    required this.onMutate,
    required this.onSyncSource,
    required this.onSyncDestination,
    required this.onScan,
    required this.onCancel,
    required this.onRemoveSource,
  });

  final Map<String, dynamic> source;
  final List<String> machineIds;
  final String Function(String id) machineLabel;
  final Map<String, dynamic>? Function(String sourceId, String destinationId)
  statusFor;
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
  final void Function(String id) onSyncSource;
  final void Function(String sourceId, String destinationId, String mode)
  onSyncDestination;
  final void Function(String sourceId, String destinationId) onScan;
  final void Function(String sourceId, String destinationId) onCancel;
  final void Function(String sourceId) onRemoveSource;

  @override
  Widget build(BuildContext context) {
    source['destinations'] = _list(source['destinations']);
    source['excludes'] = _list(source['excludes']);
    final sourceId = _str(source['id'], 'source');
    final destinations = _mapRefs(source['destinations']);
    return Section(
      title: sourceId,
      trailing: Wrap(
        spacing: 6,
        runSpacing: 6,
        children: [
          StatusPill(
            text: _bool(source['enabled'], true) ? 'enabled' : 'disabled',
            color: _bool(source['enabled'], true)
                ? Palette.green
                : Palette.muted,
          ),
          StatusPill(
            text: '${destinations.length} destinations',
            color: Palette.accent,
          ),
          MiniButton(
            icon: Icons.play_arrow_outlined,
            label: 'Sync',
            onTap: () => onSyncSource(sourceId),
          ),
          MiniButton(
            icon: Icons.add,
            label: 'Destination',
            onTap: () => onMutate(() {
              final next = 'dst_${destinations.length + 1}';
              _list(source['destinations']).add({
                'id': next,
                'machine_id': 'local',
                'path': '',
                'enabled': true,
                'schedule': {
                  'mode': 'daily',
                  'time': '10:00',
                  'timezone': 'local',
                  'weekday': 'saturday',
                  'sync_current_cycle_manually': false,
                },
              });
            }),
          ),
          MiniButton(
            icon: Icons.delete_outline,
            label: 'Remove',
            danger: true,
            onTap: () => onRemoveSource(sourceId),
          ),
        ],
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Wrap(
            spacing: 10,
            runSpacing: 10,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              SizedBox(
                width: 150,
                child: CommitField(
                  label: 'Source ID',
                  value: sourceId,
                  onCommit: (value) {
                    source['id'] = value;
                    onChanged();
                  },
                ),
              ),
              SizedBox(
                width: 150,
                child: EnumField(
                  label: 'Machine',
                  value: _str(source['machine_id'], 'local'),
                  values: machineIds,
                  labelOf: machineLabel,
                  onChanged: (value) =>
                      onMutate(() => source['machine_id'] = value),
                ),
              ),
              SizedBox(
                width: 420,
                child: CommitField(
                  label: 'Source path',
                  value: _str(source['src']),
                  onCommit: (value) {
                    source['src'] = value;
                    onChanged();
                  },
                ),
              ),
              SizedBox(
                width: 130,
                child: EnumField(
                  label: 'Mode',
                  value: _str(source['mode'], 'mirror'),
                  values: const ['mirror', 'copy'],
                  onChanged: (value) => onMutate(() => source['mode'] = value),
                ),
              ),
              LabeledSwitch(
                label: 'Enabled',
                value: _bool(source['enabled'], true),
                onChanged: (value) => onMutate(() => source['enabled'] = value),
              ),
              LabeledSwitch(
                label: 'Add directory',
                value: _bool(source['add_directory'], false),
                onChanged: (value) =>
                    onMutate(() => source['add_directory'] = value),
              ),
              MiniButton(
                icon: Icons.block_outlined,
                label: _excludeButtonLabel(source['excludes']),
                onTap: () => _showExcludes(context, source, onChanged),
              ),
            ],
          ),
          const SizedBox(height: 12),
          if (destinations.isEmpty)
            const SizedBox.shrink()
          else
            Column(
              children: destinations
                  .map(
                    (dst) => _DestinationRow(
                      sourceId: sourceId,
                      destination: dst,
                      destinations: _list(source['destinations']),
                      machineIds: machineIds,
                      machineLabel: machineLabel,
                      status: statusFor(sourceId, _str(dst['id'])),
                      onChanged: onChanged,
                      onMutate: onMutate,
                      onSync: onSyncDestination,
                      onScan: onScan,
                      onCancel: onCancel,
                    ),
                  )
                  .toList(),
            ),
        ],
      ),
    );
  }

  Future<void> _showExcludes(
    BuildContext context,
    Map<String, dynamic> source,
    Future<void> Function([String label]) onChanged,
  ) async {
    await showDialog<void>(
      context: context,
      builder: (context) => _ExcludedDialog(
        sourceId: _str(source['id'], 'source'),
        sourcePath: _machinePath(
          _str(source['machine_id'], 'local'),
          _str(source['src'], '-'),
        ),
        initialItems: _list(source['excludes']).map((item) => '$item').toList(),
        onChanged: (items) async {
          source['excludes'] = items;
          await onChanged('Excludes saved');
        },
      ),
    );
  }
}

class _DestinationRow extends StatelessWidget {
  const _DestinationRow({
    required this.sourceId,
    required this.destination,
    required this.destinations,
    required this.machineIds,
    required this.machineLabel,
    required this.status,
    required this.onChanged,
    required this.onMutate,
    required this.onSync,
    required this.onScan,
    required this.onCancel,
  });

  final String sourceId;
  final Map<String, dynamic> destination;
  final List<dynamic> destinations;
  final List<String> machineIds;
  final String Function(String id) machineLabel;
  final Map<String, dynamic>? status;
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
  final void Function(String sourceId, String destinationId, String mode)
  onSync;
  final void Function(String sourceId, String destinationId) onScan;
  final void Function(String sourceId, String destinationId) onCancel;

  @override
  Widget build(BuildContext context) {
    destination['schedule'] = _map(destination['schedule']);
    final schedule = destination['schedule'] as Map<String, dynamic>;
    final destinationId = _str(destination['id'], 'destination');
    final state = _str(status?['status'], 'unknown');
    final issues = _list(status?['issues']);
    final diffs = _map(status?['scan_differences']);
    final diffTotal = diffs.values.fold<int>(
      0,
      (sum, value) => sum + _int(value),
    );
    return Container(
      margin: const EdgeInsets.only(top: 8),
      padding: const EdgeInsets.all(10),
      decoration: BoxDecoration(
        color: const Color(0xfffbfcfe),
        border: Border.all(color: Palette.line),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Icon(
                state == 'ok' ? Icons.check_circle : Icons.error_outline,
                color: state == 'ok'
                    ? Palette.green
                    : (issues.isEmpty ? Palette.warn : Palette.red),
                size: 18,
              ),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  '$destinationId -> ${_str(destination['path'], '-')}',
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(fontWeight: FontWeight.w700),
                ),
              ),
              StatusPill(
                text: state,
                color: state == 'ok' ? Palette.green : Palette.warn,
              ),
              const SizedBox(width: 6),
              _MasterIconMenuButton(
                icon: Icons.play_arrow_outlined,
                tooltip: 'Sync',
                options: const ['incremental', 'full', 'repair'],
                labelFor: (mode) => switch (mode) {
                  'incremental' => 'Incremental',
                  'full' => 'Full',
                  'repair' => 'Repair',
                  _ => mode,
                },
                onSelected: (mode) => onSync(sourceId, destinationId, mode),
              ),
              IconButton(
                tooltip: 'Compare',
                icon: const Icon(Icons.compare_arrows_outlined, size: 20),
                onPressed: () => onScan(sourceId, destinationId),
              ),
              IconButton(
                tooltip: 'Cancel',
                icon: const Icon(Icons.stop_circle_outlined, size: 20),
                onPressed: () => onCancel(sourceId, destinationId),
              ),
              IconButton(
                tooltip: 'Sync settings',
                icon: const Icon(Icons.tune, size: 20),
                onPressed: () => _openSyncDialog(context),
              ),
              IconButton(
                tooltip: 'Remove',
                icon: const Icon(Icons.delete_outline, size: 20),
                color: Palette.red,
                onPressed: () =>
                    onMutate(() => destinations.remove(destination)),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Wrap(
            spacing: 10,
            runSpacing: 10,
            crossAxisAlignment: WrapCrossAlignment.center,
            children: [
              SizedBox(
                width: 140,
                child: CommitField(
                  label: 'Destination ID',
                  value: destinationId,
                  onCommit: (value) {
                    destination['id'] = value;
                    onChanged();
                  },
                ),
              ),
              SizedBox(
                width: 140,
                child: EnumField(
                  label: 'Machine',
                  value: _str(destination['machine_id'], 'local'),
                  values: machineIds,
                  labelOf: machineLabel,
                  onChanged: (value) =>
                      onMutate(() => destination['machine_id'] = value),
                ),
              ),
              SizedBox(
                width: 360,
                child: CommitField(
                  label: 'Path',
                  value: _str(destination['path']),
                  onCommit: (value) {
                    destination['path'] = value;
                    onChanged();
                  },
                ),
              ),
              SizedBox(
                width: 120,
                child: EnumField(
                  label: 'Schedule',
                  value: _str(schedule['mode'], 'daily'),
                  values: const ['realtime', 'daily', 'weekly', 'manual'],
                  onChanged: (value) =>
                      onMutate(() => schedule['mode'] = value),
                ),
              ),
              SizedBox(
                width: 98,
                child: CommitField(
                  label: 'Time',
                  value: _str(schedule['time'], '10:00'),
                  onCommit: (value) {
                    schedule['time'] = value;
                    onChanged();
                  },
                ),
              ),
              SizedBox(
                width: 130,
                child: EnumField(
                  label: 'Weekday',
                  value: _str(schedule['weekday'], 'saturday'),
                  values: const [
                    'monday',
                    'tuesday',
                    'wednesday',
                    'thursday',
                    'friday',
                    'saturday',
                    'sunday',
                  ],
                  onChanged: (value) =>
                      onMutate(() => schedule['weekday'] = value),
                ),
              ),
              LabeledSwitch(
                label: 'Enabled',
                value: _bool(destination['enabled'], true),
                onChanged: (value) =>
                    onMutate(() => destination['enabled'] = value),
              ),
              LabeledSwitch(
                label: 'Manual cycle',
                value: _bool(schedule['sync_current_cycle_manually'], false),
                onChanged: (value) => onMutate(
                  () => schedule['sync_current_cycle_manually'] = value,
                ),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Wrap(
            spacing: 8,
            runSpacing: 6,
            children: [
              StatusPill(
                text:
                    'cycle ${_str(status?['last_verified_cycle_id'], '-')} / ${_str(status?['target_cycle_id'], '-')}',
                color: Palette.muted,
              ),
              if (diffTotal > 0)
                StatusPill(text: 'diff $diffTotal', color: Palette.warn),
              for (final issue in issues.take(4))
                StatusPill(text: _str(issue), color: Palette.red),
            ],
          ),
        ],
      ),
    );
  }

  Future<void> _openSyncDialog(BuildContext context) async {
    final sync = Map<String, dynamic>.from(_map(destination['sync']));
    final result = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) => _SyncSettingsDialog(sync: sync),
    );
    if (result != null) {
      destination['sync'] = result;
      await onChanged('Destination settings saved');
    }
  }
}

class _SettingsDialog extends StatefulWidget {
  const _SettingsDialog({required this.app, required this.sync});

  final Map<String, dynamic> app;
  final Map<String, dynamic> sync;

  @override
  State<_SettingsDialog> createState() => _SettingsDialogState();
}

class _SettingsDialogState extends State<_SettingsDialog> {
  late final TextEditingController timeout = TextEditingController(
    text: _str(widget.sync['transfer_timeout_secs'], '120'),
  );
  late final TextEditingController bwlimit = TextEditingController(
    text: _str(widget.sync['bwlimit_kbps'], '0'),
  );
  late final TextEditingController pool = TextEditingController(
    text: _str(widget.app['tcp_connection_pool_size'], '100'),
  );

  @override
  void dispose() {
    timeout.dispose();
    bwlimit.dispose();
    pool.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: 'Settings',
      width: 760,
      maxHeight: 720,
      shrinkWrap: true,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          _SettingsGrid(
            children: [
              _SettingsSection(
                title: 'Sync',
                children: [
                  _SettingsCheckRow(
                    label: 'Mirror',
                    value: _bool(widget.sync['mirror'], true),
                    onChanged: (value) =>
                        setState(() => widget.sync['mirror'] = value),
                  ),
                  _SettingsCheckRow(
                    label: 'Checksum',
                    value: _bool(widget.sync['checksum'], false),
                    onChanged: (value) =>
                        setState(() => widget.sync['checksum'] = value),
                  ),
                  _SettingsCheckRow(
                    label: 'ZFS diff',
                    value: _bool(widget.sync['zfs_diff'], true),
                    onChanged: (value) =>
                        setState(() => widget.sync['zfs_diff'] = value),
                  ),
                  _SettingsCheckRow(
                    label: 'Debug logs',
                    value: _bool(widget.sync['debug_logs'], false),
                    onChanged: (value) =>
                        setState(() => widget.sync['debug_logs'] = value),
                  ),
                ],
              ),
              _SettingsSection(
                title: 'Limits',
                children: [
                  _SettingsNumberField(
                    label: 'Transfer timeout seconds',
                    controller: timeout,
                  ),
                  _SettingsNumberField(
                    label: 'Bandwidth KB/s',
                    controller: bwlimit,
                  ),
                  _SettingsNumberField(
                    label: 'TCP connection pool',
                    controller: pool,
                  ),
                ],
              ),
              _SettingsSection(
                title: 'Application',
                children: [
                  _SettingsCheckRow(
                    label: 'Start on boot',
                    value: _bool(widget.app['autostart'], false),
                    onChanged: (value) =>
                        setState(() => widget.app['autostart'] = value),
                  ),
                  _SettingsCheckRow(
                    label: 'Close button minimizes to tray',
                    value: _bool(widget.app['close_to_tray'], true),
                    onChanged: (value) =>
                        setState(() => widget.app['close_to_tray'] = value),
                  ),
                ],
              ),
            ],
          ),
          const SizedBox(height: 12),
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              MasterButton(
                label: 'Save',
                width: 72,
                primary: true,
                onTap: () {
                  widget.app['tcp_connection_pool_size'] =
                      int.tryParse(pool.text) ?? 100;
                  widget.sync['transfer_timeout_secs'] =
                      int.tryParse(timeout.text) ?? 120;
                  widget.sync['bwlimit_kbps'] = int.tryParse(bwlimit.text) ?? 0;
                  widget.app['sync'] = widget.sync;
                  Navigator.pop(context, widget.app);
                },
              ),
            ],
          ),
        ],
      ),
    );
  }
}

class _SettingsGrid extends StatelessWidget {
  const _SettingsGrid({required this.children});

  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        if (constraints.maxWidth < 560) {
          return Column(
            children: [
              for (var i = 0; i < children.length; i += 1) ...[
                if (i > 0) const SizedBox(height: 12),
                children[i],
              ],
            ],
          );
        }
        final rows = <Widget>[];
        for (var i = 0; i < children.length; i += 2) {
          rows.add(
            IntrinsicHeight(
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Expanded(child: children[i]),
                  const SizedBox(width: 12),
                  Expanded(
                    child: i + 1 < children.length
                        ? children[i + 1]
                        : const SizedBox.shrink(),
                  ),
                ],
              ),
            ),
          );
        }
        return Column(
          children: [
            for (var i = 0; i < rows.length; i += 1) ...[
              if (i > 0) const SizedBox(height: 12),
              rows[i],
            ],
          ],
        );
      },
    );
  }
}

class _SettingsSection extends StatelessWidget {
  const _SettingsSection({required this.title, required this.children});

  final String title;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: const Color(0xfff8fafc),
        border: Border.all(color: Palette.line),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            title,
            style: const TextStyle(fontSize: 13, fontWeight: FontWeight.w700),
          ),
          const SizedBox(height: 8),
          ...children,
        ],
      ),
    );
  }
}

class _SettingsCheckRow extends StatelessWidget {
  const _SettingsCheckRow({
    required this.label,
    required this.value,
    required this.onChanged,
  });

  final String label;
  final bool value;
  final ValueChanged<bool> onChanged;

  @override
  Widget build(BuildContext context) {
    return InkWell(
      onTap: () => onChanged(!value),
      child: SizedBox(
        height: 30,
        child: Row(
          children: [
            SizedBox(
              width: 16,
              height: 16,
              child: Checkbox(
                value: value,
                onChanged: (next) => onChanged(next ?? false),
                materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
                visualDensity: VisualDensity.compact,
              ),
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                label,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(fontSize: 13, color: Palette.text),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _SettingsNumberField extends StatelessWidget {
  const _SettingsNumberField({required this.label, required this.controller});

  final String label;
  final TextEditingController controller;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.only(top: 8),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            label,
            style: const TextStyle(color: Palette.muted, fontSize: 12),
          ),
          const SizedBox(height: 4),
          _SettingsNumberInput(controller: controller),
        ],
      ),
    );
  }
}

class _SettingsNumberInput extends StatelessWidget {
  const _SettingsNumberInput({required this.controller});

  final TextEditingController controller;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: _settingsInputHeight,
      child: TextFormField(
        controller: controller,
        keyboardType: TextInputType.number,
        style: const TextStyle(fontSize: 14, color: Palette.text, height: 1.2),
        decoration: const InputDecoration(
          isDense: true,
          contentPadding: EdgeInsets.symmetric(horizontal: 10, vertical: 9),
          constraints: BoxConstraints.tightFor(height: _settingsInputHeight),
        ),
      ),
    );
  }
}

class _SyncSettingsDialog extends StatefulWidget {
  const _SyncSettingsDialog({required this.sync, this.inherited = false});

  final Map<String, dynamic> sync;
  final bool inherited;

  @override
  State<_SyncSettingsDialog> createState() => _SyncSettingsDialogState();
}

class _SyncSettingsDialogState extends State<_SyncSettingsDialog> {
  late final TextEditingController timeout = TextEditingController(
    text: _str(widget.sync['transfer_timeout_secs'], '120'),
  );
  late final TextEditingController bwlimit = TextEditingController(
    text: _str(widget.sync['bwlimit_kbps'], '0'),
  );

  @override
  void dispose() {
    timeout.dispose();
    bwlimit.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: 'Destination Sync',
      width: 760,
      maxHeight: 720,
      shrinkWrap: true,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          _SettingsGrid(
            children: [
              _SettingsSection(
                title: 'Sync',
                children: [
                  _SettingsCheckRow(
                    label: 'Mirror',
                    value: _bool(widget.sync['mirror'], true),
                    onChanged: (value) =>
                        setState(() => widget.sync['mirror'] = value),
                  ),
                  _SettingsCheckRow(
                    label: 'Checksum',
                    value: _bool(widget.sync['checksum'], false),
                    onChanged: (value) =>
                        setState(() => widget.sync['checksum'] = value),
                  ),
                  _SettingsCheckRow(
                    label: 'ZFS diff',
                    value: _bool(widget.sync['zfs_diff'], true),
                    onChanged: (value) =>
                        setState(() => widget.sync['zfs_diff'] = value),
                  ),
                  _SettingsCheckRow(
                    label: 'Debug logs',
                    value: _bool(widget.sync['debug_logs'], false),
                    onChanged: (value) =>
                        setState(() => widget.sync['debug_logs'] = value),
                  ),
                ],
              ),
              _SettingsSection(
                title: 'Limits',
                children: [
                  _SettingsNumberField(
                    label: 'Transfer timeout seconds',
                    controller: timeout,
                  ),
                  _SettingsNumberField(
                    label: 'Bandwidth KB/s',
                    controller: bwlimit,
                  ),
                ],
              ),
            ],
          ),
          const SizedBox(height: 12),
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              MasterButton(
                label: 'Use Global',
                width: 96,
                onTap: widget.inherited
                    ? null
                    : () => Navigator.pop(context, <String, dynamic>{}),
              ),
              const SizedBox(width: 8),
              MasterButton(
                label: 'Save',
                width: 72,
                primary: true,
                onTap: () {
                  widget.sync['transfer_timeout_secs'] =
                      int.tryParse(timeout.text) ?? 120;
                  widget.sync['bwlimit_kbps'] = int.tryParse(bwlimit.text) ?? 0;
                  Navigator.pop(context, widget.sync);
                },
              ),
            ],
          ),
        ],
      ),
    );
  }
}

class _ScheduleDialog extends StatefulWidget {
  const _ScheduleDialog({required this.schedule});

  final Map<String, dynamic> schedule;

  @override
  State<_ScheduleDialog> createState() => _ScheduleDialogState();
}

class _ScheduleDialogState extends State<_ScheduleDialog> {
  late String mode = _str(widget.schedule['mode'], 'realtime');
  late String weekday = _normalizeWeekday(_str(widget.schedule['weekday']));
  late final TextEditingController time = TextEditingController(
    text: _normalizeScheduleTime(_str(widget.schedule['time'], '19:00')),
  );
  late final TextEditingController everyWeeks = TextEditingController(
    text: '${_normalizeScheduleEveryWeeks(widget.schedule['every_weeks'])}',
  );
  late final TextEditingController anchorDate = TextEditingController(
    text: _normalizeScheduleAnchorDate(
      _str(widget.schedule['anchor_date'], '2026-01-05'),
    ),
  );

  @override
  void dispose() {
    time.dispose();
    everyWeeks.dispose();
    anchorDate.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final scheduled = mode != 'realtime';
    return _MasterDialogFrame(
      title: 'Schedule',
      width: 420,
      maxHeight: mode == 'weekly' ? 430 : 270,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const _MasterLabel('Mode'),
          SizedBox(
            height: _masterControlHeight,
            child: _MasterMenuSelect(
              value: mode,
              options: const ['realtime', 'daily', 'weekly'],
              labelFor: _scheduleModeLabel,
              onSelected: (value) => setState(() => mode = value),
            ),
          ),
          if (scheduled) ...[
            const SizedBox(height: 10),
            const _MasterLabel('Time'),
            _CompactInput(controller: time, placeholder: '19:00'),
          ],
          if (mode == 'weekly') ...[
            const SizedBox(height: 10),
            const _MasterLabel('Weekday'),
            SizedBox(
              height: _masterControlHeight,
              child: _MasterMenuSelect(
                value: weekday,
                options: const [
                  'monday',
                  'tuesday',
                  'wednesday',
                  'thursday',
                  'friday',
                  'saturday',
                  'sunday',
                ],
                labelFor: _weekdayLabel,
                onSelected: (value) => setState(() => weekday = value),
              ),
            ),
            const SizedBox(height: 10),
            const _MasterLabel('Every weeks'),
            _CompactInput(controller: everyWeeks, placeholder: '1'),
            const SizedBox(height: 10),
            const _MasterLabel('Anchor date'),
            _CompactInput(controller: anchorDate, placeholder: '2026-01-05'),
          ],
          const Spacer(),
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              MasterButton(
                label: 'Apply',
                width: 72,
                primary: true,
                onTap: () => Navigator.pop(
                  context,
                  _cloneSchedule({
                    'mode': mode,
                    'time': scheduled ? time.text : '19:00',
                    'weekday': weekday,
                    'every_weeks': mode == 'weekly' ? everyWeeks.text : 1,
                    'anchor_date': mode == 'weekly'
                        ? anchorDate.text
                        : '2026-01-05',
                  }),
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }
}

class _MasterDialogFrame extends StatelessWidget {
  const _MasterDialogFrame({
    required this.title,
    required this.width,
    required this.maxHeight,
    required this.child,
    this.shrinkWrap = false,
  });

  final String title;
  final double width;
  final double maxHeight;
  final Widget child;
  final bool shrinkWrap;

  @override
  Widget build(BuildContext context) {
    final size = MediaQuery.sizeOf(context);
    final panelWidth = math.max(320.0, math.min(width, size.width - 48));
    final panelHeight = math.max(240.0, math.min(maxHeight, size.height - 48));
    return Dialog(
      insetPadding: const EdgeInsets.all(24),
      backgroundColor: Colors.transparent,
      child: ConstrainedBox(
        constraints: BoxConstraints(
          minWidth: panelWidth,
          maxWidth: panelWidth,
          maxHeight: panelHeight,
        ),
        child: SizedBox(
          height: shrinkWrap ? null : panelHeight,
          child: Container(
            padding: const EdgeInsets.all(14),
            decoration: BoxDecoration(
              color: Palette.panel,
              border: Border.all(color: Palette.line),
              borderRadius: BorderRadius.circular(8),
            ),
            child: Column(
              mainAxisSize: shrinkWrap ? MainAxisSize.min : MainAxisSize.max,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: Text(
                        title,
                        style: const TextStyle(
                          fontSize: 18,
                          fontWeight: FontWeight.w700,
                        ),
                      ),
                    ),
                    MasterButton(
                      label: 'x',
                      square: true,
                      onTap: () => Navigator.pop(context),
                    ),
                  ],
                ),
                const SizedBox(height: 10),
                if (shrinkWrap)
                  Flexible(child: child)
                else
                  Expanded(child: child),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class _MasterPre extends StatelessWidget {
  const _MasterPre({
    required this.text,
    this.minHeight = 160,
    this.maxHeight = 520,
  });

  final String text;
  final double minHeight;
  final double maxHeight;

  @override
  Widget build(BuildContext context) {
    return Container(
      constraints: BoxConstraints(minHeight: minHeight, maxHeight: maxHeight),
      width: double.infinity,
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: const Color(0xfff8fafc),
        border: Border.all(color: Palette.line),
        borderRadius: BorderRadius.circular(6),
      ),
      child: SingleChildScrollView(
        child: SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: Text(
            text,
            style: const TextStyle(
              fontFamily: 'Consolas',
              fontSize: 12,
              height: 1.5,
              color: Palette.text,
            ),
          ),
        ),
      ),
    );
  }
}

class _PathPickerDialog extends StatefulWidget {
  const _PathPickerDialog({
    required this.api,
    required this.machineIds,
    required this.machineLabel,
    required this.initialMachineId,
    required this.initialPath,
    required this.showAddDirectory,
    required this.initialAddDirectory,
    this.validate,
  });

  final AutoSyncApi api;
  final List<String> machineIds;
  final String Function(String id) machineLabel;
  final String initialMachineId;
  final String initialPath;
  final bool showAddDirectory;
  final bool initialAddDirectory;
  final String? Function(Map<String, dynamic> selection)? validate;

  @override
  State<_PathPickerDialog> createState() => _PathPickerDialogState();
}

class _PathPickerDialogState extends State<_PathPickerDialog> {
  late String machineId = widget.initialMachineId;
  late String path = widget.initialPath;
  late String selectedPath = widget.initialPath;
  String? parent;
  String error = '';
  bool loading = true;
  late bool addDirectory = widget.initialAddDirectory;
  List<Map<String, dynamic>> entries = [];

  @override
  void initState() {
    super.initState();
    _load(path);
  }

  Future<void> _load(String nextPath) async {
    if (nextPath != path) {
      selectedPath = '';
    }
    setState(() {
      loading = true;
      error = '';
    });
    try {
      final res = await widget.api.browsePaths(
        path: nextPath,
        machineId: machineId,
      );
      if (!mounted) return;
      setState(() {
        path = _str(res['path'], nextPath);
        parent = res['parent'] == null ? null : _str(res['parent']);
        entries = _mapRefs(res['entries']);
        loading = false;
      });
    } catch (err) {
      if (!mounted) return;
      setState(() {
        error = '$err';
        loading = false;
      });
    }
  }

  void _choose(String chosenPath) {
    final selection = {
      'machine_id': machineId,
      'path': chosenPath,
      'add_directory': addDirectory,
    };
    final validation = widget.validate?.call(selection);
    if (validation != null && validation.isNotEmpty) {
      setState(() => error = validation);
      return;
    }
    Navigator.pop(context, selection);
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: 'Path',
      width: 720,
      maxHeight: 480,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              _MachineSelectButton(
                width: 160,
                value: machineId,
                options: widget.machineIds,
                labelFor: widget.machineLabel,
                onSelected: (value) {
                  setState(() {
                    machineId = value;
                    selectedPath = '';
                  });
                  _load(
                    value == 'local' && hostPlatformIsWindows ? r'C:\' : '/',
                  );
                },
              ),
              const SizedBox(width: 8),
              Expanded(child: _FolderPathLabel(path)),
            ],
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              MasterButton(
                label: 'Up',
                width: 64,
                onTap: parent == null ? null : () => _load(parent!),
              ),
              const SizedBox(width: 8),
              MasterButton(
                label: 'Choose',
                width: 82,
                primary: true,
                onTap: () =>
                    _choose(selectedPath.isEmpty ? path : selectedPath),
              ),
              if (widget.showAddDirectory) ...[
                const SizedBox(width: 12),
                _CheckCell(
                  value: addDirectory,
                  label: 'Add containing directory',
                  onChanged: (value) => setState(() => addDirectory = value),
                ),
              ],
            ],
          ),
          if (error.isNotEmpty) ...[
            const SizedBox(height: 8),
            Text(error, style: const TextStyle(color: Palette.red)),
          ],
          const SizedBox(height: 8),
          Expanded(
            child: _FileListView(
              loading: loading,
              entries: entries,
              isDir: (entry) => _str(entry['kind']) == 'dir',
              isSelected: (itemPath) => selectedPath == itemPath,
              onSelected: (itemPath, value) =>
                  setState(() => selectedPath = value ? itemPath : ''),
              onOpenDir: _load,
            ),
          ),
        ],
      ),
    );
  }
}

class _FolderPathLabel extends StatelessWidget {
  const _FolderPathLabel(this.path);

  final String path;

  @override
  Widget build(BuildContext context) {
    return Container(
      height: _masterControlHeight,
      alignment: Alignment.centerLeft,
      padding: const EdgeInsets.symmetric(horizontal: 10),
      decoration: BoxDecoration(
        color: const Color(0xfff8fafc),
        border: Border.all(color: Palette.line),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Text(
        _displayPath(path),
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
        style: const TextStyle(fontFamily: 'Consolas', fontSize: 12),
      ),
    );
  }
}

class _MachineSelectButton extends StatelessWidget {
  const _MachineSelectButton({
    required this.width,
    required this.value,
    required this.options,
    required this.labelFor,
    required this.onSelected,
  });

  final double width;
  final String value;
  final List<String> options;
  final String Function(String id) labelFor;
  final ValueChanged<String> onSelected;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: width,
      height: _masterControlHeight,
      child: _MasterMenuSelect(
        value: value,
        options: options,
        labelFor: labelFor,
        onSelected: onSelected,
      ),
    );
  }
}

class _FileListEntryLabel extends StatelessWidget {
  const _FileListEntryLabel({required this.name, required this.isDir});

  final String name;
  final bool isDir;

  @override
  Widget build(BuildContext context) {
    final color = isDir ? Palette.accent : Palette.text;
    return Row(
      children: [
        SizedBox(
          width: 20,
          child: Icon(
            isDir ? Icons.folder_outlined : Icons.description_outlined,
            size: 16,
            color: color,
          ),
        ),
        const SizedBox(width: 6),
        Expanded(
          child: Text(
            name,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: TextStyle(
              color: color,
              fontFamily: 'Consolas',
              fontSize: 12,
              fontWeight: isDir ? FontWeight.w700 : FontWeight.w400,
            ),
          ),
        ),
      ],
    );
  }
}

class _DestinationInfoDialog extends StatefulWidget {
  const _DestinationInfoDialog({
    required this.api,
    required this.source,
    required this.destination,
    required this.status,
    required this.runtimeStatus,
    required this.syncActivity,
    required this.task,
    required this.scanReport,
    required this.error,
  });

  final AutoSyncApi api;
  final Map<String, dynamic> source;
  final Map<String, dynamic> destination;
  final Map<String, dynamic>? status;
  final Map<String, dynamic> runtimeStatus;
  final Map<String, dynamic> syncActivity;
  final Map<String, dynamic>? task;
  final Map<String, dynamic> scanReport;
  final String error;

  @override
  State<_DestinationInfoDialog> createState() => _DestinationInfoDialogState();
}

class _DestinationInfoDialogState extends State<_DestinationInfoDialog> {
  late Map<String, dynamic>? status = widget.status;
  late Map<String, dynamic> runtimeStatus = widget.runtimeStatus;
  late Map<String, dynamic> syncActivity = widget.syncActivity;
  late Map<String, dynamic>? task = widget.task;
  late Map<String, dynamic> scanReport = widget.scanReport;
  late String error = widget.error;
  Timer? refreshTimer;
  int refreshTick = 0;

  @override
  void initState() {
    super.initState();
    refreshTimer = Timer.periodic(
      const Duration(seconds: 1),
      (_) => _refresh(),
    );
  }

  @override
  void dispose() {
    refreshTimer?.cancel();
    super.dispose();
  }

  Future<void> _refresh() async {
    final sourceId = _str(widget.source['id']);
    final destinationId = _str(widget.destination['id']);
    Map<String, dynamic>? nextStatus = status;
    Map<String, dynamic>? nextTask = task;
    Map<String, dynamic>? nextReport = scanReport;
    String nextError = error;
    try {
      final nextRuntime = await widget.api.getRuntimeStatus();
      final nextActivity = await widget.api.getSyncActivity();
      refreshTick += 1;
      if (refreshTick == 1 || refreshTick % 5 == 0) {
        try {
          final rows = await widget.api.getStatus();
          nextStatus = _map(
            rows.firstWhere(
              (item) =>
                  _str(item['source_id']) == sourceId &&
                  _str(item['destination_id']) == destinationId,
              orElse: () => <String, dynamic>{},
            ),
          );
        } catch (_) {}
        try {
          nextTask = _newestTaskFor(
            await widget.api.getAllTasks(limit: 50),
            sourceId,
            destinationId,
          );
        } catch (_) {}
        try {
          nextReport = await widget.api.scanReport(sourceId, destinationId);
          nextError = '';
        } catch (err) {
          nextError = '$err';
        }
      }
      if (!mounted) return;
      setState(() {
        runtimeStatus = nextRuntime;
        syncActivity = nextActivity;
        status = nextStatus;
        task = nextTask;
        scanReport = nextReport ?? {};
        error = nextError;
      });
    } catch (_) {}
  }

  @override
  Widget build(BuildContext context) {
    final source = widget.source;
    final destination = widget.destination;
    final sourceId = _str(source['id']);
    final destinationId = _str(destination['id']);
    final runtime = _runtimeForDestination(
      source,
      destination,
      runtimeStatus,
      syncActivity,
    );
    final transfer = _matchingTransfer(runtime, sourceId, destinationId);
    final scan = _matchingScan(runtime, sourceId, destinationId);
    final rows = <MapEntry<String, String>>[
      MapEntry('Task', '$sourceId -> $destinationId'),
      MapEntry(
        'Path',
        [
          _machinePath(
            _str(source['machine_id'], 'local'),
            _str(source['src']),
          ),
          _machinePath(
            _str(destination['machine_id'], 'local'),
            _str(destination['path']),
          ),
        ].join('  ->  '),
      ),
      MapEntry('Status', _infoStatusLabel(status, runtime, transfer, scan)),
      MapEntry('Cycle', _cycleDisplay(status)),
      MapEntry('Type', _infoTypeLabel(source, destination, runtime, task)),
      MapEntry('Phase', _infoPhaseLabel(runtime, transfer, scan, task)),
      MapEntry('Snapshot', _infoSnapshotLabel(runtime, transfer, scan)),
      MapEntry(
        'Summary',
        _infoSummaryLabel(runtime, scanReport, task, scan, error),
      ),
      if (error.isNotEmpty) MapEntry('Error', error),
    ];
    return _MasterDialogFrame(
      title: 'Info',
      width: 860,
      maxHeight: 430,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            child: ListView(
              children: [
                for (final row in rows)
                  _InfoRow(
                    label: row.key,
                    value: row.value.isEmpty ? '-' : row.value,
                  ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _InfoRow extends StatelessWidget {
  const _InfoRow({required this.label, required this.value});

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    return Container(
      padding: const EdgeInsets.symmetric(vertical: 8),
      decoration: const BoxDecoration(
        border: Border(bottom: BorderSide(color: Palette.line)),
      ),
      child: Row(
        children: [
          SizedBox(
            width: 150,
            child: Text(label, style: const TextStyle(color: Palette.muted)),
          ),
          Expanded(
            child: Text(
              value,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(fontFamily: 'Consolas', fontSize: 12),
            ),
          ),
        ],
      ),
    );
  }
}

Map<String, dynamic>? _newestTaskFor(
  List<dynamic> machines,
  String sourceId,
  String destinationId,
) {
  Map<String, dynamic>? best;
  DateTime? bestStarted;
  for (final machine in machines) {
    for (final item in _list(_map(machine)['tasks'])) {
      final task = _map(item);
      if (_str(task['source_id']) != sourceId) continue;
      final ids = _str(
        task['destination_id'],
      ).split(',').map((id) => id.trim()).where((id) => id.isNotEmpty);
      if (!ids.contains(destinationId)) continue;
      final started = DateTime.tryParse(_str(task['started_at']));
      if (best == null ||
          (started != null &&
              (bestStarted == null || started.isAfter(bestStarted)))) {
        best = task;
        bestStarted = started;
      }
    }
  }
  return best;
}

String _destinationStatusText(Map<String, dynamic>? status) {
  if (status == null) return 'unknown';
  final value = _str(status['status'], 'unknown');
  final reason = _str(status['status_reason']);
  return reason.isEmpty ? value : '$value: $reason';
}

String _infoStatusLabel(
  Map<String, dynamic>? status,
  Map<String, dynamic> runtime,
  Map<String, dynamic> transfer,
  Map<String, dynamic> scan,
) {
  if (transfer.isNotEmpty) return 'syncing: transferring';
  if (scan.isNotEmpty) {
    return _str(scan['kind']) == 'compare'
        ? 'syncing: comparing'
        : 'syncing: scanning';
  }
  final phase = _str(runtime['sync_phase'], _str(runtime['phase']));
  if (_bool(runtime['syncing'])) {
    return phase.isEmpty ? 'syncing' : 'syncing: ${_phaseLabel(phase)}';
  }
  return _destinationStatusText(status);
}

Map<String, dynamic> _runtimeForDestination(
  Map<String, dynamic> source,
  Map<String, dynamic> destination,
  Map<String, dynamic> localRuntime,
  Map<String, dynamic> activity,
) {
  final sourceId = _str(source['id']);
  final destinationId = _str(destination['id']);
  bool matches(Map<String, dynamic> runtime) =>
      _runtimeMatchesTarget(runtime, sourceId, destinationId) ||
      _matchingTransfer(runtime, sourceId, destinationId).isNotEmpty ||
      _matchingScan(runtime, sourceId, destinationId).isNotEmpty;
  if (matches(localRuntime)) return localRuntime;
  for (final machine in _list(activity['machines'])) {
    final runtime = _map(_map(machine)['runtime']);
    if (matches(runtime)) return runtime;
  }
  return localRuntime;
}

bool _runtimeMatchesTarget(
  Map<String, dynamic> runtime,
  String sourceId,
  String destinationId,
) {
  if (!_hasLiveRuntimeActivity(runtime)) return false;
  final runtimeSource = _str(runtime['source_id']);
  final runtimeDestination = _str(runtime['destination_id']);
  return runtimeSource == sourceId && runtimeDestination == destinationId;
}

Map<String, dynamic> _matchingTransfer(
  Map<String, dynamic> runtime,
  String sourceId,
  String destinationId,
) {
  final transfer = _map(runtime['transfer']);
  if (transfer.isEmpty) return {};
  final transferSource = _str(transfer['source_id']);
  final transferDestination = _str(transfer['destination_id']);
  if ((transferSource.isEmpty || transferSource == sourceId) &&
      (transferDestination.isEmpty || transferDestination == destinationId)) {
    return transfer;
  }
  return {};
}

Map<String, dynamic> _matchingScan(
  Map<String, dynamic> runtime,
  String sourceId,
  String destinationId,
) {
  for (final item in _list(runtime['scans'])) {
    final scan = _map(item);
    final scanSource = _str(scan['source_id']);
    final scanDestination = _str(scan['destination_id']);
    if ((scanSource.isEmpty || scanSource == sourceId) &&
        (scanDestination.isEmpty || scanDestination == destinationId)) {
      return scan;
    }
  }
  final scan = _map(runtime['scan']);
  final scanSource = _str(scan['source_id']);
  final scanDestination = _str(scan['destination_id']);
  if (scan.isNotEmpty &&
      (scanSource.isEmpty || scanSource == sourceId) &&
      (scanDestination.isEmpty || scanDestination == destinationId)) {
    return scan;
  }
  return {};
}

String _infoTypeLabel(
  Map<String, dynamic> source,
  Map<String, dynamic> destination,
  Map<String, dynamic> runtime,
  Map<String, dynamic>? task,
) {
  if (_str(task?['status']) == 'running') {
    return _taskKindLabel(_str(task?['kind']));
  }
  final kind = _str(runtime['sync_kind']);
  if (_bool(runtime['syncing']) && kind.isNotEmpty) {
    return _taskKindLabel(kind == 'scan' ? 'compare' : kind);
  }
  return task == null ? '-' : _taskKindLabel(_str(task['kind']));
}

String _infoPhaseLabel(
  Map<String, dynamic> runtime,
  Map<String, dynamic> transfer,
  Map<String, dynamic> scan,
  Map<String, dynamic>? task,
) {
  if (transfer.isNotEmpty) return 'Transferring';
  if (scan.isNotEmpty) {
    return _str(scan['kind']) == 'compare' ? 'Comparing' : 'Scanning';
  }
  final phase = _str(runtime['sync_phase'], _str(runtime['phase']));
  if (phase.isNotEmpty) return _phaseLabel(phase);
  if (_str(task?['status']) == 'running') {
    return _str(task?['kind']) == 'compare' ? 'Comparing' : 'Syncing';
  }
  return '-';
}

String _phaseLabel(String phase) {
  switch (phase.trim()) {
    case 'scanning':
      return 'Scanning';
    case 'transferring':
      return 'Transferring';
    case 'verifying':
      return 'Verifying';
    case 'preparing':
      return 'Preparing';
    default:
      return phase;
  }
}

String _infoSnapshotLabel(
  Map<String, dynamic> runtime,
  Map<String, dynamic> transfer,
  Map<String, dynamic> scan,
) {
  if (transfer.isNotEmpty) {
    final file = _compactStatusPath(_str(transfer['rel_path'], '-'), 42);
    final speed = _formatBytesPerSecond(_int(transfer['bytes_per_sec']));
    final transferred = _int(transfer['transferred_bytes']);
    final total = _int(transfer['total_bytes']);
    final progress = total > 0 ? '${(transferred * 100 / total).floor()}%' : '';
    return [file, speed, progress].where((part) => part.isNotEmpty).join(' · ');
  }
  if (scan.isNotEmpty) {
    final entries = _int(scan['entries_seen']);
    final path = _compactStatusPath(
      _str(scan['current_path'], _str(scan['root_path'])),
      46,
    );
    return '$entries entries${path.isEmpty ? '' : ' · $path'}';
  }
  return 'idle';
}

String _infoSummaryLabel(
  Map<String, dynamic> runtime,
  Map<String, dynamic> report,
  Map<String, dynamic>? task,
  Map<String, dynamic> scan,
  String error,
) {
  if (error.isNotEmpty) return 'compare failed: $error';
  if (_str(task?['status']) == 'running') {
    if (_str(task?['kind']) == 'compare') {
      final entries = scan.isEmpty ? 0 : _int(scan['entries_seen']);
      return entries > 0
          ? 'comparing... $entries entries compared so far'
          : 'comparing... (waiting for result)';
    }
    return _syncProgressLabel(runtime).isEmpty
        ? 'syncing...'
        : _syncProgressLabel(runtime);
  }
  if (report.isNotEmpty) {
    final total =
        _int(report['to_add']) +
        _int(report['to_update']) +
        _int(report['to_delete']) +
        _int(report['type_mismatch']) +
        _int(report['metadata']);
    if (total == 0) {
      final matched = _int(report['in_sync']);
      return matched > 0 ? '0 differences ($matched matched)' : '0 differences';
    }
    final parts = <String>[];
    if (_int(report['to_add']) > 0) parts.add('+${_int(report['to_add'])}');
    if (_int(report['to_update']) > 0) {
      parts.add('~${_int(report['to_update'])}');
    }
    if (_int(report['to_delete']) > 0) {
      parts.add('-${_int(report['to_delete'])}');
    }
    if (_int(report['type_mismatch']) > 0) {
      parts.add('!${_int(report['type_mismatch'])}');
    }
    if (_int(report['metadata']) > 0) {
      parts.add('#${_int(report['metadata'])}');
    }
    final matched = _int(report['in_sync']);
    final note = matched > 0 ? ' · $matched / ${matched + total} matched' : '';
    return '$total differences (${parts.join(' ')})$note';
  }
  if (task == null) return '-';
  if (_str(task['kind']) == 'compare') {
    final diffs = _int(task['differences']);
    final scanned = _int(task['entries_scanned']);
    final suffix = scanned > 0 ? ' · $scanned compared' : '';
    return diffs == 0 ? '0 differences$suffix' : '$diffs differences$suffix';
  }
  final synced = _int(task['files_synced']);
  final failed = _int(task['differences']);
  if (failed > 0) {
    return '$synced/${synced + failed} files synced · $failed failed';
  }
  if (synced > 0) return '$synced files synced';
  return _str(task['status'], '-');
}

String _syncProgressLabel(Map<String, dynamic> runtime) {
  final plan = _list(runtime['sync_plan']);
  if (plan.length < 4) return '';
  final total = _int(plan[0]);
  final toCopy = _int(plan[1]);
  final matched = _int(plan[2]);
  final done = _int(plan[3]);
  if (total == 0 && toCopy == 0 && matched == 0) return '';
  return 'synced $done / $toCopy to copy · $matched unchanged ($total total)';
}

String _formatBytesPerSecond(int bytes) {
  if (bytes <= 0) return '';
  if (bytes < 1024) return '$bytes B/s';
  if (bytes < 1024 * 1024) return '${(bytes / 1024).toStringAsFixed(1)} KB/s';
  if (bytes < 1024 * 1024 * 1024) {
    return '${(bytes / 1024 / 1024).toStringAsFixed(1)} MB/s';
  }
  return '${(bytes / 1024 / 1024 / 1024).toStringAsFixed(1)} GB/s';
}

class _IssueSummary extends StatelessWidget {
  const _IssueSummary(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    return Text(
      text,
      maxLines: 2,
      overflow: TextOverflow.ellipsis,
      style: const TextStyle(color: Palette.muted, fontSize: 12),
    );
  }
}

class _ReadmeBody extends StatelessWidget {
  const _ReadmeBody();

  @override
  Widget build(BuildContext context) {
    return const SingleChildScrollView(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _ReadmeSection(
            title: 'Destination Sync',
            paragraphs: [
              'Incremental 会关闭当前 source cycle，并且只同步当前选中的 destination。对 Realtime 目标它应用积压的事件路径；ZFS 源上如果有已验证的基准快照，会走 zfs diff 快路径只同步差异。',
              'Full 是完整对账加同步，修复包括目标侧漂移在内的所有差异；开启 Mirror 时删除 source 不存在的多余路径。有两个实现，按 ZFS diff 配置自动选择：zfs diff 版在 src 和 dst 都在本机 ZFS 且有已验证基准快照时，只对账变化过的路径；对比版会并行扫描整棵树、对比清单、只传输缺失或不一致的文件。',
              'Scan（对账不同步）：生成差异报告，不改动任何文件。与 Full 相同，ZFS diff 可用时只比对两侧基准以来变化的路径；否则两端并行全树扫描。',
            ],
          ),
          _ReadmeSection(
            title: '可靠性行为',
            paragraphs: [
              '复制过程中源文件被修改不会把目标标红：这些路径记为黄色 source_changing 问题，下一轮自动收敛。单个文件失败不会中断整批传输（最多容忍 20 个），连接断开才会立即终止。',
              '跨机传输的每个文件在落盘前都做 blake3 端到端校验，先写临时文件再原子改名，中断后可断点续传。',
            ],
          ),
          _ReadmeSection(
            title: 'Restart Recovery',
            paragraphs: [
              '进程重启后会重新驱动未完成的 cycle：目标端已经存在且匹配的文件会跳过；缺失文件、不一致文件、类型变化，以及未完成的临时传输都会被修复。',
              '注意：Realtime 目标的绿点表示“事件都已应用”，不代表整棵树被验证过。如果怀疑有漂移，先用 Scan 查看差异，再用 Full 对账修复。',
            ],
          ),
          _ReadmeSection(
            title: 'Example',
            paragraphs: [
              r'对于 \\?\C:\Users\tiger\Documents\xwechat_files 到 nas:/opt，实际 destination root 通常是 /opt/xwechat_files。',
              '如果重启前已经同步了一部分，Incremental 会继续补齐缺口并修复不一致；Full 会对账并修复所有差异，并在 Mirror 开启时删除额外文件。',
            ],
            last: true,
          ),
        ],
      ),
    );
  }
}

class _ReadmeSection extends StatelessWidget {
  const _ReadmeSection({
    required this.title,
    required this.paragraphs,
    this.last = false,
  });

  final String title;
  final List<String> paragraphs;
  final bool last;

  @override
  Widget build(BuildContext context) {
    return Container(
      width: double.infinity,
      padding: EdgeInsets.only(bottom: last ? 0 : 12),
      margin: EdgeInsets.only(bottom: last ? 0 : 12),
      decoration: BoxDecoration(
        border: Border(
          bottom: last
              ? BorderSide.none
              : const BorderSide(color: Palette.line),
        ),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            title,
            style: const TextStyle(fontSize: 13, fontWeight: FontWeight.w700),
          ),
          ...paragraphs.map(
            (text) => Padding(
              padding: const EdgeInsets.only(top: 8),
              child: Text(
                text,
                style: const TextStyle(
                  color: Palette.muted,
                  fontSize: 13,
                  height: 1.55,
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _CompactInput extends StatefulWidget {
  const _CompactInput({
    this.controller,
    this.initialValue,
    this.placeholder,
    this.onChanged,
    this.onTap,
    this.numeric = false,
    this.height = _masterControlHeight,
  });

  final TextEditingController? controller;
  final String? initialValue;
  final String? placeholder;
  final ValueChanged<String>? onChanged;
  final VoidCallback? onTap;
  final bool numeric;
  final double height;

  @override
  State<_CompactInput> createState() => _CompactInputState();
}

class _CompactInputState extends State<_CompactInput> {
  TextEditingController? internalController;
  late final FocusNode focusNode;

  TextEditingController get effectiveController =>
      widget.controller ?? internalController!;

  @override
  void initState() {
    super.initState();
    focusNode = FocusNode();
    if (widget.controller == null) {
      internalController = TextEditingController(
        text: widget.initialValue ?? '',
      );
    }
  }

  @override
  void didUpdateWidget(covariant _CompactInput oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (widget.controller != null) {
      internalController?.dispose();
      internalController = null;
      return;
    }
    internalController ??= TextEditingController(
      text: widget.initialValue ?? '',
    );
    final oldText = oldWidget.initialValue ?? '';
    final nextText = widget.initialValue ?? '';
    final canReplaceFocusedText =
        focusNode.hasFocus && effectiveController.text == oldText;
    if (effectiveController.text != nextText &&
        (!focusNode.hasFocus || canReplaceFocusedText)) {
      effectiveController.text = nextText;
    }
  }

  @override
  void dispose() {
    internalController?.dispose();
    focusNode.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Container(
      height: widget.height,
      padding: const EdgeInsets.symmetric(horizontal: 9),
      decoration: BoxDecoration(
        color: Colors.white,
        border: Border.all(color: Palette.line),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Center(
        child: TextFormField(
          controller: effectiveController,
          focusNode: focusNode,
          keyboardType: widget.numeric
              ? TextInputType.number
              : TextInputType.text,
          maxLines: 1,
          onChanged: widget.onChanged,
          onTap: widget.onTap,
          style: const TextStyle(fontSize: 12),
          decoration: InputDecoration(
            hintText: widget.placeholder,
            border: InputBorder.none,
            enabledBorder: InputBorder.none,
            focusedBorder: InputBorder.none,
            isCollapsed: true,
            contentPadding: EdgeInsets.zero,
          ),
        ),
      ),
    );
  }
}

class _CompactSelect extends StatelessWidget {
  const _CompactSelect({
    required this.value,
    required this.options,
    required this.onChanged,
    this.height = _masterControlHeight,
    this.openUp = false,
  });

  final String value;
  final List<String> options;
  final ValueChanged<String> onChanged;
  final double height;
  final bool openUp;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: height,
      child: _MasterMenuSelect(
        value: value,
        options: options,
        height: height,
        openUp: openUp,
        onSelected: onChanged,
      ),
    );
  }
}

class _CheckCell extends StatelessWidget {
  const _CheckCell({required this.value, required this.onChanged, this.label});

  final bool value;
  final ValueChanged<bool> onChanged;
  final String? label;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: _masterControlHeight,
      child: Row(
        mainAxisAlignment: label == null
            ? MainAxisAlignment.center
            : MainAxisAlignment.start,
        children: [
          Checkbox(
            value: value,
            visualDensity: VisualDensity.compact,
            onChanged: (next) => onChanged(next ?? false),
          ),
          if (label != null) Text(label!, style: const TextStyle(fontSize: 13)),
        ],
      ),
    );
  }
}

class _MachinesDialog extends StatefulWidget {
  const _MachinesDialog({
    required this.api,
    required this.machines,
    required this.initialStatus,
    required this.onChanged,
  });

  final AutoSyncApi api;
  final List<Map<String, dynamic>> machines;
  final Map<String, dynamic> initialStatus;
  final Future<void> Function() onChanged;

  @override
  State<_MachinesDialog> createState() => _MachinesDialogState();
}

class _MachinesDialogState extends State<_MachinesDialog> {
  static const double _editorHeight = 34;

  String message = '';
  bool busy = false;
  List<Map<String, dynamic>> rows = [];
  final id = TextEditingController();
  final name = TextEditingController();
  final alias = TextEditingController();
  final host = TextEditingController();
  final port = TextEditingController(text: '18765');
  final sshUser = TextEditingController();
  final sshPort = TextEditingController(text: '22');
  final installDir = TextEditingController();
  String os = 'linux';

  @override
  void initState() {
    super.initState();
    final statusRows = _mapRefs(widget.initialStatus['machines']);
    rows = statusRows.isNotEmpty ? statusRows : widget.machines;
  }

  @override
  void dispose() {
    id.dispose();
    name.dispose();
    alias.dispose();
    host.dispose();
    port.dispose();
    sshUser.dispose();
    sshPort.dispose();
    installDir.dispose();
    super.dispose();
  }

  Future<void> _do(String label, Future<void> Function() action) async {
    setState(() {
      busy = true;
      message = '$label...';
    });
    try {
      await action();
      await widget.onChanged();
      await _refresh();
      setState(() => message = '$label done');
    } catch (error) {
      setState(() => message = '$label failed: $error');
    } finally {
      if (mounted) {
        setState(() => busy = false);
      }
    }
  }

  Future<void> _refresh({bool discover = false}) async {
    final status = await widget.api.getMachines(discover: discover);
    final nextRows = _mapRefs(status['machines']);
    if (mounted && nextRows.isNotEmpty) {
      setState(() => rows = nextRows);
    }
  }

  void _select(Map<String, dynamic> machine) {
    id.text = _str(machine['id']);
    name.text = _str(machine['name']);
    alias.text = _str(machine['alias_name']);
    host.text = _str(machine['host']);
    port.text = _str(machine['port'], '18765');
    sshUser.text = _str(machine['ssh_user']);
    sshPort.text = _str(machine['ssh_port'], '22');
    installDir.text = _str(machine['install_dir']);
    setState(() => os = _str(machine['os'], 'linux'));
  }

  Future<void> _saveMachine() {
    final machineId = id.text.trim().isNotEmpty
        ? id.text.trim()
        : (name.text.trim().isNotEmpty ? name.text.trim() : host.text.trim());
    final machine = {
      'id': machineId,
      'name': name.text.trim(),
      'alias_name': alias.text.trim(),
      'host': host.text.trim(),
      'port': int.tryParse(port.text) ?? 18765,
      'ssh_user': sshUser.text.trim(),
      'ssh_port': int.tryParse(sshPort.text) ?? 22,
      'os': os,
      'install_dir': installDir.text.trim(),
      'enabled': true,
      'manual': true,
    };
    return widget.api.addMachine(machine);
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: 'Machines',
      width: 880,
      maxHeight: 560,
      shrinkWrap: true,
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          ConstrainedBox(
            constraints: const BoxConstraints(maxHeight: 330),
            child: Container(
              decoration: const BoxDecoration(
                border: Border(top: BorderSide(color: Palette.line)),
              ),
              child: ListView(
                shrinkWrap: true,
                children: [
                  const _MachineHeaderRow(),
                  if (rows.isEmpty)
                    const SizedBox.shrink()
                  else
                    ...rows.map((machine) {
                      final machineId = _str(machine['id']);
                      return _MachineRow(
                        machine: machine,
                        selected: machineId == id.text,
                        onTap: () => _select(machine),
                        onRemove: machineId == 'local' || busy
                            ? null
                            : () => _do(
                                'Remove $machineId',
                                () => widget.api.removeMachine(machineId),
                              ),
                      );
                    }),
                ],
              ),
            ),
          ),
          Container(
            padding: const EdgeInsets.only(top: 20),
            decoration: const BoxDecoration(
              border: Border(top: BorderSide(color: Palette.line)),
            ),
            child: Column(
              children: [
                _MachineEditorGrid(
                  height: _editorHeight,
                  name: name,
                  alias: alias,
                  host: host,
                  port: port,
                  sshUser: sshUser,
                  sshPort: sshPort,
                  os: os == 'windows' ? 'windows' : 'linux',
                  onOsChanged: (value) => setState(() => os = value),
                ),
                const SizedBox(height: 8),
                Row(
                  children: [
                    Expanded(child: _IssueSummary(message)),
                    MasterButton(
                      label: 'Save',
                      width: 72,
                      primary: true,
                      onTap: busy
                          ? null
                          : () => _do('Save machine', _saveMachine),
                    ),
                    const SizedBox(width: 6),
                    MasterButton(
                      label: 'Discover',
                      width: 76,
                      onTap: busy
                          ? null
                          : () =>
                                _do('Discover', () => _refresh(discover: true)),
                    ),
                  ],
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _FormHead extends StatelessWidget {
  const _FormHead({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    final child = Text(
      text,
      maxLines: 1,
      overflow: TextOverflow.ellipsis,
      style: const TextStyle(
        color: Palette.muted,
        fontSize: 11,
        fontWeight: FontWeight.w700,
      ),
    );
    return child;
  }
}

class _MachineEditorGrid extends StatelessWidget {
  const _MachineEditorGrid({
    required this.height,
    required this.name,
    required this.alias,
    required this.host,
    required this.port,
    required this.sshUser,
    required this.sshPort,
    required this.os,
    required this.onOsChanged,
  });

  static const _labels = [
    'Name',
    'Alias',
    'Host',
    'Port',
    'SSH User',
    'SSH Port',
    'OS',
  ];
  static const _flexes = [96, 96, 140, 90, 114, 86, 104];

  final double height;
  final TextEditingController name;
  final TextEditingController alias;
  final TextEditingController host;
  final TextEditingController port;
  final TextEditingController sshUser;
  final TextEditingController sshPort;
  final String os;
  final ValueChanged<String> onOsChanged;

  @override
  Widget build(BuildContext context) {
    final controls = <Widget>[
      _CompactInput(controller: name, height: height),
      _CompactInput(controller: alias, height: height),
      _CompactInput(controller: host, height: height),
      _CompactInput(controller: port, numeric: true, height: height),
      _CompactInput(controller: sshUser, height: height),
      _CompactInput(controller: sshPort, numeric: true, height: height),
      _CompactSelect(
        height: height,
        value: os,
        options: const ['linux', 'windows'],
        openUp: true,
        onChanged: onOsChanged,
      ),
    ];
    return Column(
      children: [
        Row(
          children: [
            for (var i = 0; i < _labels.length; i++) ...[
              Expanded(
                flex: _flexes[i],
                child: _FormHead(text: _labels[i]),
              ),
              if (i != _labels.length - 1) const SizedBox(width: 6),
            ],
          ],
        ),
        const SizedBox(height: 6),
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            for (var i = 0; i < controls.length; i++) ...[
              Expanded(flex: _flexes[i], child: controls[i]),
              if (i != controls.length - 1) const SizedBox(width: 6),
            ],
          ],
        ),
      ],
    );
  }
}

class _MachineHeaderRow extends StatelessWidget {
  const _MachineHeaderRow();

  @override
  Widget build(BuildContext context) {
    return const _MachineGrid(
      head: true,
      dot: SizedBox.shrink(),
      name: Text('Name'),
      host: Text('Host'),
      port: Text('Port'),
      sshUser: Text('SSH User'),
      sshPort: Text('SSH Port'),
      os: Text('OS'),
      action: SizedBox.shrink(),
    );
  }
}

class _MachineRow extends StatelessWidget {
  const _MachineRow({
    required this.machine,
    required this.selected,
    required this.onTap,
    required this.onRemove,
  });

  final Map<String, dynamic> machine;
  final bool selected;
  final VoidCallback onTap;
  final VoidCallback? onRemove;

  @override
  Widget build(BuildContext context) {
    final online = _bool(machine['online'], _str(machine['id']) == 'local');
    final name = _str(machine['name'], _str(machine['id']));
    final alias = _str(machine['alias_name']);
    final meta = alias.isNotEmpty && alias != name
        ? alias
        : _str(machine['id']);
    return InkWell(
      onTap: onTap,
      child: Container(
        color: selected ? const Color(0xfff8fafc) : Colors.transparent,
        child: _MachineGrid(
          dot: Container(
            width: 10,
            height: 10,
            decoration: BoxDecoration(
              color: online ? Palette.green : Palette.red,
              shape: BoxShape.circle,
            ),
          ),
          name: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              Text(
                name,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(
                  fontSize: 13,
                  fontWeight: FontWeight.w600,
                ),
              ),
              Text(
                meta,
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(color: Palette.muted, fontSize: 12),
              ),
            ],
          ),
          host: _GridText(_str(machine['host'])),
          port: _GridText(_str(machine['port'])),
          sshUser: _GridText(_str(machine['ssh_user'], '-')),
          sshPort: _GridText(_str(machine['ssh_port'], '22')),
          os: _GridText(_str(machine['os'])),
          action: MasterButton(
            label: 'x',
            square: true,
            danger: true,
            onTap: onRemove,
          ),
        ),
      ),
    );
  }
}

class _MachineGrid extends StatelessWidget {
  const _MachineGrid({
    required this.dot,
    required this.name,
    required this.host,
    required this.port,
    required this.sshUser,
    required this.sshPort,
    required this.os,
    required this.action,
    this.head = false,
  });

  final Widget dot;
  final Widget name;
  final Widget host;
  final Widget port;
  final Widget sshUser;
  final Widget sshPort;
  final Widget os;
  final Widget action;
  final bool head;

  @override
  Widget build(BuildContext context) {
    final style = TextStyle(
      color: head ? Palette.muted : Palette.text,
      fontSize: head ? 12 : 13,
      fontWeight: head ? FontWeight.w600 : FontWeight.w400,
    );
    return DefaultTextStyle.merge(
      style: style,
      child: Container(
        constraints: const BoxConstraints(minHeight: 38),
        padding: const EdgeInsets.symmetric(vertical: 4),
        decoration: const BoxDecoration(
          border: Border(bottom: BorderSide(color: Palette.line)),
        ),
        child: Row(
          children: [
            SizedBox(width: 10, child: Center(child: dot)),
            const SizedBox(width: 6),
            Expanded(flex: 105, child: name),
            const SizedBox(width: 6),
            Expanded(flex: 61, child: host),
            const SizedBox(width: 6),
            SizedBox(width: 84, child: port),
            const SizedBox(width: 6),
            SizedBox(width: 118, child: sshUser),
            const SizedBox(width: 6),
            SizedBox(width: 104, child: sshPort),
            const SizedBox(width: 6),
            SizedBox(width: 94, child: os),
            const SizedBox(width: 6),
            SizedBox(width: 52, child: Center(child: action)),
          ],
        ),
      ),
    );
  }
}

class _CollectorDialog extends StatefulWidget {
  const _CollectorDialog({required this.api});

  final AutoSyncApi api;

  @override
  State<_CollectorDialog> createState() => _CollectorDialogState();
}

class _CollectorDialogState extends State<_CollectorDialog> {
  Map<String, dynamic> cfg = {};
  Map<String, dynamic> status = {};
  Map<String, dynamic> deployStatus = {};
  final hostRunStatuses = <int, Map<String, dynamic>>{};
  final hostDeployStatuses = <int, Map<String, dynamic>>{};
  String message = '';
  bool loading = true;
  bool busy = false;
  bool autoPush = false;
  final activeRunIndexes = <int>{};
  final activeDeployIndexes = <int>{};
  Timer? saveTimer;
  Timer? runTimer;
  Timer? deployTimer;
  final gitDir = TextEditingController();
  final splitMb = TextEditingController(text: '95');

  List<Map<String, dynamic>> get hosts => _mapRefs(cfg['hosts']);

  @override
  void initState() {
    super.initState();
    gitDir.addListener(_scheduleSave);
    splitMb.addListener(_scheduleSave);
    _load();
  }

  @override
  void dispose() {
    gitDir.removeListener(_scheduleSave);
    splitMb.removeListener(_scheduleSave);
    saveTimer?.cancel();
    runTimer?.cancel();
    deployTimer?.cancel();
    if (cfg.isNotEmpty) {
      unawaited(_save());
    }
    gitDir.dispose();
    splitMb.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    try {
      final nextCfg = await widget.api.collectorConfig();
      final nextStatus = await widget.api.collectorStatus();
      final nextDeployStatus = await widget.api.collectorDeployStatus();
      final nextHosts = _mapRefs(
        nextCfg['hosts'],
      ).map((host) => Map<String, dynamic>.from(host)).toList();
      final nextHostRunStatuses = <int, Map<String, dynamic>>{};
      final nextHostDeployStatuses = <int, Map<String, dynamic>>{};
      final nextActiveRunIndexes = <int>{};
      final nextActiveDeployIndexes = <int>{};
      for (var index = 0; index < nextHosts.length; index++) {
        final run = await widget.api.collectorStatus(index: index);
        final deploy = await widget.api.collectorDeployStatus(index: index);
        if (run.isNotEmpty) {
          nextHostRunStatuses[index] = run;
          if (_bool(run['running'])) nextActiveRunIndexes.add(index);
        }
        if (deploy.isNotEmpty) {
          nextHostDeployStatuses[index] = deploy;
          if (_bool(deploy['running'])) nextActiveDeployIndexes.add(index);
        }
      }
      if (mounted) {
        setState(() {
          cfg = Map<String, dynamic>.from(nextCfg);
          cfg['hosts'] = nextHosts;
          status = nextStatus;
          deployStatus = nextDeployStatus;
          hostRunStatuses
            ..clear()
            ..addAll(nextHostRunStatuses);
          hostDeployStatuses
            ..clear()
            ..addAll(nextHostDeployStatuses);
          activeRunIndexes
            ..clear()
            ..addAll(nextActiveRunIndexes);
          activeDeployIndexes
            ..clear()
            ..addAll(nextActiveDeployIndexes);
          gitDir.text = _str(cfg['git_dir']);
          splitMb.text = _str(cfg['split_threshold_mb'], '95');
          autoPush = _bool(cfg['auto_commit_push']);
          loading = false;
          message = '';
        });
        _startDeployPolling();
        if (nextActiveRunIndexes.isNotEmpty || _bool(nextStatus['running'])) {
          _startRunPolling();
        }
      }
    } catch (error) {
      if (mounted) {
        setState(() {
          message = '$error';
          loading = false;
        });
      }
    }
  }

  Future<void> _save() async {
    cfg['git_dir'] = gitDir.text.trim();
    cfg['split_threshold_mb'] = int.tryParse(splitMb.text) ?? 95;
    cfg['auto_commit_push'] = autoPush;
    await widget.api.saveCollectorConfig(cfg);
  }

  void _scheduleSave() {
    if (loading || cfg.isEmpty) return;
    saveTimer?.cancel();
    saveTimer = Timer(const Duration(milliseconds: 400), () {
      saveTimer = null;
      _save().catchError((Object error) {
        if (mounted) setState(() => message = '$error');
      });
    });
  }

  Future<void> _persistNow() async {
    saveTimer?.cancel();
    saveTimer = null;
    final nextCfg = await widget.api.saveCollectorConfig(cfg);
    if (!mounted) return;
    setState(() {
      cfg = Map<String, dynamic>.from(nextCfg);
      cfg['hosts'] = _mapRefs(
        nextCfg['hosts'],
      ).map((host) => Map<String, dynamic>.from(host)).toList();
      gitDir.text = _str(cfg['git_dir']);
      splitMb.text = _str(cfg['split_threshold_mb'], '95');
      autoPush = _bool(cfg['auto_commit_push']);
    });
  }

  Future<Map<String, dynamic>?> _refreshHostConfig(int index) async {
    await _persistNow();
    final nextCfg = await widget.api.collectorConfig();
    if (!mounted) return null;
    final nextHosts = _mapRefs(
      nextCfg['hosts'],
    ).map((host) => Map<String, dynamic>.from(host)).toList();
    setState(() {
      cfg = Map<String, dynamic>.from(nextCfg);
      cfg['hosts'] = nextHosts;
      gitDir.text = _str(cfg['git_dir']);
      splitMb.text = _str(cfg['split_threshold_mb'], '95');
      autoPush = _bool(cfg['auto_commit_push']);
    });
    if (index < 0 || index >= nextHosts.length) return null;
    return nextHosts[index];
  }

  Future<void> _runHost(int index) async {
    if (_bool(hostRunStatuses[index]?['running'])) {
      setState(
        () => message = 'Collector run for this host is already running',
      );
      return;
    }
    setState(() {
      busy = true;
      message = '';
      activeRunIndexes.add(index);
      status = {'running': true, 'log': <String>[]};
      hostRunStatuses[index] = Map<String, dynamic>.from(status);
    });
    try {
      await _persistNow();
      final next = await widget.api.collectorRunHost(index);
      if (mounted) {
        setState(() {
          status = next;
          hostRunStatuses[index] = Map<String, dynamic>.from(next);
          message = '';
        });
      }
      if (_bool(next['running'])) {
        _startRunPolling();
      }
    } catch (error) {
      setState(() => message = '$error');
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  void _startRunPolling() {
    runTimer?.cancel();
    runTimer = Timer.periodic(const Duration(seconds: 1), (_) {
      unawaited(_refreshRunStatus());
    });
    unawaited(_refreshRunStatus());
  }

  Future<void> _refreshRunStatus() async {
    try {
      final indexes = activeRunIndexes.toList();
      if (indexes.isEmpty) {
        runTimer?.cancel();
        runTimer = null;
        return;
      }
      final states = <int, Map<String, dynamic>>{};
      for (final index in indexes) {
        states[index] = await widget.api.collectorStatus(index: index);
      }
      if (!mounted) return;
      setState(() {
        for (final entry in states.entries) {
          hostRunStatuses[entry.key] = Map<String, dynamic>.from(entry.value);
          status = entry.value;
          if (!_bool(entry.value['running'])) {
            activeRunIndexes.remove(entry.key);
          }
        }
      });
      if (activeRunIndexes.isEmpty) {
        runTimer?.cancel();
        runTimer = null;
      }
    } catch (_) {}
  }

  void _addHost() {
    final list = hosts;
    list.add({
      'name': '',
      'hostname': '',
      'user': 'root',
      'port': 22,
      'identity_file': '',
      'root': '',
      'paths': <String>[],
      'exclude': <String>[],
      'enabled': true,
      'deploy_script': '',
    });
    setState(() => cfg['hosts'] = list);
    _scheduleSave();
  }

  Future<void> _browseRoot(Map<String, dynamic> host) async {
    final selected = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) => _PathPickerDialog(
        api: widget.api,
        machineIds: const ['local'],
        machineLabel: (id) => id,
        initialMachineId: 'local',
        initialPath: _str(host['root'], hostPlatformIsWindows ? r'C:\' : '/'),
        showAddDirectory: false,
        initialAddDirectory: false,
      ),
    );
    if (selected == null) return;
    setState(() => host['root'] = _str(selected['path']));
    _scheduleSave();
  }

  Future<void> _browseGitDir() async {
    final selected = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) => _PathPickerDialog(
        api: widget.api,
        machineIds: const ['local'],
        machineLabel: (id) => id,
        initialMachineId: 'local',
        initialPath: gitDir.text.trim().isEmpty
            ? (hostPlatformIsWindows ? r'C:\' : '/')
            : gitDir.text.trim(),
        showAddDirectory: false,
        initialAddDirectory: false,
      ),
    );
    if (selected == null) return;
    gitDir.text = _str(selected['path']);
    _scheduleSave();
  }

  Future<void> _runDeploy(int index) async {
    if (_bool(hostDeployStatuses[index]?['running'])) {
      setState(() => message = 'Deploy for this host is already running');
      return;
    }
    setState(() {
      message = 'Deploying...';
      activeDeployIndexes.add(index);
      deployStatus = {'running': true, 'log': <String>[]};
      hostDeployStatuses[index] = Map<String, dynamic>.from(deployStatus);
    });
    try {
      await _persistNow();
      final next = await widget.api.collectorDeploy(index);
      if (mounted) {
        setState(() {
          deployStatus = next;
          hostDeployStatuses[index] = Map<String, dynamic>.from(next);
          message = '';
        });
      }
      if (_bool(next['running'])) {
        _startDeployPolling();
      }
    } catch (error) {
      if (mounted) {
        setState(() {
          message = '$error';
          deployStatus = {
            'running': false,
            'ok': false,
            'log': <String>['$error'],
          };
          hostDeployStatuses[index] = Map<String, dynamic>.from(deployStatus);
        });
      }
    }
  }

  void _startDeployPolling() {
    deployTimer?.cancel();
    deployTimer = Timer.periodic(const Duration(seconds: 1), (_) {
      unawaited(_refreshDeployStatus());
    });
    unawaited(_refreshDeployStatus());
  }

  Future<void> _refreshDeployStatus() async {
    try {
      final indexes = List<int>.generate(hosts.length, (index) => index);
      final states = <int, Map<String, dynamic>>{};
      for (final index in indexes) {
        states[index] = await widget.api.collectorDeployStatus(index: index);
      }
      if (!mounted) return;
      setState(() {
        activeDeployIndexes.clear();
        for (final entry in states.entries) {
          hostDeployStatuses[entry.key] = Map<String, dynamic>.from(
            entry.value,
          );
          if (_bool(entry.value['running'])) {
            activeDeployIndexes.add(entry.key);
            deployStatus = entry.value;
          }
        }
      });
    } catch (_) {}
  }

  Future<void> _showHostLog(int index) async {
    final activeRun = _bool(hostRunStatuses[index]?['running']);
    final activeDeploy = _bool(hostDeployStatuses[index]?['running']);
    final initialRun = activeRun
        ? (hostRunStatuses[index] ?? <String, dynamic>{})
        : (hostRunStatuses[index] ?? <String, dynamic>{});
    final initialDeploy = activeDeploy
        ? (hostDeployStatuses[index] ?? <String, dynamic>{})
        : (hostDeployStatuses[index] ?? <String, dynamic>{});
    await showDialog<void>(
      context: context,
      builder: (context) => _CollectorLogDialog(
        api: widget.api,
        initialRunStatus: initialRun,
        initialDeployStatus: initialDeploy,
        pollRun: activeRun,
        pollDeploy: activeDeploy,
        hostIndex: index,
      ),
    );
  }

  Future<void> _showConfig() async {
    await _persistNow();
    if (!mounted) return;
    await showDialog<void>(
      context: context,
      builder: (context) => _MasterDialogFrame(
        title: 'Collector Config',
        width: 900,
        maxHeight: 720,
        child: _MasterPre(
          text: const JsonEncoder.withIndent('  ').convert(cfg),
          maxHeight: 640,
        ),
      ),
    );
  }

  String _runState() {
    if (loading) return 'Loading...';
    if (message.isNotEmpty) return message;
    if (activeRunIndexes.isNotEmpty) {
      if (activeRunIndexes.length == 1) return _collectorRunningText();
      return 'Collecting ${activeRunIndexes.length} hosts...';
    }
    if (activeDeployIndexes.isNotEmpty) {
      if (activeDeployIndexes.length == 1) {
        final index = activeDeployIndexes.first;
        if (index >= 0 && index < hosts.length) {
          final host = hosts[index];
          final label = _str(host['name'], _str(host['hostname'], 'host'));
          return 'Deploying $label...';
        }
      }
      return 'Deploying ${activeDeployIndexes.length} hosts...';
    }
    if (status['ok'] != null) return _collectorSummaryText();
    return '';
  }

  String _collectorRunningText() {
    final current = _str(status['current_file']);
    final total = _int(status['total_files']);
    final done = _int(status['succeeded_files']) + _int(status['failed_files']);
    final progress = total > 0 ? ' · $done/$total' : '';
    if (current.isEmpty) return 'Copying...$progress';
    return 'Copying $current$progress';
  }

  String _collectorSummaryText() {
    final total = _int(status['total_files']);
    final succeeded = _int(status['succeeded_files']);
    final failed = _int(status['failed_files']);
    final started = _str(status['started_at']);
    final duration = _collectorDurationLabel(status['duration_ms']);
    final parts = <String>['$total files', '$succeeded ok', '$failed failed'];
    if (started.isNotEmpty) parts.add('started $started');
    if (duration.isNotEmpty) parts.add(duration);
    return parts.join(' · ');
  }

  String _collectorDurationLabel(dynamic value) {
    if (value == null) return '';
    final ms = value is num ? value.toDouble() : double.tryParse('$value');
    if (ms == null) return '';
    final seconds = ms / 1000;
    if (seconds < 60) return 'duration ${seconds.round()}s';
    final minutes = seconds ~/ 60;
    if (minutes < 60) return 'duration ${minutes}m ${seconds.round() % 60}s';
    return 'duration ${minutes ~/ 60}h ${minutes % 60}m';
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: 'Collector',
      width: 980,
      maxHeight: 640,
      shrinkWrap: true,
      child: loading
          ? const Center(child: CircularProgressIndicator())
          : Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Row(
                  crossAxisAlignment: CrossAxisAlignment.end,
                  children: [
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          const _MasterLabel('Git repository dir'),
                          Row(
                            children: [
                              Expanded(
                                child: _CompactInput(controller: gitDir),
                              ),
                              const SizedBox(width: 6),
                              MasterButton(
                                label: '...',
                                square: true,
                                onTap: _browseGitDir,
                              ),
                            ],
                          ),
                        ],
                      ),
                    ),
                    const SizedBox(width: 12),
                    SizedBox(
                      width: 120,
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          const _MasterLabel('Split at (MiB)'),
                          _CompactInput(controller: splitMb, numeric: true),
                        ],
                      ),
                    ),
                    const SizedBox(width: 12),
                    SizedBox(
                      width: 150,
                      child: _CheckCell(
                        value: autoPush,
                        label: 'Auto commit & push',
                        onChanged: (next) {
                          setState(() => autoPush = next);
                          _scheduleSave();
                        },
                      ),
                    ),
                  ],
                ),
                const SizedBox(height: 12),
                Container(
                  padding: const EdgeInsets.only(top: 10),
                  decoration: const BoxDecoration(
                    border: Border(top: BorderSide(color: Palette.line)),
                  ),
                  child: Row(
                    children: [
                      const Expanded(
                        child: Text(
                          'Hosts',
                          style: TextStyle(fontWeight: FontWeight.w600),
                        ),
                      ),
                      MasterButton(
                        label: '+ Add host',
                        width: 96,
                        onTap: busy ? null : _addHost,
                      ),
                    ],
                  ),
                ),
                const SizedBox(height: 8),
                ConstrainedBox(
                  constraints: const BoxConstraints(maxHeight: 260),
                  child: ListView(
                    shrinkWrap: true,
                    children: [
                      const _CollectorHostHeader(),
                      if (hosts.isEmpty)
                        const SizedBox.shrink()
                      else
                        ...hosts.asMap().entries.map(
                          (entry) => _CollectorHostRow(
                            index: entry.key,
                            host: entry.value,
                            onChanged: () {
                              setState(() {});
                              _scheduleSave();
                            },
                            onRemove: () {
                              final next = hosts;
                              next.removeAt(entry.key);
                              setState(() => cfg['hosts'] = next);
                              _scheduleSave();
                            },
                            onBrowseRoot: () => _browseRoot(entry.value),
                            onPaths: () async {
                              final host = await _refreshHostConfig(entry.key);
                              if (!context.mounted || host == null) return;
                              await showDialog<void>(
                                context: context,
                                builder: (context) => _CollectorPathsDialog(
                                  api: widget.api,
                                  host: host,
                                  onChanged: () {
                                    setState(() {});
                                    _scheduleSave();
                                  },
                                ),
                              );
                            },
                            onDeploy: () async {
                              await showDialog<void>(
                                context: context,
                                builder: (context) => _CollectorDeployDialog(
                                  api: widget.api,
                                  hostIndex: entry.key,
                                  host: entry.value,
                                  onChanged: () {
                                    setState(() {});
                                    _scheduleSave();
                                  },
                                ),
                              );
                            },
                            onDeployLog: () => _showHostLog(entry.key),
                            onCollect:
                                _bool(hostRunStatuses[entry.key]?['running'])
                                ? null
                                : () => _runHost(entry.key),
                            onDeployRun:
                                _bool(hostDeployStatuses[entry.key]?['running'])
                                ? null
                                : () => _runDeploy(entry.key),
                          ),
                        ),
                    ],
                  ),
                ),
                const SizedBox(height: 10),
                Row(
                  children: [
                    Expanded(child: Center(child: _IssueSummary(_runState()))),
                    MasterButton(
                      label: 'Config',
                      width: 78,
                      onTap: busy ? null : _showConfig,
                    ),
                  ],
                ),
              ],
            ),
    );
  }
}

class _CollectorLogDialog extends StatefulWidget {
  const _CollectorLogDialog({
    required this.api,
    required this.initialRunStatus,
    required this.initialDeployStatus,
    required this.pollRun,
    required this.pollDeploy,
    required this.hostIndex,
  });

  final AutoSyncApi api;
  final Map<String, dynamic> initialRunStatus;
  final Map<String, dynamic> initialDeployStatus;
  final bool pollRun;
  final bool pollDeploy;
  final int hostIndex;

  @override
  State<_CollectorLogDialog> createState() => _CollectorLogDialogState();
}

class _CollectorLogDialogState extends State<_CollectorLogDialog> {
  late Map<String, dynamic> runStatus;
  late Map<String, dynamic> deployStatus;
  Timer? timer;

  @override
  void initState() {
    super.initState();
    runStatus = Map<String, dynamic>.from(widget.initialRunStatus);
    deployStatus = Map<String, dynamic>.from(widget.initialDeployStatus);
    if (widget.pollRun || widget.pollDeploy) {
      timer = Timer.periodic(
        const Duration(seconds: 1),
        (_) => unawaited(_refresh()),
      );
      unawaited(_refresh());
    }
  }

  @override
  void dispose() {
    timer?.cancel();
    super.dispose();
  }

  Future<void> _refresh() async {
    try {
      final nextRun = widget.pollRun
          ? await widget.api.collectorStatus(index: widget.hostIndex)
          : runStatus;
      final nextDeploy = widget.pollDeploy
          ? await widget.api.collectorDeployStatus(index: widget.hostIndex)
          : deployStatus;
      if (!mounted) return;
      setState(() {
        runStatus = nextRun;
        deployStatus = nextDeploy;
      });
      if (!_bool(nextRun['running']) && !_bool(nextDeploy['running'])) {
        timer?.cancel();
        timer = null;
      }
    } catch (_) {}
  }

  String _stateText(Map<String, dynamic> state, String runningText) {
    if (_bool(state['running'])) return runningText;
    if (state.containsKey('ok') && _bool(state['ok'])) return 'Done';
    if (state.containsKey('ok') && !_bool(state['ok'], true)) {
      return 'Finished with errors';
    }
    return '';
  }

  List<String> _stateLog(Map<String, dynamic> state, String runningText) {
    final lines = _list(state['log']).map((line) => '$line').toList();
    for (final issue in _mapRefs(state['errors'])) {
      final kind = _str(issue['kind']);
      final host = _str(issue['host']);
      final path = _str(issue['path']);
      final message = _str(issue['message']);
      lines.add('[$kind] $host $path :: $message'.trim());
    }
    if (lines.isNotEmpty) return lines;
    final stateText = _stateText(state, runningText);
    return stateText.isEmpty ? <String>[] : <String>[stateText];
  }

  String _logText() {
    final parts = <String>[];
    final runLines = _stateLog(runStatus, 'Collecting...');
    if (runLines.isNotEmpty) {
      parts.add('Collect');
      parts.addAll(runLines);
    }
    final deployLines = _stateLog(deployStatus, 'Deploying...');
    if (deployLines.isNotEmpty) {
      if (parts.isNotEmpty) parts.add('');
      parts.add('Deploy');
      parts.addAll(deployLines);
    }
    return parts.join('\n');
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: 'Collector Log',
      width: 900,
      maxHeight: 720,
      child: _MasterPre(text: _logText(), minHeight: 320, maxHeight: 640),
    );
  }
}

class _CollectorHostHeader extends StatelessWidget {
  const _CollectorHostHeader();

  @override
  Widget build(BuildContext context) {
    return const _CollectorHostGrid(
      head: true,
      cells: [
        Text('Host'),
        Text('HostName'),
        Text('User'),
        Text('Port'),
        Text('IdentityFile'),
        Text('Root dir'),
        Text('Files'),
        Text(''),
        Text(''),
        Text(''),
        Text(''),
        Text(''),
      ],
    );
  }
}

class _CollectorHostRow extends StatelessWidget {
  const _CollectorHostRow({
    required this.index,
    required this.host,
    required this.onChanged,
    required this.onRemove,
    required this.onBrowseRoot,
    required this.onPaths,
    required this.onDeploy,
    required this.onDeployLog,
    required this.onCollect,
    required this.onDeployRun,
  });

  final int index;
  final Map<String, dynamic> host;
  final VoidCallback onChanged;
  final VoidCallback onRemove;
  final VoidCallback onBrowseRoot;
  final VoidCallback onPaths;
  final VoidCallback onDeploy;
  final VoidCallback onDeployLog;
  final VoidCallback? onCollect;
  final VoidCallback? onDeployRun;

  @override
  Widget build(BuildContext context) {
    final pathCount = _list(
      host['paths'],
    ).where((path) => _str(path).trim().isNotEmpty).length;
    void setField(String key, dynamic value) {
      host[key] = value;
      onChanged();
    }

    return _CollectorHostGrid(
      cells: [
        _CompactInput(
          initialValue: _str(host['name']),
          placeholder: 'alias',
          onChanged: (value) => setField('name', value),
        ),
        _CompactInput(
          initialValue: _str(host['hostname']),
          placeholder: '1.2.3.4',
          onChanged: (value) => setField('hostname', value),
        ),
        _CompactInput(
          initialValue: _str(host['user']),
          placeholder: 'root',
          onChanged: (value) => setField('user', value),
        ),
        _CompactInput(
          initialValue: _str(host['port'], '22'),
          numeric: true,
          onChanged: (value) => setField('port', int.tryParse(value) ?? 22),
        ),
        _CompactInput(
          initialValue: _str(host['identity_file']),
          placeholder: '~/.ssh/id_ed25519',
          onChanged: (value) => setField('identity_file', value),
        ),
        InkWell(
          borderRadius: BorderRadius.circular(6),
          onTap: onBrowseRoot,
          child: _MasterReadOnlyInput(value: _str(host['root'])),
        ),
        MasterButton(label: '$pathCount', onTap: onPaths),
        _CollectorTinyButton(label: '📝', onTap: onDeploy),
        _CollectorTinyButton(
          label: '⤓',
          accent: true,
          color: Palette.accent,
          onTap: onCollect,
        ),
        _CollectorTinyButton(
          label: '▶',
          accent: true,
          color: Palette.green,
          onTap: onDeployRun,
        ),
        _CollectorTinyButton(label: 'Log', onTap: onDeployLog),
        _CollectorTinyButton(label: 'x', color: Palette.red, onTap: onRemove),
      ],
    );
  }
}

class _CollectorTinyButton extends StatelessWidget {
  const _CollectorTinyButton({
    required this.label,
    required this.onTap,
    this.color,
    this.accent = false,
  });

  final String label;
  final VoidCallback? onTap;
  final Color? color;
  final bool accent;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: _masterControlHeight,
      width: double.infinity,
      child: OutlinedButton(
        onPressed: onTap,
        style: OutlinedButton.styleFrom(
          padding: EdgeInsets.zero,
          minimumSize: const Size(0, _masterControlHeight),
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          visualDensity: VisualDensity.compact,
          backgroundColor: Colors.white,
          disabledBackgroundColor: const Color(0xffe2e8f0),
          foregroundColor: color ?? (accent ? Palette.accent : Palette.text),
          disabledForegroundColor: const Color(0xff64748b),
          side: const BorderSide(color: Palette.line),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(6)),
          textStyle: const TextStyle(fontSize: 12, fontWeight: FontWeight.w700),
        ),
        child: Text(label, maxLines: 1, overflow: TextOverflow.clip),
      ),
    );
  }
}

const double _collectorTableBaseWidth = 952;

class _CollectorHostGrid extends StatelessWidget {
  const _CollectorHostGrid({required this.cells, this.head = false});

  final List<Widget> cells;
  final bool head;

  @override
  Widget build(BuildContext context) {
    final style = TextStyle(
      color: head ? Palette.muted : Palette.text,
      fontSize: head ? 11 : 12,
    );
    return LayoutBuilder(
      builder: (context, constraints) {
        final extra = math.max(
          0.0,
          constraints.maxWidth - _collectorTableBaseWidth,
        );
        final widths = <double>[
          82,
          126,
          70,
          64,
          152 + extra * 0.65,
          180 + extra * 0.35,
          42,
          34,
          34,
          34,
          34,
          34,
        ];
        return DefaultTextStyle.merge(
          style: style,
          child: Padding(
            padding: const EdgeInsets.only(bottom: 8),
            child: Row(
              children: [
                for (var i = 0; i < widths.length; i++) ...[
                  SizedBox(width: widths[i], child: cells[i]),
                  if (i != widths.length - 1) const SizedBox(width: 6),
                ],
              ],
            ),
          ),
        );
      },
    );
  }
}

class _CollectorPathsDialog extends StatefulWidget {
  const _CollectorPathsDialog({
    required this.api,
    required this.host,
    required this.onChanged,
  });

  final AutoSyncApi api;
  final Map<String, dynamic> host;
  final VoidCallback onChanged;

  @override
  State<_CollectorPathsDialog> createState() => _CollectorPathsDialogState();
}

class _CollectorPathsDialogState extends State<_CollectorPathsDialog> {
  bool showingExclude = false;

  List<String> get paths =>
      _list(widget.host['paths']).map((p) => '$p').toList();
  List<String> get exclude =>
      _list(widget.host['exclude']).map((p) => '$p').toList();

  void _setList(String key, List<String> value) {
    widget.host[key] = value;
    widget.onChanged();
  }

  Future<void> _browse(String key) async {
    final current = key == 'exclude' ? exclude : paths;
    final selected = current
        .map((path) => path.trim())
        .where((path) => path.isNotEmpty)
        .toSet();
    final disabled = key == 'paths'
        ? _sortRemotePaths(exclude).toSet()
        : <String>{};
    await showDialog<void>(
      context: context,
      builder: (context) => _CollectorRemotePathDialog(
        api: widget.api,
        host: widget.host,
        title: key == 'exclude' ? 'Ignore' : 'Collect',
        initialPath: current.isNotEmpty ? current.last : '/',
        selectedPaths: selected,
        disabledPaths: disabled,
        onSelectionChanged: (items) {
          setState(() => _setList(key, _sortRemotePaths(items)));
        },
      ),
    );
  }

  Future<void> _previewPath(String remotePath) async {
    final filePath = _collectorLocalPath(widget.host, remotePath);
    if (filePath.isEmpty) return;
    try {
      final preview = await widget.api.localFilePreview(filePath);
      if (_str(preview['kind']) != 'file') return;
      if (!_bool(preview['previewable'])) return;
      final text = _str(preview['text']);
      if (!mounted) return;
      await showDialog<void>(
        context: context,
        builder: (context) => _CollectorFilePreviewDialog(
          api: widget.api,
          filePath: filePath,
          remotePath: remotePath,
          initialText: text,
        ),
      );
    } catch (_) {
      return;
    }
  }

  @override
  Widget build(BuildContext context) {
    final label = _str(
      widget.host['name'],
      _str(widget.host['hostname'], 'host'),
    ).trim();
    return _MasterDialogFrame(
      title: 'Files & folders - $label',
      width: 780,
      maxHeight: 720,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              _TaskMachineTab(
                label: 'Collect (${paths.length})',
                selected: !showingExclude,
                onTap: () => setState(() => showingExclude = false),
              ),
              const SizedBox(width: 6),
              _TaskMachineTab(
                label: 'Ignore (${exclude.length})',
                selected: showingExclude,
                onTap: () => setState(() => showingExclude = true),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Align(
            alignment: Alignment.centerRight,
            child: MasterButton(
              label: 'Browse',
              width: 72,
              onTap: () =>
                  unawaited(_browse(showingExclude ? 'exclude' : 'paths')),
            ),
          ),
          const SizedBox(height: 8),
          Expanded(
            child: ListView(
              children: [
                _PathListEditor(
                  key: ValueKey(
                    showingExclude ? 'collector-ignore' : 'collector-collect',
                  ),
                  items: showingExclude ? exclude : paths,
                  iconFor: (path) => _collectorPathIcon(widget.host, path),
                  onPreview: _previewPath,
                  onChanged: (items) => setState(
                    () => _setList(showingExclude ? 'exclude' : 'paths', items),
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}

IconData _collectorPathIcon(Map<String, dynamic> host, String remotePath) {
  final localPath = _collectorLocalPath(host, remotePath);
  if (localPath.isEmpty) return Icons.description_outlined;
  if (localPathIsDirectory(localPath)) return Icons.folder_outlined;
  return Icons.description_outlined;
}

String _collectorLocalPath(Map<String, dynamic> host, String remotePath) {
  final root = _str(host['root']).trim();
  if (root.isEmpty) return '';
  final parts = remotePath
      .trim()
      .replaceAll('\\', '/')
      .split('/')
      .where((part) => part.isNotEmpty && part != '.')
      .toList();
  if (parts.isEmpty || parts.any((part) => part == '..')) return '';
  var path = root;
  for (final part in parts) {
    final needsSeparator = !path.endsWith('\\') && !path.endsWith('/');
    path = '$path${needsSeparator ? hostPathSeparator : ''}$part';
  }
  return path;
}

class _CollectorFilePreviewDialog extends StatefulWidget {
  const _CollectorFilePreviewDialog({
    required this.api,
    required this.filePath,
    required this.remotePath,
    required this.initialText,
  });

  final AutoSyncApi api;
  final String filePath;
  final String remotePath;
  final String initialText;

  @override
  State<_CollectorFilePreviewDialog> createState() =>
      _CollectorFilePreviewDialogState();
}

class _CollectorFilePreviewDialogState
    extends State<_CollectorFilePreviewDialog> {
  late final TextEditingController controller;
  bool editing = false;
  bool saving = false;
  String message = '';

  @override
  void initState() {
    super.initState();
    controller = TextEditingController(text: widget.initialText);
  }

  @override
  void dispose() {
    controller.dispose();
    super.dispose();
  }

  Future<void> _save() async {
    if (saving) return;
    setState(() {
      saving = true;
      message = '';
    });
    try {
      await widget.api.saveLocalFileText(widget.filePath, controller.text);
      if (!mounted) return;
      setState(() {
        editing = false;
        saving = false;
      });
    } catch (error) {
      if (!mounted) return;
      setState(() {
        saving = false;
        message = '$error';
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    return _MasterDialogFrame(
      title: widget.remotePath,
      width: 900,
      maxHeight: 720,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            mainAxisAlignment: MainAxisAlignment.end,
            children: [
              MasterButton(
                label: 'Edit',
                width: 72,
                onTap: editing ? null : () => setState(() => editing = true),
              ),
              if (editing) ...[
                const SizedBox(width: 8),
                MasterButton(
                  label: 'Save',
                  width: 72,
                  primary: true,
                  onTap: saving ? null : () => unawaited(_save()),
                ),
              ],
            ],
          ),
          if (message.isNotEmpty) ...[
            const SizedBox(height: 8),
            Text(message, style: const TextStyle(color: Palette.red)),
          ],
          const SizedBox(height: 8),
          Expanded(
            child: TextField(
              controller: controller,
              readOnly: !editing,
              expands: true,
              maxLines: null,
              minLines: null,
              keyboardType: TextInputType.multiline,
              textAlignVertical: TextAlignVertical.top,
              style: const TextStyle(fontFamily: 'Consolas', fontSize: 12),
              decoration: InputDecoration(
                filled: true,
                fillColor: editing ? Colors.white : const Color(0xfff8fafc),
                border: const OutlineInputBorder(),
                enabledBorder: const OutlineInputBorder(
                  borderSide: BorderSide(color: Palette.line),
                ),
                focusedBorder: const OutlineInputBorder(
                  borderSide: BorderSide(color: Palette.accent),
                ),
                isDense: true,
                contentPadding: const EdgeInsets.all(10),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _CollectorRemotePathDialog extends StatefulWidget {
  const _CollectorRemotePathDialog({
    required this.api,
    required this.host,
    required this.title,
    required this.initialPath,
    required this.selectedPaths,
    required this.disabledPaths,
    required this.onSelectionChanged,
  });

  final AutoSyncApi api;
  final Map<String, dynamic> host;
  final String title;
  final String initialPath;
  final Set<String> selectedPaths;
  final Set<String> disabledPaths;
  final ValueChanged<List<String>> onSelectionChanged;

  @override
  State<_CollectorRemotePathDialog> createState() =>
      _CollectorRemotePathDialogState();
}

class _CollectorRemotePathDialogState
    extends State<_CollectorRemotePathDialog> {
  String path = '/';
  String? parent;
  String error = '';
  bool loading = true;
  List<Map<String, dynamic>> entries = [];
  late final Set<String> selected = {...widget.selectedPaths};
  late final Set<String> disabled = _sortRemotePaths(
    widget.disabledPaths,
  ).toSet();
  final pageCache = <String, Map<String, dynamic>>{};

  @override
  void initState() {
    super.initState();
    path = widget.initialPath.trim().isEmpty ? '/' : widget.initialPath.trim();
    _load(path);
  }

  Map<String, dynamic> _request(String nextPath) => {
    'hostname': _str(widget.host['hostname']),
    'user': _str(widget.host['user']),
    'port': _int(widget.host['port'], 22),
    'identity_file': _str(widget.host['identity_file']),
    'path': nextPath,
  };

  Future<void> _load(String nextPath) async {
    final cached = pageCache[nextPath];
    if (cached != null) {
      setState(() {
        path = _str(cached['path'], nextPath);
        parent = cached['parent'] == null ? null : _str(cached['parent']);
        entries = _mapRefs(cached['entries']);
        loading = false;
        error = '';
      });
      return;
    }
    setState(() {
      loading = true;
      error = '';
    });
    try {
      final response = await widget.api.collectorBrowse(_request(nextPath));
      if (!mounted) return;
      pageCache[nextPath] = response;
      pageCache[_str(response['path'], nextPath)] = response;
      setState(() {
        path = _str(response['path'], nextPath);
        parent = response['parent'] == null ? null : _str(response['parent']);
        entries = _mapRefs(response['entries']);
        loading = false;
      });
    } catch (err) {
      if (!mounted) return;
      setState(() {
        error = '$err';
        loading = false;
      });
    }
  }

  void _togglePath(String itemPath, bool value) {
    if (_remotePathCoveredByAny(itemPath, disabled)) return;
    setState(() {
      if (value) {
        selected.add(itemPath);
      } else {
        selected.remove(itemPath);
      }
    });
    widget.onSelectionChanged(_sortRemotePaths(selected));
  }

  @override
  Widget build(BuildContext context) {
    final hostLabel = [
      _str(widget.host['user']),
      _str(widget.host['hostname']),
    ].where((part) => part.isNotEmpty).join('@');
    return _MasterDialogFrame(
      title: '${widget.title} - ${hostLabel.isEmpty ? 'host' : hostLabel}',
      width: 760,
      maxHeight: 720,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _FolderPathLabel(path),
          const SizedBox(height: 8),
          Row(
            children: [
              MasterButton(
                label: 'Up',
                width: 64,
                onTap: parent == null ? null : () => _load(parent!),
              ),
              const SizedBox(width: 8),
              Text(
                '${selected.length} selected',
                style: const TextStyle(color: Palette.muted, fontSize: 12),
              ),
            ],
          ),
          if (error.isNotEmpty) ...[
            const SizedBox(height: 8),
            Text(error, style: const TextStyle(color: Palette.red)),
          ],
          const SizedBox(height: 8),
          Expanded(
            child: _FileListView(
              loading: loading,
              entries: entries,
              isDir: (entry) => _bool(entry['is_dir']),
              isSelected: selected.contains,
              isDisabled: (itemPath) =>
                  _remotePathCoveredByAny(itemPath, disabled),
              onSelected: _togglePath,
              onOpenDir: _load,
            ),
          ),
        ],
      ),
    );
  }
}

List<String> _sortRemotePaths(Iterable<String> items) {
  final paths = items
      .map((path) => path.trim())
      .where((path) => path.isNotEmpty)
      .toSet()
      .toList();
  paths.sort();
  return paths;
}

bool _remotePathCoveredByAny(String path, Iterable<String> roots) {
  final target = _normalizeRemoteComparePath(path);
  if (target.isEmpty) return false;
  for (final root in roots.map(_normalizeRemoteComparePath)) {
    if (root.isEmpty) continue;
    if (root == '/') return true;
    if (target == root || target.startsWith('$root/')) return true;
  }
  return false;
}

String _normalizeRemoteComparePath(String path) {
  var value = path.trim().replaceAll('\\', '/');
  while (value.contains('//')) {
    value = value.replaceAll('//', '/');
  }
  while (value.length > 1 && value.endsWith('/')) {
    value = value.substring(0, value.length - 1);
  }
  return value;
}

class _FileListView extends StatelessWidget {
  const _FileListView({
    required this.loading,
    required this.entries,
    required this.isDir,
    required this.isSelected,
    this.isDisabled,
    required this.onSelected,
    required this.onOpenDir,
  });

  final bool loading;
  final List<Map<String, dynamic>> entries;
  final bool Function(Map<String, dynamic> entry) isDir;
  final bool Function(String path) isSelected;
  final bool Function(String path)? isDisabled;
  final void Function(String path, bool selected) onSelected;
  final ValueChanged<String> onOpenDir;

  @override
  Widget build(BuildContext context) {
    return Container(
      decoration: const BoxDecoration(
        border: Border(top: BorderSide(color: Palette.line)),
      ),
      child: loading
          ? const Center(child: CircularProgressIndicator())
          : entries.isEmpty
          ? const SizedBox.shrink()
          : ListView.builder(
              itemCount: entries.length,
              itemBuilder: (context, index) {
                final entry = entries[index];
                final dir = isDir(entry);
                final itemPath = _str(entry['path']);
                return _FileListEntryRow(
                  name: _str(entry['name']),
                  isDir: dir,
                  selected: isSelected(itemPath),
                  disabled: isDisabled?.call(itemPath) ?? false,
                  onSelected: (value) => onSelected(itemPath, value),
                  onOpen: dir ? () => onOpenDir(itemPath) : null,
                );
              },
            ),
    );
  }
}

class _FileListEntryRow extends StatelessWidget {
  const _FileListEntryRow({
    required this.name,
    required this.isDir,
    required this.selected,
    this.disabled = false,
    required this.onSelected,
    required this.onOpen,
  });

  final String name;
  final bool isDir;
  final bool selected;
  final bool disabled;
  final ValueChanged<bool> onSelected;
  final VoidCallback? onOpen;

  @override
  Widget build(BuildContext context) {
    return Container(
      height: 34,
      decoration: const BoxDecoration(
        border: Border(bottom: BorderSide(color: Palette.line)),
      ),
      child: Row(
        children: [
          SizedBox(
            width: 26,
            child: Center(
              child: _FileListCheckbox(
                value: selected,
                disabled: disabled,
                onChanged: onSelected,
              ),
            ),
          ),
          Expanded(
            child: InkWell(
              onTap: onOpen,
              child: Container(
                height: 34,
                alignment: Alignment.centerLeft,
                padding: const EdgeInsets.symmetric(horizontal: 2),
                child: Opacity(
                  opacity: disabled ? 0.45 : 1,
                  child: _FileListEntryLabel(name: name, isDir: isDir),
                ),
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _FileListCheckbox extends StatelessWidget {
  const _FileListCheckbox({
    required this.value,
    required this.disabled,
    required this.onChanged,
  });

  final bool value;
  final bool disabled;
  final ValueChanged<bool> onChanged;

  @override
  Widget build(BuildContext context) {
    return InkWell(
      onTap: disabled ? null : () => onChanged(!value),
      borderRadius: BorderRadius.circular(3),
      child: Container(
        width: 16,
        height: 16,
        decoration: BoxDecoration(
          color: disabled
              ? const Color(0xfff1f5f9)
              : value
              ? Palette.accent
              : Colors.white,
          border: Border.all(
            color: disabled
                ? const Color(0xffcbd5e1)
                : value
                ? Palette.accent
                : Palette.line,
          ),
          borderRadius: BorderRadius.circular(3),
        ),
        child: value
            ? const Icon(Icons.check, size: 13, color: Colors.white)
            : null,
      ),
    );
  }
}

class _PathListEditor extends StatelessWidget {
  const _PathListEditor({
    super.key,
    required this.items,
    required this.onChanged,
    this.iconFor,
    this.onPreview,
  });

  final List<String> items;
  final ValueChanged<List<String>> onChanged;
  final IconData Function(String value)? iconFor;
  final ValueChanged<String>? onPreview;

  @override
  Widget build(BuildContext context) {
    if (items.isEmpty) {
      return const SizedBox.shrink();
    }
    return Column(
      children: items.asMap().entries.map((entry) {
        return Padding(
          padding: const EdgeInsets.only(bottom: 6),
          child: Row(
            children: [
              if (iconFor != null) ...[
                SizedBox(
                  width: 18,
                  child: Icon(
                    iconFor!(entry.value),
                    size: 16,
                    color: Palette.muted,
                  ),
                ),
                const SizedBox(width: 6),
              ],
              Expanded(
                child: _CompactInput(
                  initialValue: entry.value,
                  placeholder: '/remote/absolute/path',
                  onTap: () => onPreview?.call(entry.value),
                  onChanged: (value) {
                    final next = [...items];
                    next[entry.key] = value;
                    onChanged(next);
                  },
                ),
              ),
              const SizedBox(width: 6),
              MasterButton(
                label: 'x',
                square: true,
                danger: true,
                onTap: () {
                  final next = [...items]..removeAt(entry.key);
                  onChanged(next);
                },
              ),
            ],
          ),
        );
      }).toList(),
    );
  }
}

class _CollectorDeployDialog extends StatefulWidget {
  const _CollectorDeployDialog({
    required this.api,
    required this.hostIndex,
    required this.host,
    required this.onChanged,
  });

  final AutoSyncApi api;
  final int hostIndex;
  final Map<String, dynamic> host;
  final VoidCallback onChanged;

  @override
  State<_CollectorDeployDialog> createState() => _CollectorDeployDialogState();
}

class _CollectorDeployDialogState extends State<_CollectorDeployDialog> {
  late final TextEditingController script;
  late final bool scriptFromFile;
  String status = '';

  @override
  void initState() {
    super.initState();
    final scriptPath = _str(widget.host['deploy_script_path']).trim();
    scriptFromFile = scriptPath.isNotEmpty;
    status = scriptFromFile ? 'Script file: ${_displayPath(scriptPath)}' : '';
    script = TextEditingController(
      text: scriptFromFile ? 'Loading...' : _str(widget.host['deploy_script']),
    );
    if (scriptFromFile) {
      unawaited(_loadScriptFile());
    }
  }

  Future<void> _loadScriptFile() async {
    try {
      final info = await widget.api.collectorDeployScript(widget.hostIndex);
      if (!mounted) return;
      setState(() {
        script.text = _str(info['script']);
        final path = _str(info['path']).trim();
        status = path.isEmpty ? '' : 'Script file: ${_displayPath(path)}';
      });
    } catch (error) {
      if (!mounted) return;
      setState(() {
        script.text = '';
        status = 'Failed to load script: $error';
      });
    }
  }

  @override
  void dispose() {
    if (!scriptFromFile) {
      widget.host['deploy_script'] = script.text;
      widget.onChanged();
    }
    script.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final label = _str(
      widget.host['name'],
      _str(widget.host['hostname'], 'host'),
    ).trim();
    return _MasterDialogFrame(
      title: 'Deploy - $label',
      width: 900,
      maxHeight: 720,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          if (status.isNotEmpty) ...[
            Text(
              status,
              style: const TextStyle(fontSize: 12, color: Palette.muted),
            ),
          ],
          const SizedBox(height: 8),
          Expanded(
            child: TextField(
              controller: script,
              readOnly: scriptFromFile,
              expands: true,
              maxLines: null,
              minLines: null,
              style: const TextStyle(fontFamily: 'Consolas', fontSize: 12),
              decoration: const InputDecoration(),
            ),
          ),
        ],
      ),
    );
  }
}

class _TasksTabbedView extends StatefulWidget {
  const _TasksTabbedView({required this.machines});

  final List<Map<String, dynamic>> machines;

  @override
  State<_TasksTabbedView> createState() => _TasksTabbedViewState();
}

class _TasksTabbedViewState extends State<_TasksTabbedView> {
  int selected = 0;

  @override
  Widget build(BuildContext context) {
    if (widget.machines.isEmpty) {
      return const SizedBox.shrink();
    }
    final index = math.min(selected, widget.machines.length - 1);
    final machine = widget.machines[index];
    final tasks = _list(machine['tasks']);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: Row(
            children: [
              for (var i = 0; i < widget.machines.length; i++) ...[
                _TaskMachineTab(
                  label: _taskMachineLabel(widget.machines[i]),
                  selected: i == index,
                  onTap: () => setState(() => selected = i),
                ),
                if (i != widget.machines.length - 1) const SizedBox(width: 8),
              ],
            ],
          ),
        ),
        const SizedBox(height: 10),
        Expanded(
          child: ListView(
            children: [
              const _TaskHeaderRow(),
              if (tasks.isEmpty)
                const SizedBox.shrink()
              else
                ...tasks.map((task) => _TaskRow(task: _map(task))),
            ],
          ),
        ),
      ],
    );
  }
}

class _TaskMachineTab extends StatelessWidget {
  const _TaskMachineTab({
    required this.label,
    required this.selected,
    required this.onTap,
  });

  final String label;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      height: _masterControlHeight,
      child: OutlinedButton(
        onPressed: onTap,
        style: OutlinedButton.styleFrom(
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          visualDensity: VisualDensity.compact,
          padding: const EdgeInsets.symmetric(horizontal: 14),
          backgroundColor: selected ? const Color(0xfff8fafc) : Colors.white,
          foregroundColor: selected ? Palette.accent : Palette.text,
          side: BorderSide(color: selected ? Palette.accent : Palette.line),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(6)),
          textStyle: const TextStyle(fontSize: 13, fontWeight: FontWeight.w700),
        ),
        child: Text(label, maxLines: 1, overflow: TextOverflow.ellipsis),
      ),
    );
  }
}

String _taskMachineLabel(Map<String, dynamic> machine) {
  return _str(machine['machine_id'], _str(machine['id'], 'machine'));
}

class _TaskHeaderRow extends StatelessWidget {
  const _TaskHeaderRow();

  @override
  Widget build(BuildContext context) {
    return const _TaskGrid(
      head: true,
      cells: [
        Text('ID'),
        Text('Status'),
        Text('Type'),
        Text('Target'),
        Text('Started'),
        Text('Duration'),
        Text('Result'),
      ],
    );
  }
}

class _TaskRow extends StatelessWidget {
  const _TaskRow({required this.task});

  final Map<String, dynamic> task;

  @override
  Widget build(BuildContext context) {
    final status = _str(task['status']);
    return _TaskGrid(
      cells: [
        _GridText(_str(task['id'])),
        Text(
          status,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(
            color: _taskStatusColor(status),
            fontWeight: FontWeight.w600,
          ),
        ),
        _GridText(_taskKindLabel(_str(task['kind']))),
        _GridText(
          '${_str(task['source_id'])} -> ${_str(task['destination_id'])}',
        ),
        _GridText(_str(task['started_at'])),
        _GridText(_taskDurationLabel(task)),
        _GridText(_taskResultLabel(task)),
      ],
    );
  }
}

class _TaskGrid extends StatelessWidget {
  const _TaskGrid({required this.cells, this.head = false});

  final List<Widget> cells;
  final bool head;

  @override
  Widget build(BuildContext context) {
    final style = TextStyle(
      color: head ? Palette.muted : Palette.text,
      fontSize: head ? 12 : 13,
      fontWeight: head ? FontWeight.w600 : FontWeight.w400,
    );
    return DefaultTextStyle.merge(
      style: style,
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 2, vertical: 5),
        decoration: const BoxDecoration(
          border: Border(bottom: BorderSide(color: Palette.line)),
        ),
        child: Row(
          children: [
            SizedBox(width: 46, child: cells[0]),
            const SizedBox(width: 8),
            SizedBox(width: 88, child: cells[1]),
            const SizedBox(width: 8),
            SizedBox(width: 110, child: cells[2]),
            const SizedBox(width: 8),
            Expanded(flex: 10, child: cells[3]),
            const SizedBox(width: 8),
            SizedBox(width: 150, child: cells[4]),
            const SizedBox(width: 8),
            SizedBox(width: 90, child: cells[5]),
            const SizedBox(width: 8),
            Expanded(flex: 14, child: cells[6]),
          ],
        ),
      ),
    );
  }
}

class _GridText extends StatelessWidget {
  const _GridText(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    return Text(text, maxLines: 1, overflow: TextOverflow.ellipsis);
  }
}

Color _taskStatusColor(String status) {
  switch (status) {
    case 'running':
      return const Color(0xff2563eb);
    case 'success':
      return Palette.accent;
    case 'failed':
    case 'aborted':
      return Palette.red;
    case 'cancelled':
    case 'warning':
      return const Color(0xffb45309);
    default:
      return Palette.text;
  }
}

String _taskKindLabel(String kind) {
  switch (kind) {
    case 'compare':
      return 'Compare';
    case 'incremental':
      return 'Incremental';
    case 'full':
      return 'Full';
    case 'repair_scan':
      return 'Repair';
    case 'repair_full':
      return 'Repair -> Full';
    default:
      return kind.isEmpty ? '-' : kind;
  }
}

String _taskDurationLabel(Map<String, dynamic> task) {
  if (_str(task['status']) == 'running') {
    return 'running';
  }
  final ms = task['duration_ms'];
  if (ms == null) return '-';
  final seconds =
      (ms is num ? ms.toDouble() : double.tryParse('$ms') ?? 0) / 1000;
  if (seconds < 60) return '${seconds.round()}s';
  final minutes = seconds ~/ 60;
  if (minutes < 60) return '${minutes}m ${seconds.round() % 60}s';
  return '${minutes ~/ 60}h ${minutes % 60}m';
}

String _taskResultLabel(Map<String, dynamic> task) {
  final parts = <String>[];
  if (_str(task['kind']) == 'compare') {
    final diffs = _int(task['differences']);
    if (_str(task['status']) == 'success') {
      parts.add('$diffs differences');
    }
    final entries = _int(task['entries_scanned']);
    if (entries > 0) parts.add('$entries entries');
  } else {
    final synced = _int(task['files_synced']);
    if (synced > 0) parts.add('$synced files');
  }
  final error = _str(task['error']);
  if (error.isNotEmpty) parts.add(error);
  return parts.isEmpty ? '-' : parts.join(' · ');
}

class _StatusBar extends StatelessWidget {
  const _StatusBar({
    required this.message,
    required this.runtimeStatus,
    required this.activity,
    required this.saving,
    required this.onConfig,
  });

  final String message;
  final Map<String, dynamic> runtimeStatus;
  final Map<String, dynamic> activity;
  final bool saving;
  final VoidCallback onConfig;

  @override
  Widget build(BuildContext context) {
    final errors = _list(runtimeStatus['config_errors']);
    final build = _map(runtimeStatus['build']);
    final commit = _str(build['commit'], _str(build['version'], 'unknown'));
    final time = _str(build['commit_time_beijing'], 'unknown');
    return Container(
      height: 42,
      padding: const EdgeInsets.symmetric(horizontal: 12),
      decoration: const BoxDecoration(
        color: Color(0xf5ffffff),
        border: Border(top: BorderSide(color: Palette.line)),
      ),
      child: Row(
        children: [
          MasterIconButton(
            kind: MasterIconKind.gear,
            color: Palette.accent,
            onTap: onConfig,
          ),
          const SizedBox(width: 10),
          Expanded(
            child: Text(
              _statusBarMessage(),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              textAlign: TextAlign.center,
              style: const TextStyle(color: Palette.muted, fontSize: 12),
            ),
          ),
          if (errors.isNotEmpty) ...[
            const SizedBox(width: 10),
            Container(
              constraints: const BoxConstraints(maxWidth: 320),
              padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
              decoration: BoxDecoration(
                color: const Color(0x1fdc2626),
                border: Border.all(color: const Color(0xffdc2626)),
                borderRadius: BorderRadius.circular(6),
              ),
              child: Text(
                '${errors.length} config errors',
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: const TextStyle(
                  color: Color(0xffdc2626),
                  fontSize: 12,
                  fontWeight: FontWeight.w600,
                ),
              ),
            ),
          ],
          const SizedBox(width: 10),
          ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 260),
            child: Text(
              '$commit · $time',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              textAlign: TextAlign.right,
              style: const TextStyle(
                color: Palette.muted,
                fontFamily: 'Consolas',
                fontSize: 12,
              ),
            ),
          ),
          if (saving) ...[
            const SizedBox(width: 10),
            const SizedBox(
              width: 14,
              height: 14,
              child: CircularProgressIndicator(strokeWidth: 2),
            ),
          ],
        ],
      ),
    );
  }

  String _statusBarMessage() {
    if (saving) return 'Saving config...';
    final active = _liveActivityStatusBarMessage(activity, runtimeStatus);
    if (active.isNotEmpty) return active;
    if (message.isNotEmpty) return message;
    return 'Ready';
  }
}

String _liveActivityStatusBarMessage(
  Map<String, dynamic> syncActivity,
  Map<String, dynamic> runtimeStatus,
) {
  for (final row in _activityRows(syncActivity, runtimeStatus)) {
    final runtime = _bool(row['local']) ? runtimeStatus : _map(row['runtime']);
    final text = _runtimeStatusBarMessage(runtime);
    if (text.isEmpty) continue;
    final label = _bool(row['local'])
        ? ''
        : _str(row['label'], _str(row['machine_id']));
    return label.isEmpty ? text : '$label · $text';
  }
  return '';
}

String _runtimeStatusBarMessage(Map<String, dynamic> runtimeStatus) {
  final transfer = _map(runtimeStatus['transfer']);
  if (transfer.isNotEmpty) {
    final dst = _str(transfer['destination_id'], _str(transfer['destination']));
    final file = _compactStatusPath(_str(transfer['rel_path'], '-'), 56);
    final speed = _formatBytesPerSecond(_int(transfer['bytes_per_sec']));
    return [
      'Backing up',
      dst,
      file,
      speed,
    ].where((part) => part.isNotEmpty).join(' · ');
  }
  final scans = _runtimeScans(runtimeStatus);
  if (scans.isNotEmpty) {
    final scan = scans.first;
    final current = _compactStatusPath(
      _str(scan['current_path'], _str(scan['root_path'])),
      56,
    );
    final entries = _int(scan['entries_seen']);
    return entries > 0
        ? 'Scanning $current · $entries entries'
        : 'Scanning $current';
  }
  final syncing = _bool(runtimeStatus['syncing']);
  final phase = _str(runtimeStatus['sync_phase'], _str(runtimeStatus['phase']));
  if (syncing) return 'Syncing ${phase.isEmpty ? '' : phase}'.trim();
  return '';
}

bool _hasAnyLiveRuntimeActivity(
  Map<String, dynamic> syncActivity,
  Map<String, dynamic> runtimeStatus,
) {
  for (final row in _activityRows(syncActivity, runtimeStatus)) {
    final runtime = _bool(row['local']) ? runtimeStatus : _map(row['runtime']);
    if (_hasLiveRuntimeActivity(runtime)) return true;
  }
  return false;
}

bool _hasLiveRuntimeActivity(Map<String, dynamic> runtimeStatus) {
  if (_map(runtimeStatus['transfer']).isNotEmpty) return true;
  if (_runtimeScans(runtimeStatus).isNotEmpty) return true;
  if (_bool(runtimeStatus['syncing'])) return true;
  final phase = _str(runtimeStatus['sync_phase'], _str(runtimeStatus['phase']));
  return phase.isNotEmpty;
}

class Section extends StatelessWidget {
  const Section({
    super.key,
    required this.title,
    required this.child,
    this.trailing,
  });

  final String title;
  final Widget child;
  final Widget? trailing;

  @override
  Widget build(BuildContext context) {
    return Container(
      margin: const EdgeInsets.only(bottom: 12),
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: Palette.panel,
        border: Border.all(color: Palette.line),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Expanded(
                child: Text(
                  title,
                  style: const TextStyle(
                    fontSize: 15,
                    fontWeight: FontWeight.w800,
                    color: Palette.text,
                  ),
                ),
              ),
              ?trailing,
            ],
          ),
          const SizedBox(height: 10),
          child,
        ],
      ),
    );
  }
}

class EmptyLine extends StatelessWidget {
  const EmptyLine(this.text, {super.key});

  final String text;

  @override
  Widget build(BuildContext context) {
    return Container(
      alignment: Alignment.centerLeft,
      height: 36,
      child: Text(text, style: const TextStyle(color: Palette.muted)),
    );
  }
}

class MiniButton extends StatelessWidget {
  const MiniButton({
    super.key,
    required this.icon,
    required this.label,
    required this.onTap,
    this.danger = false,
  });

  final IconData icon;
  final String label;
  final VoidCallback? onTap;
  final bool danger;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.only(left: 6),
      child: OutlinedButton.icon(
        onPressed: onTap,
        icon: Icon(icon, size: 17),
        label: Text(label),
        style: OutlinedButton.styleFrom(
          minimumSize: const Size(0, 34),
          padding: const EdgeInsets.symmetric(horizontal: 10),
          foregroundColor: danger ? Palette.red : Palette.text,
          side: const BorderSide(color: Palette.line),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(6)),
          textStyle: const TextStyle(fontSize: 12, fontWeight: FontWeight.w600),
        ),
      ),
    );
  }
}

class StatusPill extends StatelessWidget {
  const StatusPill({
    super.key,
    required this.text,
    required this.color,
    this.icon,
  });

  final String text;
  final Color color;
  final IconData? icon;

  @override
  Widget build(BuildContext context) {
    return Container(
      height: 25,
      padding: const EdgeInsets.symmetric(horizontal: 8),
      decoration: BoxDecoration(
        color: color.withAlpha(18),
        border: Border.all(color: color.withAlpha(70)),
        borderRadius: BorderRadius.circular(999),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (icon != null) ...[
            Icon(icon, size: 14, color: color),
            const SizedBox(width: 4),
          ],
          Flexible(
            child: Text(
              text,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                color: color,
                fontSize: 12,
                fontWeight: FontWeight.w700,
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class CommitField extends StatefulWidget {
  const CommitField({
    super.key,
    required this.label,
    required this.value,
    required this.onCommit,
  });

  final String label;
  final String value;
  final ValueChanged<String> onCommit;

  @override
  State<CommitField> createState() => _CommitFieldState();
}

class _CommitFieldState extends State<CommitField> {
  late final FocusNode focusNode;
  late TextEditingController controller;
  String lastValue = '';

  @override
  void initState() {
    super.initState();
    lastValue = widget.value;
    controller = TextEditingController(text: widget.value);
    focusNode = FocusNode()..addListener(_onFocus);
  }

  @override
  void didUpdateWidget(covariant CommitField oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (!focusNode.hasFocus && widget.value != controller.text) {
      controller.text = widget.value;
      lastValue = widget.value;
    }
  }

  @override
  void dispose() {
    focusNode.removeListener(_onFocus);
    focusNode.dispose();
    controller.dispose();
    super.dispose();
  }

  void _onFocus() {
    if (!focusNode.hasFocus) {
      _commit();
    }
  }

  void _commit() {
    final value = controller.text.trim();
    if (value != lastValue) {
      lastValue = value;
      widget.onCommit(value);
    }
  }

  @override
  Widget build(BuildContext context) {
    return TextField(
      focusNode: focusNode,
      controller: controller,
      onSubmitted: (_) => _commit(),
      decoration: InputDecoration(labelText: widget.label),
    );
  }
}

class EnumField extends StatelessWidget {
  const EnumField({
    super.key,
    required this.label,
    required this.value,
    required this.values,
    required this.onChanged,
    this.labelOf,
  });

  final String label;
  final String value;
  final List<String> values;
  final ValueChanged<String> onChanged;
  final String Function(String value)? labelOf;

  @override
  Widget build(BuildContext context) {
    final items = {...values, value}.where((item) => item.isNotEmpty).toList();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(label, style: const TextStyle(color: Palette.muted, fontSize: 12)),
        const SizedBox(height: 4),
        SizedBox(
          height: _masterControlHeight,
          child: _MasterMenuSelect(
            value: value,
            options: items,
            labelFor: labelOf,
            onSelected: onChanged,
          ),
        ),
      ],
    );
  }
}

class LabeledSwitch extends StatelessWidget {
  const LabeledSwitch({
    super.key,
    required this.label,
    required this.value,
    required this.onChanged,
  });

  final String label;
  final bool value;
  final ValueChanged<bool> onChanged;

  @override
  Widget build(BuildContext context) {
    return InkWell(
      borderRadius: BorderRadius.circular(6),
      onTap: () => onChanged(!value),
      child: Container(
        height: 38,
        padding: const EdgeInsets.symmetric(horizontal: 8),
        decoration: BoxDecoration(
          border: Border.all(color: Palette.line),
          borderRadius: BorderRadius.circular(6),
          color: Colors.white,
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Switch(
              value: value,
              onChanged: onChanged,
              materialTapTargetSize: MaterialTapTargetSize.shrinkWrap,
            ),
            Text(label, style: const TextStyle(fontSize: 12)),
          ],
        ),
      ),
    );
  }
}
