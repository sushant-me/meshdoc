//! MeshDoc, offline-first, end-to-end-encrypted collaboration.
//!
//! This is the Android client. It is a working prototype: the core loop (identity,
//! invite-based workspaces, CRDT documents, encrypted change sync, BLE discovery and
//! Wi-Fi Direct group formation) is real. There is no map, no chat, no account.

import 'dart:async';

import 'package:flutter/material.dart';

import 'engine/ffi.dart';
import 'engine/models.dart';
import 'engine/radio.dart';

void main() {
  runApp(const MeshDocApp());
}

class MeshDocApp extends StatelessWidget {
  const MeshDocApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'MeshDoc',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        useMaterial3: true,
        colorSchemeSeed: const Color(0xFF1F6FEB),
        brightness: Brightness.dark,
      ),
      home: const HomeScreen(),
    );
  }
}

void snack(BuildContext context, String message, {bool error = false}) {
  ScaffoldMessenger.of(context).showSnackBar(
    SnackBar(
      content: Text(message),
      backgroundColor: error ? Theme.of(context).colorScheme.errorContainer : null,
      duration: Duration(seconds: error ? 6 : 3),
    ),
  );
}

// Home: identity + workspace list

class HomeScreen extends StatefulWidget {
  const HomeScreen({super.key});

  @override
  State<HomeScreen> createState() => _HomeScreenState();
}

class _HomeScreenState extends State<HomeScreen> {
  MeshDocEngine? _engine;
  String? _error;
  Identity? _identity;
  List<WorkspaceSummary> _workspaces = const [];
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    setState(() {
      _loading = true;
      _error = null;
    });
    final engine = MeshDocEngine.tryLoad();
    if (engine == null) {
      setState(() {
        _engine = null;
        _error = MeshDocEngine.unavailableReason ??
            'The native core (libmeshdoc.so) is not bundled in this build.';
        _loading = false;
      });
      return;
    }
    try {
      final dir = defaultStoreDir();
      final init = engine.init(dir, prototypePassphrase);
      final workspaces = engine.listWorkspaces(dir, prototypePassphrase);
      setState(() {
        _engine = engine;
        _identity = Identity(
          fingerprint: init.fingerprint,
          publicKey: init.publicKey,
        );
        _workspaces = workspaces;
        _loading = false;
      });
    } catch (e) {
      setState(() {
        _error = '$e';
        _loading = false;
      });
    }
  }

  Future<void> _createWorkspace() async {
    final name = await _askText(context, 'New workspace', 'Name', 'Field Ops');
    if (name == null || name.trim().isEmpty) return;
    try {
      _engine!.createWorkspace(defaultStoreDir(), prototypePassphrase, name.trim());
      await _load();
      if (mounted) snack(context, 'Workspace "$name" created');
    } catch (e) {
      if (mounted) snack(context, '$e', error: true);
    }
  }

  Future<void> _joinWorkspace() async {
    final token = await _askText(context, 'Join a workspace', 'Invite token', '');
    if (token == null || token.trim().isEmpty) return;
    try {
      final joined =
          _engine!.join(defaultStoreDir(), prototypePassphrase, token.trim());
      await _load();
      if (mounted) {
        snack(context, 'Joined "${joined.name}" (${joined.members} members)');
      }
    } catch (e) {
      if (mounted) snack(context, '$e', error: true);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Scaffold(
      appBar: AppBar(
        title: const Text('MeshDoc'),
        actions: [
          IconButton(icon: const Icon(Icons.refresh), onPressed: _loading ? null : _load),
        ],
      ),
      floatingActionButton: _engine == null
          ? null
          : Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                FloatingActionButton.extended(
                  heroTag: 'join',
                  onPressed: _joinWorkspace,
                  icon: const Icon(Icons.qr_code_scanner),
                  label: const Text('Join'),
                ),
                const SizedBox(height: 12),
                FloatingActionButton.extended(
                  heroTag: 'create',
                  onPressed: _createWorkspace,
                  icon: const Icon(Icons.add),
                  label: const Text('New workspace'),
                ),
              ],
            ),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : ListView(
              padding: const EdgeInsets.all(16),
              children: [
                if (_error != null)
                  Card(
                    color: theme.colorScheme.errorContainer,
                    child: Padding(
                      padding: const EdgeInsets.all(16),
                      child: Row(children: [
                        const Icon(Icons.warning_amber),
                        const SizedBox(width: 12),
                        Expanded(child: Text(_error!, style: theme.textTheme.bodyMedium)),
                      ]),
                    ),
                  ),
                if (_identity != null) _identityCard(theme),
                const SizedBox(height: 16),
                Text('Workspaces', style: theme.textTheme.titleMedium),
                const SizedBox(height: 8),
                if (_workspaces.isEmpty)
                  const Padding(
                    padding: EdgeInsets.symmetric(vertical: 24),
                    child: Text(
                      'No workspaces yet.\nCreate one, or join with a token from a teammate.',
                      textAlign: TextAlign.center,
                    ),
                  ),
                ..._workspaces.map((w) => Card(
                      child: ListTile(
                        leading: const Icon(Icons.folder_shared),
                        title: Text(w.name.isEmpty ? w.id.substring(0, 8) : w.name),
                        subtitle: Text(
                            '${w.docs} documents · ${w.members} members · epoch ${w.epoch}'),
                        trailing: const Icon(Icons.chevron_right),
                        onTap: () async {
                          await Navigator.of(context).push(MaterialPageRoute(
                            builder: (_) => WorkspaceScreen(
                              engine: _engine!,
                              workspaceId: w.id,
                              workspaceName: w.name,
                            ),
                          ));
                          _load();
                        },
                      ),
                    )),
                const SizedBox(height: 80),
              ],
            ),
    );
  }

  Widget _identityCard(ThemeData theme) => Card(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
            Row(children: [
              const Icon(Icons.fingerprint),
              const SizedBox(width: 8),
              Text('This device', style: theme.textTheme.titleMedium),
            ]),
            const SizedBox(height: 8),
            SelectableText(
              _identity!.fingerprint,
              style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
            ),
            const SizedBox(height: 4),
            Text('No account, no phone number, your identity is this key.',
                style: theme.textTheme.bodySmall),
          ]),
        ),
      );
}

Future<String?> _askText(
    BuildContext context, String title, String label, String initial) async {
  final controller = TextEditingController(text: initial);
  return showDialog<String>(
    context: context,
    builder: (ctx) => AlertDialog(
      title: Text(title),
      content: TextField(
        controller: controller,
        autofocus: true,
        maxLines: label == 'Invite token' ? 4 : 1,
        decoration: InputDecoration(labelText: label),
      ),
      actions: [
        TextButton(onPressed: () => Navigator.pop(ctx), child: const Text('Cancel')),
        FilledButton(
          onPressed: () => Navigator.pop(ctx, controller.text),
          child: const Text('OK'),
        ),
      ],
    ),
  );
}

// Workspace: documents + sync

class WorkspaceScreen extends StatefulWidget {
  const WorkspaceScreen({
    super.key,
    required this.engine,
    required this.workspaceId,
    required this.workspaceName,
  });

  final MeshDocEngine engine;
  final String workspaceId;
  final String workspaceName;

  @override
  State<WorkspaceScreen> createState() => _WorkspaceScreenState();
}

class _WorkspaceScreenState extends State<WorkspaceScreen> {
  WorkspaceState? _state;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    try {
      final s = widget.engine.state(
          defaultStoreDir(), prototypePassphrase, widget.workspaceId);
      if (mounted) {
        setState(() {
          _state = s;
          _error = null;
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    }
  }

  Future<void> _addDocument() async {
    final type = await showModalBottomSheet<String>(
      context: context,
      builder: (ctx) => SafeArea(
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          for (final t in const [
            ('checklist', 'Checklist', Icons.checklist),
            ('note', 'Note', Icons.notes),
            ('supply_form', 'Supply form', Icons.inventory_2),
            ('geo_point', 'Geo point', Icons.place),
          ])
            ListTile(
              leading: Icon(t.$3),
              title: Text(t.$2),
              onTap: () => Navigator.pop(ctx, t.$1),
            ),
        ]),
      ),
    );
    if (type == null || !mounted) return;
    final title = await _askText(context, 'New $type', 'Title', 'Untitled');
    if (title == null || title.trim().isEmpty || !mounted) return;
    try {
      widget.engine.createDoc(defaultStoreDir(), prototypePassphrase,
          widget.workspaceId, type, title.trim());
      await _load();
    } catch (e) {
      if (mounted) snack(context, '$e', error: true);
    }
  }

  Future<void> _invite() async {
    final controller = TextEditingController();
    final peerKey = await showDialog<String>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Invite a device'),
        content: Column(mainAxisSize: MainAxisSize.min, children: [
          const Text(
              'Paste the joining device\'s public identity (shown on its home screen).'),
          const SizedBox(height: 12),
          TextField(
            controller: controller,
            maxLines: 3,
            decoration: const InputDecoration(labelText: 'Public identity (128 hex)'),
          ),
        ]),
        actions: [
          TextButton(onPressed: () => Navigator.pop(ctx), child: const Text('Cancel')),
          FilledButton(
              onPressed: () => Navigator.pop(ctx, controller.text),
              child: const Text('Create invite')),
        ],
      ),
    );
    if (peerKey == null || peerKey.trim().isEmpty) return;
    try {
      final invite = widget.engine.invite(defaultStoreDir(), prototypePassphrase,
          widget.workspaceId, peerKey.trim());
      if (!mounted) return;
      await showDialog<void>(
        context: context,
        builder: (ctx) => AlertDialog(
          title: const Text('Invite token'),
          content: SingleChildScrollView(
            child: Column(mainAxisSize: MainAxisSize.min, children: [
              Text('${invite.bytes} bytes, QR-sized. Send it to the joining device.'),
              const SizedBox(height: 12),
              SelectableText(invite.token,
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 11)),
            ]),
          ),
          actions: [
            FilledButton(onPressed: () => Navigator.pop(ctx), child: const Text('Done')),
          ],
        ),
      );
    } catch (e) {
      if (mounted) snack(context, '$e', error: true);
    }
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final s = _state;
    return Scaffold(
      appBar: AppBar(
        title: Text(widget.workspaceName.isEmpty
            ? widget.workspaceId.substring(0, 8)
            : widget.workspaceName),
        actions: [
          IconButton(icon: const Icon(Icons.person_add), onPressed: _invite,
              tooltip: 'Invite'),
          IconButton(icon: const Icon(Icons.refresh), onPressed: _load),
        ],
      ),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: _addDocument,
        icon: const Icon(Icons.note_add),
        label: const Text('Document'),
      ),
      body: s == null
          ? Center(child: Text(_error ?? 'Loading…'))
          : ListView(
              padding: const EdgeInsets.all(16),
              children: [
                _statusCard(theme, s),
                const SizedBox(height: 12),
                RadioPanel(
                  engine: widget.engine,
                  workspaceId: widget.workspaceId,
                  onSynced: _load,
                ),
                const SizedBox(height: 16),
                Text('Documents', style: theme.textTheme.titleMedium),
                const SizedBox(height: 8),
                if (s.documents.isEmpty)
                  const Padding(
                    padding: EdgeInsets.symmetric(vertical: 24),
                    child: Text('No documents yet.', textAlign: TextAlign.center),
                  ),
                ...s.documents.map((d) => Card(
                      child: ListTile(
                        leading: Icon(_iconFor(d.type)),
                        title: Text(d.title),
                        subtitle: Text('${d.type} · ${d.summary}'),
                        trailing: const Icon(Icons.chevron_right),
                        onTap: () async {
                          await Navigator.of(context).push(MaterialPageRoute(
                            builder: (_) => DocumentScreen(
                              engine: widget.engine,
                              workspaceId: widget.workspaceId,
                              docId: d.id,
                              title: d.title,
                            ),
                          ));
                          _load();
                        },
                      ),
                    )),
                const SizedBox(height: 80),
              ],
            ),
    );
  }

  Widget _statusCard(ThemeData theme, WorkspaceState s) => Card(
        child: Padding(
          padding: const EdgeInsets.all(16),
          child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
            Text(s.name, style: theme.textTheme.titleLarge),
            const SizedBox(height: 6),
            Text('epoch ${s.epoch} · ${s.members} members · ${s.changes} changes'
                '${s.pending > 0 ? " · ${s.pending} waiting for a key" : ""}'),
            const SizedBox(height: 4),
            SelectableText(s.workspaceId,
                style: const TextStyle(fontFamily: 'monospace', fontSize: 11)),
          ]),
        ),
      );
}

IconData _iconFor(String type) => switch (type) {
      'checklist' => Icons.checklist,
      'note' => Icons.notes,
      'supply_form' => Icons.inventory_2,
      'geo_point' => Icons.place,
      _ => Icons.description,
    };

// Document

class DocumentScreen extends StatefulWidget {
  const DocumentScreen({
    super.key,
    required this.engine,
    required this.workspaceId,
    required this.docId,
    required this.title,
  });

  final MeshDocEngine engine;
  final String workspaceId;
  final String docId;
  final String title;

  @override
  State<DocumentScreen> createState() => _DocumentScreenState();
}

class _DocumentScreenState extends State<DocumentScreen> {
  MeshDocDocument? _doc;
  String? _error;
  final _newItem = TextEditingController();

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    try {
      final d = widget.engine.document(defaultStoreDir(), prototypePassphrase,
          widget.workspaceId, widget.docId);
      if (mounted) {
        setState(() {
          _doc = d;
          _error = null;
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = '$e');
    }
  }

  Future<void> _add() async {
    final text = _newItem.text.trim();
    if (text.isEmpty) return;
    try {
      widget.engine.addChecklistItem(defaultStoreDir(), prototypePassphrase,
          widget.workspaceId, widget.docId, text);
      _newItem.clear();
      await _load();
    } catch (e) {
      if (mounted) snack(context, '$e', error: true);
    }
  }

  Future<void> _toggle(ChecklistItem item) async {
    try {
      widget.engine.toggleChecklistItem(defaultStoreDir(), prototypePassphrase,
          widget.workspaceId, widget.docId, item.id, !item.done);
      await _load();
    } catch (e) {
      if (mounted) snack(context, '$e', error: true);
    }
  }

  @override
  Widget build(BuildContext context) {
    final d = _doc;
    return Scaffold(
      appBar: AppBar(title: Text(widget.title)),
      body: d == null
          ? Center(child: Text(_error ?? 'Loading…'))
          : switch (d) {
              ChecklistDocument c => Column(children: [
                  Expanded(
                    child: c.items.isEmpty
                        ? const Center(child: Text('Nothing on the list yet.'))
                        : ListView.builder(
                            itemCount: c.items.length,
                            itemBuilder: (_, i) {
                              final item = c.items[i];
                              return CheckboxListTile(
                                value: item.done,
                                onChanged: (_) => _toggle(item),
                                title: Text(
                                  item.text,
                                  style: item.done
                                      ? const TextStyle(
                                          decoration: TextDecoration.lineThrough)
                                      : null,
                                ),
                                controlAffinity: ListTileControlAffinity.leading,
                                secondary: const Icon(Icons.drag_handle),
                              );
                            },
                          ),
                  ),
                  SafeArea(
                    child: Padding(
                      padding: const EdgeInsets.all(12),
                      child: Row(children: [
                        Expanded(
                          child: TextField(
                            controller: _newItem,
                            decoration: const InputDecoration(
                              hintText: 'Add an item…',
                              border: OutlineInputBorder(),
                            ),
                            onSubmitted: (_) => _add(),
                          ),
                        ),
                        const SizedBox(width: 8),
                        FilledButton(onPressed: _add, child: const Text('Add')),
                      ]),
                    ),
                  ),
                ]),
              NoteDocument n => Padding(
                  padding: const EdgeInsets.all(16),
                  child: SelectableText(n.text.isEmpty ? '(empty note)' : n.text),
                ),
              GeoPointDocument g => ListTile(
                  leading: const Icon(Icons.place),
                  title: Text(g.label.isEmpty ? '(unnamed)' : g.label),
                  subtitle: Text('${g.lat}, ${g.lon} · ${g.kind}'),
                ),
              SupplyFormDocument f => ListView(
                  children: [
                    for (final field in f.fields)
                      ListTile(
                        title: Text(field.id),
                        trailing: Text('${field.value} ${field.unit}'),
                      ),
                  ],
                ),
              _ => Padding(
                  padding: const EdgeInsets.all(16),
                  child: Text('Unsupported document type'),
                ),
            },
    );
  }
}

// Radio panel: discovery + sync

class RadioPanel extends StatefulWidget {
  const RadioPanel({
    super.key,
    required this.engine,
    required this.workspaceId,
    required this.onSynced,
  });

  final MeshDocEngine engine;
  final String workspaceId;
  final Future<void> Function() onSynced;

  @override
  State<RadioPanel> createState() => _RadioPanelState();
}

class _RadioPanelState extends State<RadioPanel> {
  final _radio = RadioService();
  RadioCapabilities? _caps;
  String? _capsError;
  String? _radioError;
  String? _status;
  bool _busy = false;
  bool _advertising = false;
  bool _scanning = false;
  final Map<String, Peer> _peers = {};
  StreamSubscription<Peer>? _sub;
  Timer? _rotate;

  @override
  void initState() {
    super.initState();
    _loadCaps();
  }

  @override
  void dispose() {
    _sub?.cancel();
    _rotate?.cancel();
    _radio.stopAdvertising().catchError((_) {});
    _radio.stopScanning().catchError((_) {});
    super.dispose();
  }

  Future<void> _loadCaps() async {
    try {
      final caps = await _radio.capabilities();
      if (mounted) setState(() => _caps = caps);
    } catch (e) {
      if (mounted) setState(() => _capsError = '$e');
    }
  }

  /// Ask the native core for the current rotating advertisement tag.
  DiscoveryKey _tag() => widget.engine.discoveryKey(
        defaultStoreDir(),
        prototypePassphrase,
        widget.workspaceId,
        DateTime.now().millisecondsSinceEpoch ~/ 1000,
      );

  Future<void> _toggleAdvertising() async {
    setState(() {
      _busy = true;
      _radioError = null;
    });
    try {
      if (_advertising) {
        await _radio.stopAdvertising();
        _rotate?.cancel();
        _advertising = false;
        _status = 'Stopped advertising';
      } else {
        await _radio.startAdvertising(_tag().uuid, _tag().payload);
        _advertising = true;
        _status = 'Advertising this workspace';
        // The tag rotates every 15 minutes; refresh it so discovery keeps working.
        _rotate = Timer.periodic(const Duration(minutes: 15), (_) async {
          try {
            final t = _tag();
            await _radio.startAdvertising(t.uuid, t.payload);
          } catch (_) {}
        });
      }
    } catch (e) {
      _radioError = '$e';
      _advertising = false;
    }
    if (mounted) setState(() => _busy = false);
  }

  Future<void> _toggleScanning() async {
    setState(() {
      _busy = true;
      _radioError = null;
    });
    try {
      if (_scanning) {
        await _radio.stopScanning();
        await _sub?.cancel();
        _sub = null;
        _scanning = false;
        _status = 'Stopped scanning';
      } else {
        _sub = _radio.peers().listen((peer) {
          if (!mounted) return;
          setState(() {
            _peers[peer.fingerprint.isEmpty ? peer.kind : peer.fingerprint] = peer;
          });
        }, onError: (Object e) {
          if (mounted) setState(() => _radioError = '$e');
        });
        await _radio.startScanning(_tag().uuid);
        _scanning = true;
        _status = 'Scanning for nearby MeshDoc devices';
      }
    } catch (e) {
      _radioError = '$e';
      _scanning = false;
    }
    if (mounted) setState(() => _busy = false);
  }

  Future<void> _syncWith(Peer peer) async {
    setState(() {
      _busy = true;
      _radioError = null;
    });
    try {
      // 1. Form (or join) a Wi-Fi Direct group with the discovered peer.
      final myFp = widget.engine
          .identity(defaultStoreDir(), prototypePassphrase)
          .fingerprint;
      final group = await _radio.formGroup(
        myFingerprint: myFp,
        peerFingerprint: peer.fingerprint.isEmpty ? myFp : peer.fingerprint,
      );
      final host = (group['host'] as String?) ?? '192.168.49.1';
      setState(() => _status = 'Group formed (${group['role']}); syncing with $host');

      // 2. Pull changes over the group link.
      final result = widget.engine.syncPull(
          defaultStoreDir(), prototypePassphrase, widget.workspaceId, host, 7777);
      setState(() => _status =
          'Pulled ${result.received} changes (${result.applied} applied)');
      await widget.onSynced();
    } catch (e) {
      _radioError = '$e';
    }
    if (mounted) setState(() => _busy = false);
  }

  Future<void> _serve() async {
    setState(() {
      _busy = true;
      _radioError = null;
    });
    try {
      final result = await syncServeAsync(
          defaultStoreDir(), prototypePassphrase, widget.workspaceId, 7777, 1);
      setState(() => _status = 'Served ${result.served} peer(s), sent ${result.sent} changes');
      await widget.onSynced();
    } catch (e) {
      _radioError = '$e';
    }
    if (mounted) setState(() => _busy = false);
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final caps = _caps;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Row(children: [
            const Icon(Icons.wifi_tethering),
            const SizedBox(width: 8),
            Text('Nearby sync', style: theme.textTheme.titleMedium),
          ]),
          const SizedBox(height: 8),
          if (_capsError != null) Text(_capsError!, style: TextStyle(color: theme.colorScheme.error)),
          if (caps != null) Text(_capabilitySummary(caps), style: theme.textTheme.bodySmall),
          if (caps != null && !caps.bleAdvertise && !caps.wifiDirectSupported)
            Padding(
              padding: const EdgeInsets.only(top: 8),
              child: Text(
                'This device cannot advertise over BLE or use Wi-Fi Direct '
                '(common on emulators). Use the TCP sync below, or run on a real phone.',
                style: theme.textTheme.bodySmall
                    ?.copyWith(color: theme.colorScheme.tertiary),
              ),
            ),
          const SizedBox(height: 12),
          Wrap(spacing: 8, runSpacing: 8, children: [
            FilledButton.tonalIcon(
              onPressed: _busy ? null : _toggleAdvertising,
              icon: Icon(_advertising ? Icons.wifi_off : Icons.campaign),
              label: Text(_advertising ? 'Stop advertising' : 'Advertise'),
            ),
            FilledButton.tonalIcon(
              onPressed: _busy ? null : _toggleScanning,
              icon: Icon(_scanning ? Icons.stop : Icons.search),
              label: Text(_scanning ? 'Stop scanning' : 'Scan'),
            ),
            OutlinedButton.icon(
              onPressed: _busy ? null : _serve,
              icon: const Icon(Icons.download),
              label: const Text('Wait for a peer'),
            ),
          ]),
          if (_status != null)
            Padding(
              padding: const EdgeInsets.only(top: 10),
              child: Text(_status!, style: theme.textTheme.bodySmall),
            ),
          if (_radioError != null)
            Padding(
              padding: const EdgeInsets.only(top: 10),
              child: Text(_radioError!, style: TextStyle(color: theme.colorScheme.error)),
            ),
          if (_peers.isNotEmpty) ...[
            const SizedBox(height: 12),
            Text('Peers seen (${_peers.length})', style: theme.textTheme.labelLarge),
            for (final peer in _peers.values)
              ListTile(
                dense: true,
                contentPadding: EdgeInsets.zero,
                leading: Icon(peer.kind == 'ble' ? Icons.bluetooth : Icons.wifi),
                title: Text(peer.fingerprint.isEmpty
                    ? '(unknown fingerprint)'
                    : peer.fingerprint.substring(0, 16)),
                subtitle: Text('${peer.kind} · ${peer.rssi} dBm'),
                trailing: FilledButton(
                  onPressed: _busy ? null : () => _syncWith(peer),
                  child: const Text('Sync'),
                ),
              ),
          ],
        ]),
      ),
    );
  }

  String _capabilitySummary(RadioCapabilities c) {
    final parts = <String>[];
    parts.add('BLE ${c.ble ? "yes" : "no"}');
    parts.add('advertise ${c.bleAdvertise ? "yes" : "no"}');
    parts.add('scan ${c.bleScan ? "yes" : "no"}');
    parts.add('Wi-Fi Direct ${c.wifiDirectSupported ? "yes" : "no"}');
    return parts.join(' · ');
  }
}
