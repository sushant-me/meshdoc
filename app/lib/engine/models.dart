/// Typed models for the JSON payloads returned by the Rust C ABI
/// (`docs/INTERFACES.md` section A) and by the Kotlin radio channel
/// (section B).
///
/// Every parser here is defensive: the payloads cross an FFI/platform boundary,
/// so a missing or mistyped field must not crash the UI. Missing values fall
/// back to empty/zero and the raw map is kept where useful.
library;

import 'dart:convert';

/// Thrown when a native payload cannot be understood at all.
class MeshDocFormatException implements Exception {
  MeshDocFormatException(this.message);

  final String message;

  @override
  String toString() => 'MeshDocFormatException: $message';
}

/// Decode a JSON object returned by the native core.
Map<String, dynamic> parseJsonObject(String source) {
  final Object? decoded;
  try {
    decoded = jsonDecode(source);
  } on FormatException catch (error) {
    throw MeshDocFormatException('Native core returned invalid JSON: $error');
  }
  if (decoded is! Map<String, dynamic>) {
    throw MeshDocFormatException(
      'Native core returned ${decoded.runtimeType}, expected a JSON object.',
    );
  }
  return decoded;
}

String _string(Map<String, dynamic> json, String key, {String fallback = ''}) {
  final value = json[key];
  return value is String ? value : fallback;
}

String? _optionalString(Map<String, dynamic> json, String key) {
  final value = json[key];
  if (value is String && value.isNotEmpty) {
    return value;
  }
  return null;
}

int _int(Map<String, dynamic> json, String key, {int fallback = 0}) {
  final value = json[key];
  if (value is int) {
    return value;
  }
  if (value is num) {
    return value.toInt();
  }
  return fallback;
}

double _double(Map<String, dynamic> json, String key, {double fallback = 0}) {
  final value = json[key];
  if (value is num) {
    return value.toDouble();
  }
  return fallback;
}

bool _bool(Map<String, dynamic> json, String key, {bool fallback = false}) {
  final value = json[key];
  return value is bool ? value : fallback;
}

List<Map<String, dynamic>> _maps(Map<String, dynamic> json, String key) {
  final value = json[key];
  if (value is! List) {
    return const [];
  }
  return [
    for (final element in value)
      if (element is Map) Map<String, dynamic>.from(element),
  ];
}

/// Device identity: a 32-hex fingerprint plus the full 128-hex public key.
class Identity {
  const Identity({required this.fingerprint, required this.publicKey});

  final String fingerprint;
  final String publicKey;

  factory Identity.fromJson(Map<String, dynamic> json) => Identity(
    fingerprint: _string(json, 'fingerprint'),
    publicKey: _string(json, 'public'),
  );

  /// `ABCD 1234 …` style grouping for display. UTF-16 safe; the payload is
  /// always ASCII hex.
  String get prettyFingerprint {
    final buffer = StringBuffer();
    for (var i = 0; i < fingerprint.length; i += 4) {
      final end = i + 4 < fingerprint.length ? i + 4 : fingerprint.length;
      if (buffer.isNotEmpty) {
        buffer.write(' ');
      }
      buffer.write(fingerprint.substring(i, end).toUpperCase());
    }
    return buffer.toString();
  }
}

class InitResult {
  const InitResult({
    required this.fingerprint,
    required this.publicKey,
    required this.created,
  });

  final String fingerprint;
  final String publicKey;
  final bool created;

  factory InitResult.fromJson(Map<String, dynamic> json) => InitResult(
    fingerprint: _string(json, 'fingerprint'),
    publicKey: _string(json, 'public'),
    created: _bool(json, 'created'),
  );
}

/// One row of `meshdoc_workspace_list`.
class WorkspaceSummary {
  const WorkspaceSummary({
    required this.id,
    required this.name,
    required this.epoch,
    required this.docs,
    required this.members,
  });

  final String id;
  final String name;
  final int epoch;
  final int docs;
  final int members;

  factory WorkspaceSummary.fromJson(Map<String, dynamic> json) =>
      WorkspaceSummary(
        id: _string(json, 'id'),
        name: _string(json, 'name'),
        epoch: _int(json, 'epoch'),
        docs: _int(json, 'docs'),
        members: _int(json, 'members'),
      );
}

class CreatedWorkspace {
  const CreatedWorkspace({
    required this.workspaceId,
    required this.name,
    required this.epoch,
  });

  final String workspaceId;
  final String name;
  final int epoch;

  factory CreatedWorkspace.fromJson(Map<String, dynamic> json) =>
      CreatedWorkspace(
        workspaceId: _string(json, 'workspace_id'),
        name: _string(json, 'name'),
        epoch: _int(json, 'epoch'),
      );
}

class JoinedWorkspace {
  const JoinedWorkspace({
    required this.workspaceId,
    required this.name,
    required this.members,
  });

  final String workspaceId;
  final String name;
  final int members;

  factory JoinedWorkspace.fromJson(Map<String, dynamic> json) =>
      JoinedWorkspace(
        workspaceId: _string(json, 'workspace_id'),
        name: _string(json, 'name'),
        members: _int(json, 'members'),
      );
}

class Invite {
  const Invite({required this.token, required this.bytes});

  final String token;

  /// Byte length reported by the native core (the token is base64; its string
  /// length is only indirectly related to the byte count).
  final int bytes;

  factory Invite.fromJson(Map<String, dynamic> json) => Invite(
    token: _string(json, 'token'),
    bytes: _int(json, 'bytes'),
  );
}

class DocumentSummary {
  const DocumentSummary({
    required this.id,
    required this.type,
    required this.title,
    required this.summary,
  });

  final String id;
  final String type;
  final String title;
  final String summary;

  factory DocumentSummary.fromJson(Map<String, dynamic> json) =>
      DocumentSummary(
        id: _string(json, 'id'),
        type: _string(json, 'type'),
        title: _string(json, 'title'),
        summary: _string(json, 'summary'),
      );
}

/// The whole workspace snapshot used by the workspace screen.
class WorkspaceState {
  const WorkspaceState({
    required this.workspaceId,
    required this.name,
    required this.epoch,
    required this.members,
    required this.documents,
    required this.changes,
    required this.pending,
  });

  final String workspaceId;
  final String name;
  final int epoch;
  final int members;
  final List<DocumentSummary> documents;
  final int changes;
  final int pending;

  factory WorkspaceState.fromJson(Map<String, dynamic> json) => WorkspaceState(
    workspaceId: _string(json, 'workspace_id'),
    name: _string(json, 'name'),
    epoch: _int(json, 'epoch'),
    members: _int(json, 'members'),
    documents: [
      for (final doc in _maps(json, 'documents'))
        DocumentSummary.fromJson(doc),
    ],
    changes: _int(json, 'changes'),
    pending: _int(json, 'pending'),
  );
}

class SyncResult {
  const SyncResult({
    required this.received,
    required this.applied,
    required this.pages,
  });

  final int received;
  final int applied;
  final int pages;

  factory SyncResult.fromJson(Map<String, dynamic> json) => SyncResult(
    received: _int(json, 'received'),
    applied: _int(json, 'applied'),
    pages: _int(json, 'pages'),
  );

  @override
  String toString() =>
      '$received received, $applied applied, $pages page(s)';
}

class SyncServeResult {
  const SyncServeResult({required this.served, required this.sent});

  final int served;
  final int sent;

  factory SyncServeResult.fromJson(Map<String, dynamic> json) =>
      SyncServeResult(
        served: _int(json, 'served'),
        sent: _int(json, 'sent'),
      );

  @override
  String toString() => '$served session(s) served, $sent change(s) sent';
}

class DiscoveryKey {
  const DiscoveryKey({
    required this.uuid,
    required this.bucket,
    required this.payload,
  });

  final String uuid;
  final int bucket;
  final String payload;

  factory DiscoveryKey.fromJson(Map<String, dynamic> json) => DiscoveryKey(
    uuid: _string(json, 'uuid'),
    bucket: _int(json, 'bucket'),
    payload: _string(json, 'payload'),
  );
}

/// Base type for the four document payloads of section A.
sealed class MeshDocDocument {
  const MeshDocDocument({
    required this.id,
    required this.type,
    required this.title,
  });

  final String id;
  final String type;
  final String title;

  factory MeshDocDocument.fromJson(Map<String, dynamic> json) {
    final id = _string(json, 'id');
    final type = _string(json, 'type');
    final title = _string(json, 'title');
    return switch (type) {
      'checklist' => ChecklistDocument(
        id: id,
        title: title,
        items: [
          for (final item in _maps(json, 'items')) ChecklistItem.fromJson(item),
        ],
      ),
      'note' => NoteDocument(id: id, title: title, text: _string(json, 'text')),
      'geo_point' => GeoPointDocument(
        id: id,
        title: title,
        lat: _double(json, 'lat'),
        lon: _double(json, 'lon'),
        label: _string(json, 'label'),
        kind: _string(json, 'kind'),
      ),
      'supply_form' => SupplyFormDocument(
        id: id,
        title: title,
        fields: [
          for (final field in _maps(json, 'fields')) SupplyField.fromJson(field),
        ],
      ),
      _ => UnknownDocument(id: id, title: title, raw: json),
    };
  }
}

class ChecklistItem {
  const ChecklistItem({
    required this.id,
    required this.text,
    required this.done,
  });

  final String id;
  final String text;
  final bool done;

  factory ChecklistItem.fromJson(Map<String, dynamic> json) => ChecklistItem(
    id: _string(json, 'id'),
    text: _string(json, 'text'),
    done: _bool(json, 'done'),
  );
}

class ChecklistDocument extends MeshDocDocument {
  const ChecklistDocument({
    required super.id,
    required super.title,
    required this.items,
  }) : super(type: 'checklist');

  final List<ChecklistItem> items;

  int get doneCount => items.where((item) => item.done).length;
}

class NoteDocument extends MeshDocDocument {
  const NoteDocument({
    required super.id,
    required super.title,
    required this.text,
  }) : super(type: 'note');

  final String text;
}

class GeoPointDocument extends MeshDocDocument {
  const GeoPointDocument({
    required super.id,
    required super.title,
    required this.lat,
    required this.lon,
    required this.label,
    required this.kind,
  }) : super(type: 'geo_point');

  final double lat;
  final double lon;
  final String label;
  final String kind;
}

class SupplyField {
  const SupplyField({required this.id, required this.value, required this.unit});

  final String id;
  final String value;
  final String unit;

  factory SupplyField.fromJson(Map<String, dynamic> json) => SupplyField(
    id: _string(json, 'id'),
    value: _string(json, 'value'),
    unit: _string(json, 'unit'),
  );
}

class SupplyFormDocument extends MeshDocDocument {
  const SupplyFormDocument({
    required super.id,
    required super.title,
    required this.fields,
  }) : super(type: 'supply_form');

  final List<SupplyField> fields;
}

/// Payload for a document type this build does not know. Shown read-only so a
/// newer core never crashes an older UI.
class UnknownDocument extends MeshDocDocument {
  const UnknownDocument({
    required super.id,
    required super.title,
    required this.raw,
  }) : super(type: 'unknown');

  final Map<String, dynamic> raw;
}

/// A discovered peer event from `EventChannel('nl.meshdoc/radio/peers')`.
class Peer {
  const Peer({
    required this.fingerprint,
    required this.rssi,
    required this.kind,
    this.host,
  });

  final String fingerprint;
  final int rssi;

  /// `ble` or `wifi_direct`.
  final String kind;

  /// Filled in once a Wi-Fi Direct group exists; `null` before that.
  final String? host;

  factory Peer.fromJson(Map<String, dynamic> json) => Peer(
    fingerprint: _string(json, 'fingerprint'),
    rssi: _int(json, 'rssi', fallback: -127),
    kind: _string(json, 'kind', fallback: 'ble'),
    host: _optionalString(json, 'host'),
  );

  Peer copyWith({String? host}) => Peer(
    fingerprint: fingerprint,
    rssi: rssi,
    kind: kind,
    host: host ?? this.host,
  );
}

/// Payload of `capabilities` from section B.
class RadioCapabilities {
  const RadioCapabilities({
    required this.ble,
    required this.bleAdvertise,
    required this.bleScan,
    required this.wifiDirect,
    required this.wifiDirectSupported,
  });

  final bool ble;
  final bool bleAdvertise;
  final bool bleScan;
  final bool wifiDirect;
  final bool wifiDirectSupported;

  factory RadioCapabilities.fromJson(Map<String, dynamic> json) =>
      RadioCapabilities(
        ble: _bool(json, 'ble'),
        bleAdvertise: _bool(json, 'bleAdvertise'),
        bleScan: _bool(json, 'bleScan'),
        wifiDirect: _bool(json, 'wifiDirect'),
        wifiDirectSupported: _bool(json, 'wifiDirectSupported'),
      );

  /// True when the platform reports no usable discovery hardware at all.
  bool get noDiscoveryHardware => !ble && !bleAdvertise && !bleScan &&
      !wifiDirect && !wifiDirectSupported;

  bool get bleUsable => ble && (bleAdvertise || bleScan);
  bool get wifiDirectUsable => wifiDirect || wifiDirectSupported;
}

/// Result of `formGroup`.
class GroupInfo {
  const GroupInfo({
    required this.role,
    required this.host,
    required this.available,
  });

  final String role; // "owner" | "client"
  final String? host;
  final bool available;

  factory GroupInfo.fromJson(Map<String, dynamic> json) => GroupInfo(
    role: _string(json, 'role'),
    host: _optionalString(json, 'host'),
    available: _bool(json, 'available', fallback: true),
  );
}

class RadioStatus {
  const RadioStatus({
    required this.advertising,
    required this.scanning,
    this.group,
  });

  final bool advertising;
  final bool scanning;
  final GroupInfo? group;

  factory RadioStatus.fromJson(Map<String, dynamic> json) {
    final groupJson = json['group'];
    return RadioStatus(
      advertising: _bool(json, 'advertising'),
      scanning: _bool(json, 'scanning'),
      group: groupJson is Map
          ? GroupInfo.fromJson(Map<String, dynamic>.from(groupJson))
          : null,
    );
  }
}
