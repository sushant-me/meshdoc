/// Bindings to the native MeshDoc core (`libmeshdoc.so`).
///
/// Contract: docs/INTERFACES.md section A. Every function returns a heap JSON
/// string or NULL; NULL means "see meshdoc_last_error()". Nothing here may leak a
/// Dart exception across the boundary, and a missing library must degrade to a
/// clear "core unavailable" state rather than crashing the app.
library;

import 'dart:async';
import 'dart:ffi';
import 'dart:io';
import 'dart:isolate';

import 'package:ffi/ffi.dart';

import 'models.dart';

// ---------------------------------------------------------------------------
// Raw C signatures
// ---------------------------------------------------------------------------

typedef _VersionC = Pointer<Utf8> Function();
typedef _VersionDart = Pointer<Utf8> Function();

typedef _FreeC = Void Function(Pointer<Utf8>);
typedef _FreeDart = void Function(Pointer<Utf8>);

typedef _LastErrorC = Pointer<Utf8> Function();
typedef _LastErrorDart = Pointer<Utf8> Function();

typedef _TwoStrC = Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>);
typedef _TwoStrDart = Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>);

typedef _ThreeStrC = Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);
typedef _ThreeStrDart = Pointer<Utf8> Function(Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);

typedef _FourStrC = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);
typedef _FourStrDart = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);

typedef _FiveStrC = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);
typedef _FiveStrDart = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>);

typedef _FourStrU16C = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Uint16);
typedef _FourStrU16Dart = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, int);

typedef _U16I32C = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Uint16, Int32);
typedef _U16I32Dart = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, int, int);

typedef _FiveStrI32C = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Int32);
typedef _FiveStrI32Dart = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, int);

typedef _ThreeStrU64C = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, Uint64);
typedef _ThreeStrU64Dart = Pointer<Utf8> Function(
    Pointer<Utf8>, Pointer<Utf8>, Pointer<Utf8>, int);

/// Raised when the native core cannot be loaded or a call fails.
class EngineException implements Exception {
  EngineException(this.message);
  final String message;
  @override
  String toString() => message;
}

/// Thin, typed wrapper over the C ABI.
class MeshDocEngine {
  MeshDocEngine._(this._lib);

  final DynamicLibrary _lib;
  static MeshDocEngine? _instance;
  static bool _unavailableLogged = false;

  /// Why the engine is unavailable, if it is.
  static String? unavailableReason;

  /// Load the native core. Returns null when the library is not bundled, which is
  /// a legitimate state (the APK can be built before the .so is added).
  static MeshDocEngine? tryLoad() {
    if (_instance != null) return _instance;
    try {
      final lib = DynamicLibrary.open('libmeshdoc.so');
      _instance = MeshDocEngine._(lib);
      unavailableReason = null;
      return _instance;
    } catch (e) {
      if (!_unavailableLogged) {
        _unavailableLogged = true;
        unavailableReason =
            'Native core (libmeshdoc.so) is not available: $e';
      }
      return null;
    }
  }

  // ---------------------------------------------------------------- primitives

  String _take(Pointer<Utf8> Function() call) {
    final ptr = call();
    if (ptr == nullptr) {
      throw EngineException(_lastError());
    }
    try {
      return ptr.toDartString();
    } finally {
      _free(ptr);
    }
  }

  String _lastError() {
    try {
      final fn = _lib.lookupFunction<_LastErrorC, _LastErrorDart>('meshdoc_last_error');
      final p = fn();
      if (p == nullptr) return 'unknown native error';
      try {
        return p.toDartString();
      } finally {
        _free(p);
      }
    } catch (_) {
      return 'unknown native error';
    }
  }

  void _free(Pointer<Utf8> p) {
    try {
      _lib.lookupFunction<_FreeC, _FreeDart>('meshdoc_free')(p);
    } catch (_) {
      // free is best effort; never throw from cleanup
    }
  }

  Map<String, dynamic> _obj(String json) => parseJsonObject(json);

  // ------------------------------------------------------------------ methods

  String version() {
    final fn = _lib.lookupFunction<_VersionC, _VersionDart>('meshdoc_version');
    final p = fn();
    if (p == nullptr) return 'unknown';
    try {
      final raw = p.toDartString();
      return raw.replaceAll('"', '');
    } finally {
      _free(p);
    }
  }

  InitResult init(String store, String pass) => InitResult.fromJson(
      _obj(_take(() => _lib.lookupFunction<_TwoStrC, _TwoStrDart>('meshdoc_init')(
          store.toNativeUtf8(), pass.toNativeUtf8()))));

  Identity identity(String store, String pass) => Identity.fromJson(
      _obj(_take(() => _lib.lookupFunction<_TwoStrC, _TwoStrDart>('meshdoc_identity')(
          store.toNativeUtf8(), pass.toNativeUtf8()))));

  CreatedWorkspace createWorkspace(String store, String pass, String name) =>
      CreatedWorkspace.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_ThreeStrC, _ThreeStrDart>('meshdoc_workspace_create')(
              store.toNativeUtf8(), pass.toNativeUtf8(), name.toNativeUtf8()))));

  List<WorkspaceSummary> listWorkspaces(String store, String pass) {
    final json = _obj(_take(() =>
        _lib.lookupFunction<_TwoStrC, _TwoStrDart>('meshdoc_workspace_list')(
            store.toNativeUtf8(), pass.toNativeUtf8())));
    final list = json['workspaces'];
    if (list is! List) return const [];
    return list
        .whereType<Map<String, dynamic>>()
        .map(WorkspaceSummary.fromJson)
        .toList();
  }

  Invite invite(String store, String pass, String ws, String joinerPublicHex) =>
      Invite.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_FourStrC, _FourStrDart>('meshdoc_workspace_invite')(
              store.toNativeUtf8(),
              pass.toNativeUtf8(),
              ws.toNativeUtf8(),
              joinerPublicHex.toNativeUtf8()))));

  JoinedWorkspace join(String store, String pass, String token) =>
      JoinedWorkspace.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_ThreeStrC, _ThreeStrDart>('meshdoc_workspace_join')(
              store.toNativeUtf8(), pass.toNativeUtf8(), token.toNativeUtf8()))));

  WorkspaceState state(String store, String pass, String ws) =>
      WorkspaceState.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_ThreeStrC, _ThreeStrDart>('meshdoc_state')(
              store.toNativeUtf8(), pass.toNativeUtf8(), ws.toNativeUtf8()))));

  MeshDocDocument document(String store, String pass, String ws, String docId) =>
      MeshDocDocument.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_FourStrC, _FourStrDart>('meshdoc_document')(
              store.toNativeUtf8(),
              pass.toNativeUtf8(),
              ws.toNativeUtf8(),
              docId.toNativeUtf8()))));

  String createDoc(String store, String pass, String ws, String type, String title) {
    final json = _obj(_take(() =>
        _lib.lookupFunction<_FiveStrC, _FiveStrDart>('meshdoc_doc_create')(
            store.toNativeUtf8(),
            pass.toNativeUtf8(),
            ws.toNativeUtf8(),
            type.toNativeUtf8(),
            title.toNativeUtf8())));
    return json['doc_id'] as String? ?? '';
  }

  String addChecklistItem(
      String store, String pass, String ws, String docId, String text) {
    final json = _obj(_take(() =>
        _lib.lookupFunction<_FiveStrC, _FiveStrDart>('meshdoc_checklist_add')(
            store.toNativeUtf8(),
            pass.toNativeUtf8(),
            ws.toNativeUtf8(),
            docId.toNativeUtf8(),
            text.toNativeUtf8())));
    return json['item_id'] as String? ?? '';
  }

  void toggleChecklistItem(String store, String pass, String ws, String docId,
      String itemId, bool done) {
    _obj(_take(() =>
        _lib.lookupFunction<_FiveStrI32C, _FiveStrI32Dart>('meshdoc_checklist_toggle')(
            store.toNativeUtf8(),
            pass.toNativeUtf8(),
            ws.toNativeUtf8(),
            docId.toNativeUtf8(),
            itemId.toNativeUtf8(),
            done ? 1 : 0)));
  }

  SyncResult syncPull(
          String store, String pass, String ws, String host, int port) =>
      SyncResult.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_FourStrU16C, _FourStrU16Dart>('meshdoc_sync_pull')(
              store.toNativeUtf8(),
              pass.toNativeUtf8(),
              ws.toNativeUtf8(),
              host.toNativeUtf8(),
              port))));

  SyncServeResult syncServe(
          String store, String pass, String ws, int port, int maxSessions) =>
      SyncServeResult.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_U16I32C, _U16I32Dart>('meshdoc_sync_serve')(
              store.toNativeUtf8(), pass.toNativeUtf8(), ws.toNativeUtf8(), port, maxSessions))));

  DiscoveryKey discoveryKey(
          String store, String pass, String ws, int unixSeconds) =>
      DiscoveryKey.fromJson(_obj(_take(() =>
          _lib.lookupFunction<_ThreeStrU64C, _ThreeStrU64Dart>('meshdoc_discovery_key')(
              store.toNativeUtf8(),
              pass.toNativeUtf8(),
              ws.toNativeUtf8(),
              unixSeconds))));
}

/// Where the encrypted store lives on this platform.
///
/// On Android the app's private files directory is writable without any plugin.
/// The package id is fixed by `android/app/build.gradle.kts`.
String defaultStoreDir() {
  if (Platform.isAndroid) {
    return '/data/data/nl.meshdoc.meshdoc_app/files/meshdoc';
  }
  final home = Platform.environment['HOME'] ?? '.';
  return '$home/.meshdoc-app';
}

/// The prototype passphrase.
///
/// A shipping client would derive this from a Keystore-backed secret and require
/// biometric unlock; this build keeps a fixed local passphrase so the demo needs no
/// setup, and says so in the UI.
const String prototypePassphrase = 'meshdoc-local-prototype-passphrase';

/// Serve sync sessions without blocking the UI isolate.
///
/// `meshdoc_sync_serve` blocks until it has served `maxSessions` peers, so calling
/// it on the UI thread would freeze the app. The isolate reopens the library.
Future<SyncServeResult> syncServeAsync(String store, String pass, String ws,
        int port, int maxSessions) =>
    Isolate.run(() {
      final engine = MeshDocEngine.tryLoad();
      if (engine == null) {
        throw EngineException(
            MeshDocEngine.unavailableReason ?? 'native core unavailable');
      }
      return engine.syncServe(store, pass, ws, port, maxSessions);
    });
