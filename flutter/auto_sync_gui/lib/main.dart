import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;

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
      try {
        final text = File(configPath).readAsStringSync();
        final match = RegExp(
          r'^\s*port\s*=\s*([0-9]+)\s*(#.*)?$',
          multiLine: true,
        ).firstMatch(text);
        port = match?.group(1);
      } catch (_) {
        port = null;
      }
    }
    return AutoSyncApi('http://127.0.0.1:${port ?? "18765"}');
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
  Future<void> saveCollectorConfig(Map<String, dynamic> cfg) async =>
      _request('POST', '/api/collector/config', body: cfg);
  Future<Map<String, dynamic>> collectorStatus() async =>
      _map(await _request('GET', '/api/collector/status'));
  Future<void> collectorRun() async => _request('POST', '/api/collector/run');
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

bool _bool(dynamic value, [bool fallback = false]) =>
    value is bool ? value : fallback;

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
        fontFamily: Platform.isWindows ? 'Segoe UI' : null,
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
  Map<String, dynamic> runtimeStatus = {};
  Map<String, dynamic> syncActivity = {};
  Map<String, dynamic> machineStatus = {};
  bool loading = true;
  bool busy = false;
  bool saving = false;
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
    } catch (error) {
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

  Future<void> _loadStatusOnly() async {
    if (!mounted || busy) {
      return;
    }
    try {
      final nextStatus = await widget.api.getStatus();
      final nextActivity = await widget.api.getSyncActivity();
      if (mounted) {
        setState(() {
          statuses = nextStatus;
          syncActivity = nextActivity;
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
      if (mounted) {
        setState(() => runtimeStatus = next);
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
      await _loadStatusOnly();
      if (mounted) {
        setState(() => message = '$label done');
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
    final controller = TextEditingController(
      text: const JsonEncoder.withIndent('  ').convert(cfg),
    );
    final result = await showDialog<Map<String, dynamic>>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Config JSON'),
        content: SizedBox(
          width: 900,
          height: 620,
          child: TextField(
            controller: controller,
            expands: true,
            maxLines: null,
            minLines: null,
            style: const TextStyle(fontFamily: 'Consolas', fontSize: 12),
            decoration: const InputDecoration(border: OutlineInputBorder()),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton.icon(
            onPressed: () {
              try {
                Navigator.pop(context, _map(jsonDecode(controller.text)));
              } catch (error) {
                ScaffoldMessenger.of(
                  context,
                ).showSnackBar(SnackBar(content: Text('Invalid JSON: $error')));
              }
            },
            icon: const Icon(Icons.save_outlined, size: 18),
            label: const Text('Save'),
          ),
        ],
      ),
    );
    controller.dispose();
    if (result != null) {
      setState(() => cfg = result);
      await _saveConfig('Config saved');
    }
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
      builder: (context) => AlertDialog(
        title: const Text('Tasks'),
        content: SizedBox(
          width: 900,
          height: 620,
          child: errorText.isNotEmpty
              ? Text(errorText)
              : ListView(
                  children: tasks.map((machine) {
                    final m = _map(machine);
                    final list = _list(m['tasks']);
                    return Section(
                      title: _str(m['machine_id'], _str(m['id'], 'machine')),
                      child: Column(
                        children: list.isEmpty
                            ? const [EmptyLine('No tasks')]
                            : list
                                  .map((task) => _TaskRow(task: _map(task)))
                                  .toList(),
                      ),
                    );
                  }).toList(),
                ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Close'),
          ),
        ],
      ),
    );
  }

  Future<void> _openReadmeDialog() async {
    String text = '';
    try {
      text = await widget.api.text('/README.md');
    } catch (error) {
      text = '$error';
    }
    if (!mounted) {
      return;
    }
    await showDialog<void>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Readme'),
        content: SizedBox(
          width: 860,
          height: 620,
          child: SingleChildScrollView(
            child: Text(
              text,
              style: const TextStyle(fontSize: 12, height: 1.45),
            ),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Close'),
          ),
        ],
      ),
    );
  }

  Future<void> _openMachinesDialog() async {
    await showDialog<void>(
      context: context,
      builder: (context) => _MachinesDialog(
        api: widget.api,
        machines: machines,
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
                    machineIdsFor: (source) =>
                        _machineIds(_str(source['machine_id'])),
                    machineLabel: _machineLabel,
                    statusFor: _statusFor,
                    onChanged: _saveConfig,
                    onMutate: (mutate) {
                      setState(mutate);
                      _saveConfig();
                    },
                    onAddSource: _addSource,
                    onRemoveSource: _removeSource,
                    onSyncAll: () => _run('Sync all', widget.api.syncAll),
                    onSyncSource: (id) => _run(
                      'Sync source $id',
                      () => widget.api.syncSource(id),
                    ),
                    onSyncDestination: (sourceId, destinationId, mode) => _run(
                      'Sync $sourceId -> $destinationId',
                      () => widget.api.syncDestination(
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
                        scope: 'destination',
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
          ),
        ],
      ),
    );
  }

  void _addSource() {
    final list = _list(cfg['source_groups']);
    cfg['source_groups'] = list;
    final next = 'src_${list.length + 1}';
    setState(() {
      list.add({
        'id': next,
        'machine_id': 'local',
        'src': '',
        'add_directory': true,
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
    _saveConfig('Source added');
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
    required this.onMachines,
    required this.onReadme,
    required this.onCollector,
    required this.onConfig,
    required this.onTasks,
  });

  final Map<String, dynamic> machineStatus;
  final VoidCallback onMachines;
  final VoidCallback onReadme;
  final VoidCallback onCollector;
  final VoidCallback onConfig;
  final VoidCallback onTasks;

  @override
  Widget build(BuildContext context) {
    final online = _int(machineStatus['online'], 0);
    final total = _int(machineStatus['total'], 0);
    return LayoutBuilder(
      builder: (context, constraints) {
        final headerWidth = constraints.maxWidth < 860
            ? 860.0
            : constraints.maxWidth;
        return SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: SizedBox(
            width: headerWidth,
            child: Container(
              height: 58,
              padding: const EdgeInsets.symmetric(horizontal: 20),
              decoration: const BoxDecoration(
                color: Palette.panel,
                border: Border(bottom: BorderSide(color: Palette.line)),
              ),
              child: Row(
                children: [
                  const Expanded(
                    child: Align(
                      alignment: Alignment.centerLeft,
                      child: Text(
                        'auto_sync',
                        style: TextStyle(
                          fontSize: 18,
                          fontWeight: FontWeight.w800,
                          color: Palette.text,
                        ),
                      ),
                    ),
                  ),
                  SizedBox(
                    width: 180,
                    child: Center(
                      child: OutlinedButton(
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
                          total == 0
                              ? 'Machines -/-'
                              : 'Machines $online/$total',
                        ),
                      ),
                    ),
                  ),
                  Expanded(
                    child: Align(
                      alignment: Alignment.centerRight,
                      child: Row(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          MasterButton(
                            label: 'Readme',
                            width: 74,
                            onTap: onReadme,
                          ),
                          const SizedBox(width: 8),
                          MasterButton(
                            label: 'Collector',
                            width: 86,
                            onTap: onCollector,
                          ),
                          const SizedBox(width: 8),
                          MasterButton(
                            label: 'Config',
                            width: 68,
                            onTap: onConfig,
                          ),
                          const SizedBox(width: 8),
                          MasterButton(
                            label: 'Tasks',
                            width: 64,
                            onTap: onTasks,
                          ),
                        ],
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
        );
      },
    );
  }
}

class _MasterSourcePanel extends StatelessWidget {
  const _MasterSourcePanel({
    required this.sources,
    required this.machineIdsFor,
    required this.machineLabel,
    required this.statusFor,
    required this.onChanged,
    required this.onMutate,
    required this.onAddSource,
    required this.onRemoveSource,
    required this.onSyncAll,
    required this.onSyncSource,
    required this.onSyncDestination,
    required this.onScan,
    required this.onCancel,
  });

  final List<Map<String, dynamic>> sources;
  final List<String> Function(Map<String, dynamic> source) machineIdsFor;
  final String Function(String id) machineLabel;
  final Map<String, dynamic>? Function(String sourceId, String destinationId)
  statusFor;
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
  final VoidCallback onAddSource;
  final void Function(String sourceId) onRemoveSource;
  final VoidCallback onSyncAll;
  final void Function(String id) onSyncSource;
  final void Function(String sourceId, String destinationId, String mode)
  onSyncDestination;
  final void Function(String sourceId, String destinationId) onScan;
  final void Function(String sourceId, String destinationId) onCancel;

  @override
  Widget build(BuildContext context) {
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
                padding: const EdgeInsets.fromLTRB(28, 14, 14, 14),
                decoration: BoxDecoration(
                  color: Palette.panel,
                  border: Border.all(color: Palette.line),
                  borderRadius: BorderRadius.circular(8),
                ),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Container(
                      padding: const EdgeInsets.only(bottom: 12),
                      decoration: const BoxDecoration(
                        border: Border(bottom: BorderSide(color: Palette.line)),
                      ),
                      child: Row(
                        children: [
                          Expanded(
                            child: Transform.translate(
                              offset: const Offset(-16, 0),
                              child: const Text(
                                'Source',
                                style: TextStyle(
                                  fontSize: 15,
                                  fontWeight: FontWeight.w700,
                                  color: Palette.text,
                                ),
                              ),
                            ),
                          ),
                          MasterButton(
                            label: 'Sync All',
                            primary: true,
                            onTap: onSyncAll,
                          ),
                          const SizedBox(width: 8),
                          MasterButton(label: 'Add Source', onTap: onAddSource),
                        ],
                      ),
                    ),
                    const SizedBox(height: 14),
                    Column(
                      children: sources.isEmpty
                          ? const [
                              SizedBox(
                                height: 58,
                                child: Center(
                                  child: Text(
                                    'No sources configured',
                                    style: TextStyle(
                                      color: Palette.muted,
                                      fontSize: 13,
                                    ),
                                  ),
                                ),
                              ),
                            ]
                          : [
                              for (var i = 0; i < sources.length; i += 1) ...[
                                _MasterSourceGroup(
                                  source: sources[i],
                                  machineIds: machineIdsFor(sources[i]),
                                  machineLabel: machineLabel,
                                  statusFor: statusFor,
                                  onChanged: onChanged,
                                  onMutate: onMutate,
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
    required this.onChanged,
    required this.onMutate,
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
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
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
    final latest = _sourceLatestCycle(
      destinations.map((dst) => statusFor(sourceId, _str(dst['id']))),
    );
    return Transform.translate(
      offset: const Offset(-18, 0),
      child: Stack(
        children: [
          const Positioned(
            left: 7,
            top: 12,
            child: Text(
              '⠿',
              style: TextStyle(
                color: Color(0xff94a3b8),
                fontSize: 13,
                height: 1,
              ),
            ),
          ),
          Container(
            padding: const EdgeInsets.fromLTRB(28, 10, 14, 12),
            decoration: BoxDecoration(
              color: const Color(0xfff8fafc),
              borderRadius: BorderRadius.circular(6),
            ),
            child: Column(
              children: [
                _MasterSyncRow(
                  left: Row(
                    children: [
                      _MasterTextField(
                        label: 'ID',
                        width: 56,
                        value: sourceId,
                        readOnly: true,
                        onCommit: (value) {
                          source['id'] = value;
                          onChanged();
                        },
                      ),
                      const SizedBox(width: 8),
                      _MasterTextField(
                        label: 'Source Path',
                        width: 160,
                        value: _machinePath(
                          _str(source['machine_id'], 'local'),
                          _str(source['src']),
                        ),
                        readOnly: true,
                        onCommit: (value) {
                          source['src'] = _stripMachinePrefix(value);
                          onChanged();
                        },
                      ),
                    ],
                  ),
                  right: Row(
                    children: [
                      _MasterTextField(
                        label: 'Latest Cycle',
                        width: 100,
                        value: latest,
                        readOnly: true,
                        onCommit: (_) {},
                      ),
                      const SizedBox(width: 8),
                      MasterButton(
                        label: 'Excluded ${_list(source['excludes']).length}',
                        onTap: () => _showExcludes(context),
                      ),
                      const SizedBox(width: 8),
                      MasterButton(
                        label: 'Sync',
                        width: 58,
                        onTap: () => onSyncSource(sourceId),
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
                ),
                for (final dst in destinations)
                  _MasterDestinationRow(
                    sourceId: sourceId,
                    destination: dst,
                    destinations: _list(source['destinations']),
                    status: statusFor(sourceId, _str(dst['id'])),
                    onChanged: onChanged,
                    onMutate: onMutate,
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
                    onTap: () {
                      onMutate(() {
                        _list(source['destinations']).add({
                          'id': 'dst_${destinations.length + 1}',
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
                      });
                    },
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Future<void> _showExcludes(BuildContext context) async {
    final controller = TextEditingController(
      text: _list(source['excludes']).join('\n'),
    );
    final result = await showDialog<List<String>>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Excluded'),
        content: SizedBox(
          width: 560,
          height: 360,
          child: TextField(
            controller: controller,
            expands: true,
            maxLines: null,
            minLines: null,
            decoration: const InputDecoration(border: OutlineInputBorder()),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(
              context,
              controller.text
                  .split('\n')
                  .map((line) => line.trim())
                  .where((line) => line.isNotEmpty)
                  .toList(),
            ),
            child: const Text('Save'),
          ),
        ],
      ),
    );
    controller.dispose();
    if (result != null) {
      source['excludes'] = result;
      onChanged('Excludes saved');
    }
  }
}

class _MasterDestinationRow extends StatelessWidget {
  const _MasterDestinationRow({
    required this.sourceId,
    required this.destination,
    required this.destinations,
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
  final Map<String, dynamic>? status;
  final Future<void> Function([String label]) onChanged;
  final void Function(VoidCallback mutate) onMutate;
  final void Function(String sourceId, String destinationId, String mode)
  onSync;
  final void Function(String sourceId, String destinationId) onScan;
  final void Function(String sourceId, String destinationId) onCancel;

  @override
  Widget build(BuildContext context) {
    destination['schedule'] = _mapRef(destination['schedule']);
    final schedule = destination['schedule'] as Map<String, dynamic>;
    final dstId = _str(destination['id'], 'dst');
    return Stack(
      clipBehavior: Clip.none,
      children: [
        Positioned(
          left: -16,
          top: 45,
          child: Container(
            width: 10,
            height: 10,
            decoration: BoxDecoration(
              color: _statusColor(status),
              shape: BoxShape.circle,
            ),
          ),
        ),
        _MasterSyncRow(
          left: Row(
            children: [
              _MasterTextField(
                label: 'ID',
                width: 56,
                value: dstId,
                readOnly: true,
                onCommit: (value) {
                  destination['id'] = value;
                  onChanged();
                },
              ),
              const SizedBox(width: 8),
              _MasterTextField(
                label: 'Destination Path',
                width: 160,
                value: _machinePath(
                  _str(destination['machine_id'], 'local'),
                  _str(destination['path']),
                ),
                readOnly: true,
                onCommit: (value) {
                  destination['path'] = _stripMachinePrefix(value);
                  onChanged();
                },
              ),
            ],
          ),
          right: Row(
            children: [
              _MasterActionStack(
                label: '',
                children: [
                  MasterIconButton(
                    kind: MasterIconKind.info,
                    color: _statusColor(status),
                    onTap: () => onScan(sourceId, dstId),
                  ),
                ],
              ),
              const SizedBox(width: 8),
              _MasterActionStack(
                label: 'Schedule',
                children: [
                  MasterButton(
                    label: _scheduleLabel(schedule),
                    width: 100,
                    accent: true,
                    alignLeft: true,
                    onTap: () => onMutate(() {
                      schedule['mode'] = _str(schedule['mode']) == 'weekly'
                          ? 'daily'
                          : 'weekly';
                    }),
                  ),
                ],
              ),
              const SizedBox(width: 8),
              _MasterTextField(
                label: 'Cycle',
                width: 100,
                value: _cycleDisplay(status),
                readOnly: true,
                onCommit: (_) {},
              ),
              const SizedBox(width: 8),
              MasterIconButton(
                kind: MasterIconKind.gear,
                color: Palette.text,
                onTap: () {},
              ),
              const SizedBox(width: 8),
              _MasterActionStack(
                label: 'Sync',
                children: [
                  MasterSelectButton(
                    value: 'Sync',
                    width: 104,
                    onSelected: (mode) => onSync(sourceId, dstId, mode),
                  ),
                ],
              ),
              const SizedBox(width: 8),
              _MasterActionStack(
                label: '',
                children: [
                  MasterButton(
                    label: 'x',
                    square: true,
                    danger: true,
                    onTap: () =>
                        onMutate(() => destinations.remove(destination)),
                  ),
                ],
              ),
            ],
          ),
        ),
      ],
    );
  }
}

class _MasterSyncRow extends StatelessWidget {
  const _MasterSyncRow({required this.left, required this.right});

  final Widget left;
  final Widget right;

  @override
  Widget build(BuildContext context) {
    return Container(
      margin: const EdgeInsets.only(bottom: 6),
      padding: const EdgeInsets.only(bottom: 6),
      decoration: const BoxDecoration(
        border: Border(bottom: BorderSide(color: Palette.line)),
      ),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        crossAxisAlignment: CrossAxisAlignment.end,
        children: [
          left,
          const SizedBox(width: 8),
          Flexible(
            fit: FlexFit.loose,
            child: Align(alignment: Alignment.centerRight, child: right),
          ),
        ],
      ),
    );
  }
}

class _MasterTextField extends StatelessWidget {
  const _MasterTextField({
    required this.label,
    required this.width,
    required this.value,
    required this.onCommit,
    this.readOnly = false,
  });

  final String label;
  final double width;
  final String value;
  final bool readOnly;
  final ValueChanged<String> onCommit;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: width,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _MasterLabel(label),
          readOnly
              ? _MasterReadOnlyInput(value: value)
              : CommitField(label: '', value: value, onCommit: onCommit),
        ],
      ),
    );
  }
}

class _MasterReadOnlyInput extends StatelessWidget {
  const _MasterReadOnlyInput({required this.value});

  final String value;

  @override
  Widget build(BuildContext context) {
    return Container(
      height: 34,
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

class _MasterActionStack extends StatelessWidget {
  const _MasterActionStack({required this.label, required this.children});

  final String label;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        _MasterLabel(label),
        Row(children: children),
      ],
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
      width: square ? 34 : width,
      height: 34,
      child: OutlinedButton(
        onPressed: onTap,
        style: OutlinedButton.styleFrom(
          minimumSize: Size(square ? 34 : 0, 34),
          maximumSize: Size(width ?? double.infinity, 34),
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          visualDensity: VisualDensity.compact,
          padding: EdgeInsets.symmetric(horizontal: square ? 0 : 12),
          backgroundColor: primary ? Palette.accent : Colors.white,
          foregroundColor: danger
              ? Palette.red
              : primary
              ? Colors.white
              : accent
              ? Palette.accent
              : Palette.text,
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

enum MasterIconKind { info, gear }

class MasterIconButton extends StatelessWidget {
  const MasterIconButton({
    super.key,
    required this.kind,
    required this.color,
    required this.onTap,
  });

  final MasterIconKind kind;
  final Color color;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) {
    return MasterButton(
      label: '',
      square: true,
      onTap: onTap,
      child: kind == MasterIconKind.info
          ? Container(
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
            )
          : Text(
              '\u2699\uFE0E',
              style: TextStyle(
                color: color,
                fontFamily: 'Segoe UI Symbol',
                fontSize: 14,
                fontWeight: FontWeight.w700,
                height: 1,
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
  });

  final String value;
  final double width;
  final ValueChanged<String> onSelected;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: width,
      height: 34,
      child: PopupMenuButton<String>(
        tooltip: 'Sync',
        padding: EdgeInsets.zero,
        offset: const Offset(0, 34),
        onSelected: onSelected,
        itemBuilder: (context) => const [
          PopupMenuItem(value: 'incremental', child: Text('Incremental')),
          PopupMenuItem(value: 'full', child: Text('Full')),
          PopupMenuItem(value: 'scan', child: Text('Compare')),
        ],
        child: Container(
          height: 34,
          padding: const EdgeInsets.symmetric(horizontal: 10),
          decoration: BoxDecoration(
            color: Colors.white,
            border: Border.all(color: Palette.line),
            borderRadius: BorderRadius.circular(6),
          ),
          child: Row(
            children: [
              Expanded(
                child: Text(
                  value,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(
                    color: Palette.text,
                    fontSize: 13,
                    fontWeight: FontWeight.w600,
                  ),
                ),
              ),
              const Icon(Icons.arrow_drop_down, size: 18, color: Palette.text),
            ],
          ),
        ),
      ),
    );
  }
}

String _machinePath(String machineId, String path) {
  final clean = path.startsWith(r'\\?\') ? path.substring(4) : path;
  final prefix = machineId.isEmpty ? 'local' : machineId;
  return '$prefix: $clean';
}

String _stripMachinePrefix(String value) {
  final idx = value.indexOf(': ');
  return idx >= 0 ? value.substring(idx + 2) : value;
}

String _scheduleLabel(Map<String, dynamic> schedule) {
  final mode = _str(schedule['mode'], 'daily');
  final time = _str(schedule['time'], '10:00');
  final weekday = _str(schedule['weekday'], 'saturday');
  if (mode == 'weekly') {
    const labels = {
      'monday': 'Mon',
      'tuesday': 'Tue',
      'wednesday': 'Wed',
      'thursday': 'Thu',
      'friday': 'Fri',
      'saturday': 'Sat',
      'sunday': 'Sun',
    };
    return '${labels[weekday] ?? weekday} $time';
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
  final latest = _str(
    status['latest_closed_cycle_id'],
    _str(status['target_cycle_id'], '-'),
  );
  return '$verified / $latest';
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
                label: 'Excluded ${_list(source['excludes']).length}',
                onTap: () => _showExcludes(context, source, onChanged),
              ),
            ],
          ),
          const SizedBox(height: 12),
          if (destinations.isEmpty)
            const EmptyLine('No destinations')
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
    final controller = TextEditingController(
      text: _list(source['excludes']).join('\n'),
    );
    final result = await showDialog<List<String>>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Excluded paths'),
        content: SizedBox(
          width: 560,
          height: 360,
          child: TextField(
            controller: controller,
            expands: true,
            maxLines: null,
            minLines: null,
            decoration: const InputDecoration(
              hintText: 'One relative path per line',
              border: OutlineInputBorder(),
            ),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () {
              final lines =
                  controller.text
                      .split('\n')
                      .map((line) => line.trim())
                      .where((line) => line.isNotEmpty)
                      .toSet()
                      .toList()
                    ..sort();
              Navigator.pop(context, lines);
            },
            child: const Text('Save'),
          ),
        ],
      ),
    );
    controller.dispose();
    if (result != null) {
      source['excludes'] = result;
      await onChanged('Excludes saved');
    }
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
              PopupMenuButton<String>(
                tooltip: 'Sync',
                onSelected: (mode) => onSync(sourceId, destinationId, mode),
                itemBuilder: (context) => const [
                  PopupMenuItem(
                    value: 'incremental',
                    child: Text('Incremental'),
                  ),
                  PopupMenuItem(value: 'full', child: Text('Full')),
                  PopupMenuItem(value: 'repair', child: Text('Repair')),
                ],
                child: const Icon(Icons.play_arrow_outlined, size: 21),
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
  late final TextEditingController port = TextEditingController(
    text: _str(widget.app['port'], '18765'),
  );
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
    port.dispose();
    timeout.dispose();
    bwlimit.dispose();
    pool.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Settings'),
      content: SizedBox(
        width: 520,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Row(
              children: [
                Expanded(child: _dialogField('Port', port)),
                const SizedBox(width: 10),
                Expanded(child: _dialogField('TCP pool', pool)),
              ],
            ),
            const SizedBox(height: 10),
            Row(
              children: [
                Expanded(child: _dialogField('Timeout secs', timeout)),
                const SizedBox(width: 10),
                Expanded(child: _dialogField('Bwlimit kbps', bwlimit)),
              ],
            ),
            const SizedBox(height: 10),
            Wrap(
              spacing: 18,
              children: [
                LabeledSwitch(
                  label: 'Autostart',
                  value: _bool(widget.app['autostart'], false),
                  onChanged: (value) =>
                      setState(() => widget.app['autostart'] = value),
                ),
                LabeledSwitch(
                  label: 'Close to tray',
                  value: _bool(widget.app['close_to_tray'], true),
                  onChanged: (value) =>
                      setState(() => widget.app['close_to_tray'] = value),
                ),
                LabeledSwitch(
                  label: 'Mirror',
                  value: _bool(widget.sync['mirror'], true),
                  onChanged: (value) =>
                      setState(() => widget.sync['mirror'] = value),
                ),
                LabeledSwitch(
                  label: 'Checksum',
                  value: _bool(widget.sync['checksum'], false),
                  onChanged: (value) =>
                      setState(() => widget.sync['checksum'] = value),
                ),
                LabeledSwitch(
                  label: 'ZFS diff',
                  value: _bool(widget.sync['zfs_diff'], true),
                  onChanged: (value) =>
                      setState(() => widget.sync['zfs_diff'] = value),
                ),
                LabeledSwitch(
                  label: 'Debug logs',
                  value: _bool(widget.sync['debug_logs'], false),
                  onChanged: (value) =>
                      setState(() => widget.sync['debug_logs'] = value),
                ),
              ],
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () {
            widget.app['port'] = int.tryParse(port.text) ?? 18765;
            widget.app['tcp_connection_pool_size'] =
                int.tryParse(pool.text) ?? 100;
            widget.sync['transfer_timeout_secs'] =
                int.tryParse(timeout.text) ?? 120;
            widget.sync['bwlimit_kbps'] = int.tryParse(bwlimit.text) ?? 0;
            widget.app['sync'] = widget.sync;
            Navigator.pop(context, widget.app);
          },
          child: const Text('Save'),
        ),
      ],
    );
  }

  Widget _dialogField(String label, TextEditingController controller) {
    return TextField(
      controller: controller,
      keyboardType: TextInputType.number,
      decoration: InputDecoration(labelText: label),
    );
  }
}

class _SyncSettingsDialog extends StatefulWidget {
  const _SyncSettingsDialog({required this.sync});

  final Map<String, dynamic> sync;

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
    return AlertDialog(
      title: const Text('Destination sync settings'),
      content: SizedBox(
        width: 430,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: timeout,
                    decoration: const InputDecoration(
                      labelText: 'Timeout secs',
                    ),
                  ),
                ),
                const SizedBox(width: 10),
                Expanded(
                  child: TextField(
                    controller: bwlimit,
                    decoration: const InputDecoration(
                      labelText: 'Bwlimit kbps',
                    ),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 10),
            Wrap(
              spacing: 18,
              children: [
                LabeledSwitch(
                  label: 'Mirror',
                  value: _bool(widget.sync['mirror'], true),
                  onChanged: (value) =>
                      setState(() => widget.sync['mirror'] = value),
                ),
                LabeledSwitch(
                  label: 'Checksum',
                  value: _bool(widget.sync['checksum'], false),
                  onChanged: (value) =>
                      setState(() => widget.sync['checksum'] = value),
                ),
                LabeledSwitch(
                  label: 'ZFS diff',
                  value: _bool(widget.sync['zfs_diff'], true),
                  onChanged: (value) =>
                      setState(() => widget.sync['zfs_diff'] = value),
                ),
                LabeledSwitch(
                  label: 'Debug logs',
                  value: _bool(widget.sync['debug_logs'], false),
                  onChanged: (value) =>
                      setState(() => widget.sync['debug_logs'] = value),
                ),
              ],
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('Cancel'),
        ),
        TextButton(
          onPressed: () => Navigator.pop(context, <String, dynamic>{}),
          child: const Text('Use global'),
        ),
        FilledButton(
          onPressed: () {
            widget.sync['transfer_timeout_secs'] =
                int.tryParse(timeout.text) ?? 120;
            widget.sync['bwlimit_kbps'] = int.tryParse(bwlimit.text) ?? 0;
            Navigator.pop(context, widget.sync);
          },
          child: const Text('Save'),
        ),
      ],
    );
  }
}

class _MachinesDialog extends StatefulWidget {
  const _MachinesDialog({
    required this.api,
    required this.machines,
    required this.onChanged,
  });

  final AutoSyncApi api;
  final List<Map<String, dynamic>> machines;
  final Future<void> Function() onChanged;

  @override
  State<_MachinesDialog> createState() => _MachinesDialogState();
}

class _MachinesDialogState extends State<_MachinesDialog> {
  String message = '';
  bool busy = false;
  final id = TextEditingController();
  final name = TextEditingController();
  final host = TextEditingController();
  final port = TextEditingController(text: '18765');
  final sshUser = TextEditingController();
  final sshPort = TextEditingController(text: '22');
  final installDir = TextEditingController();
  String os = 'linux';

  @override
  void dispose() {
    id.dispose();
    name.dispose();
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
      setState(() => message = '$label done');
    } catch (error) {
      setState(() => message = '$label failed: $error');
    } finally {
      setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Machines'),
      content: SizedBox(
        width: 820,
        height: 560,
        child: Column(
          children: [
            Expanded(
              child: ListView(
                children: widget.machines.map((machine) {
                  final id = _str(machine['id']);
                  return Container(
                    margin: const EdgeInsets.only(bottom: 8),
                    padding: const EdgeInsets.all(10),
                    decoration: BoxDecoration(
                      border: Border.all(color: Palette.line),
                      borderRadius: BorderRadius.circular(6),
                    ),
                    child: Row(
                      children: [
                        Expanded(
                          child: Column(
                            crossAxisAlignment: CrossAxisAlignment.start,
                            children: [
                              Text(
                                '$id  ${_str(machine['alias_name'], _str(machine['name']))}',
                                style: const TextStyle(
                                  fontWeight: FontWeight.w700,
                                ),
                              ),
                              const SizedBox(height: 3),
                              Text(
                                '${_str(machine['host'])}:${_str(machine['port'])}  ${_str(machine['os'])}  ${_str(machine['install_dir'])}',
                                style: const TextStyle(color: Palette.muted),
                              ),
                            ],
                          ),
                        ),
                        IconButton(
                          tooltip: 'Remove',
                          onPressed: id == 'local' || busy
                              ? null
                              : () => _do(
                                  'Remove $id',
                                  () => widget.api.removeMachine(id),
                                ),
                          icon: const Icon(Icons.delete_outline),
                        ),
                      ],
                    ),
                  );
                }).toList(),
              ),
            ),
            const Divider(height: 18),
            Wrap(
              spacing: 10,
              runSpacing: 10,
              children: [
                SizedBox(width: 110, child: _input('ID', id)),
                SizedBox(width: 130, child: _input('Name', name)),
                SizedBox(width: 150, child: _input('Host', host)),
                SizedBox(width: 85, child: _input('Port', port)),
                SizedBox(
                  width: 110,
                  child: EnumField(
                    label: 'OS',
                    value: os,
                    values: const ['linux', 'windows', 'openwrt'],
                    onChanged: (value) => setState(() => os = value),
                  ),
                ),
                SizedBox(width: 110, child: _input('SSH user', sshUser)),
                SizedBox(width: 85, child: _input('SSH port', sshPort)),
                SizedBox(width: 180, child: _input('Install dir', installDir)),
              ],
            ),
            const SizedBox(height: 8),
            Row(
              children: [
                Expanded(
                  child: Text(
                    message,
                    style: const TextStyle(color: Palette.muted),
                  ),
                ),
                TextButton.icon(
                  onPressed: busy
                      ? null
                      : () => _do(
                          'Discover',
                          () => widget.api.getMachines(discover: true),
                        ),
                  icon: const Icon(Icons.wifi_find_outlined, size: 18),
                  label: const Text('Discover'),
                ),
                FilledButton.icon(
                  onPressed: busy
                      ? null
                      : () => _do('Add machine', () {
                          final machine = {
                            'id': id.text.trim(),
                            'alias_name': name.text.trim(),
                            'name': name.text.trim(),
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
                        }),
                  icon: const Icon(Icons.add, size: 18),
                  label: const Text('Add'),
                ),
              ],
            ),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('Close'),
        ),
      ],
    );
  }

  Widget _input(String label, TextEditingController controller) {
    return TextField(
      controller: controller,
      decoration: InputDecoration(labelText: label),
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
  String message = '';
  bool loading = true;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    try {
      final nextCfg = await widget.api.collectorConfig();
      final nextStatus = await widget.api.collectorStatus();
      if (mounted) {
        setState(() {
          cfg = nextCfg;
          status = nextStatus;
          loading = false;
        });
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

  @override
  Widget build(BuildContext context) {
    final controller = TextEditingController(
      text: const JsonEncoder.withIndent('  ').convert(cfg),
    );
    return AlertDialog(
      title: const Text('Collector'),
      content: SizedBox(
        width: 760,
        height: 560,
        child: loading
            ? const Center(child: CircularProgressIndicator())
            : Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Wrap(
                    spacing: 8,
                    children: [
                      StatusPill(
                        text: _bool(status['running']) ? 'running' : 'idle',
                        color: _bool(status['running'])
                            ? Palette.warn
                            : Palette.green,
                      ),
                      if (message.isNotEmpty)
                        StatusPill(text: message, color: Palette.muted),
                    ],
                  ),
                  const SizedBox(height: 10),
                  Expanded(
                    child: TextField(
                      controller: controller,
                      expands: true,
                      maxLines: null,
                      minLines: null,
                      style: const TextStyle(
                        fontFamily: 'Consolas',
                        fontSize: 12,
                      ),
                      decoration: const InputDecoration(
                        border: OutlineInputBorder(),
                      ),
                    ),
                  ),
                ],
              ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('Close'),
        ),
        TextButton.icon(
          onPressed: () async {
            try {
              await widget.api.collectorRun();
              await _load();
            } catch (error) {
              setState(() => message = '$error');
            }
          },
          icon: const Icon(Icons.play_arrow_outlined, size: 18),
          label: const Text('Run'),
        ),
        FilledButton.icon(
          onPressed: () async {
            try {
              await widget.api.saveCollectorConfig(
                _map(jsonDecode(controller.text)),
              );
              await _load();
            } catch (error) {
              setState(() => message = '$error');
            }
          },
          icon: const Icon(Icons.save_outlined, size: 18),
          label: const Text('Save'),
        ),
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
    final color = status == 'success'
        ? Palette.green
        : status == 'running'
        ? Palette.warn
        : status == 'failed'
        ? Palette.red
        : Palette.muted;
    return Container(
      padding: const EdgeInsets.symmetric(vertical: 7),
      decoration: const BoxDecoration(
        border: Border(bottom: BorderSide(color: Palette.line)),
      ),
      child: Row(
        children: [
          SizedBox(
            width: 90,
            child: StatusPill(text: status, color: color),
          ),
          Expanded(
            child: Text(
              '${_str(task['kind'])} ${_str(task['source_id'])} -> ${_str(task['destination_id'])}',
              overflow: TextOverflow.ellipsis,
            ),
          ),
          Text(
            _str(task['started_at']),
            style: const TextStyle(color: Palette.muted, fontSize: 12),
          ),
        ],
      ),
    );
  }
}

class _StatusBar extends StatelessWidget {
  const _StatusBar({
    required this.message,
    required this.runtimeStatus,
    required this.activity,
    required this.saving,
  });

  final String message;
  final Map<String, dynamic> runtimeStatus;
  final Map<String, dynamic> activity;
  final bool saving;

  @override
  Widget build(BuildContext context) {
    final syncing = _bool(runtimeStatus['syncing']);
    final phase = _str(
      runtimeStatus['sync_phase'],
      _str(runtimeStatus['phase']),
    );
    final build = _str(_map(runtimeStatus['build'])['version']);
    final errors = _list(runtimeStatus['config_errors']);
    return Container(
      height: 34,
      padding: const EdgeInsets.symmetric(horizontal: 14),
      decoration: const BoxDecoration(
        color: Palette.panel,
        border: Border(top: BorderSide(color: Palette.line)),
      ),
      child: Row(
        children: [
          Icon(
            syncing ? Icons.sync : Icons.check_circle_outline,
            size: 17,
            color: syncing ? Palette.warn : Palette.green,
          ),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              message.isNotEmpty
                  ? message
                  : syncing
                  ? 'Syncing ${phase.isEmpty ? '' : phase}'
                  : 'Idle',
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: const TextStyle(color: Palette.muted, fontSize: 12),
            ),
          ),
          if (errors.isNotEmpty)
            Text(
              '${errors.length} config errors',
              style: const TextStyle(color: Palette.red, fontSize: 12),
            ),
          if (build.isNotEmpty) ...[
            const SizedBox(width: 14),
            Text(
              build,
              style: const TextStyle(color: Palette.muted, fontSize: 12),
            ),
          ],
          if (saving) ...[
            const SizedBox(width: 12),
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
    return DropdownButtonFormField<String>(
      initialValue: value.isEmpty ? null : value,
      decoration: InputDecoration(labelText: label),
      items: items
          .map(
            (item) => DropdownMenuItem(
              value: item,
              child: Text(labelOf == null ? item : labelOf!(item)),
            ),
          )
          .toList(),
      onChanged: (value) {
        if (value != null) {
          onChanged(value);
        }
      },
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
