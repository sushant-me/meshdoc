/// Wrapper over the Android radio platform channels.
///
/// Contract: docs/INTERFACES.md section B. Implemented in Kotlin
/// (`RadioPlugin.kt`, `BleRadio.kt`, `WifiDirectRadio.kt`). Nothing here derives
/// keys — the discovery UUID always comes from the native core.
library;

import 'dart:async';

import 'package:flutter/services.dart';

import 'models.dart';

class RadioException implements Exception {
  RadioException(this.message);
  final String message;
  @override
  String toString() => message;
}

class RadioService {
  static const MethodChannel _methods = MethodChannel('nl.meshdoc/radio');
  static const EventChannel _peerEvents = EventChannel('nl.meshdoc/radio/peers');

  /// Discovery events. Emits [Peer]s as they are seen over BLE or Wi-Fi Direct.
  Stream<Peer> peers() => _peerEvents.receiveBroadcastStream().map((event) {
        if (event is Map) {
          return Peer.fromJson(Map<String, dynamic>.from(event));
        }
        return const Peer(fingerprint: '', rssi: -127, kind: 'unknown');
      });

  Future<T> _call<T>(String method, [Map<String, dynamic>? args]) async {
    try {
      final result = await _methods.invokeMethod<Object?>(method, args);
      return result as T;
    } on PlatformException catch (e) {
      throw RadioException(e.message ?? '${e.code}: radio call failed');
    } on MissingPluginException {
      throw RadioException(
          'Radio channel is not available on this platform build.');
    }
  }

  Future<RadioCapabilities> capabilities() async {
    final raw = await _call<Map<Object?, Object?>>('capabilities');
    return RadioCapabilities.fromJson(Map<String, dynamic>.from(raw));
  }

  /// Start advertising the rotating discovery tag. Returns null on success.
  Future<void> startAdvertising(String uuidHex, String payloadHex) =>
      _call<void>('startAdvertising', {
        'uuidHex': uuidHex,
        'payloadHex': payloadHex,
      });

  Future<void> stopAdvertising() => _call<void>('stopAdvertising');

  Future<void> startScanning(String uuidHex) =>
      _call<void>('startScanning', {'uuidHex': uuidHex});

  Future<void> stopScanning() => _call<void>('stopScanning');

  /// Form (or join) a Wi-Fi Direct group. Returns the owner address to dial.
  Future<Map<String, dynamic>> formGroup({
    required String myFingerprint,
    required String peerFingerprint,
    String? deviceAddress,
  }) async {
    final raw = await _call<Map<Object?, Object?>>('formGroup', {
      'myFingerprint': myFingerprint,
      'peerFingerprint': peerFingerprint,
      'deviceAddress': deviceAddress,
    });
    return Map<String, dynamic>.from(raw);
  }

  Future<void> leaveGroup() => _call<void>('leaveGroup');

  Future<Map<String, dynamic>> status() async {
    final raw = await _call<Map<Object?, Object?>>('status');
    return Map<String, dynamic>.from(raw);
  }
}
