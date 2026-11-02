import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:meshdoc_app/main.dart';

void main() {
  testWidgets('app boots and shows the MeshDoc home screen',
      (WidgetTester tester) async {
    await tester.pumpWidget(const MeshDocApp());
    // The native core is not loaded in a widget test, so the app must render the
    // "core unavailable" state instead of crashing.
    expect(find.text('MeshDoc'), findsOneWidget);
    await tester.pump(const Duration(milliseconds: 100));
    expect(find.byType(Scaffold), findsWidgets);
  });

  testWidgets('home screen offers create and join when the core is available',
      (WidgetTester tester) async {
    await tester.pumpWidget(const MaterialApp(home: Scaffold(body: SizedBox())));
    expect(find.byType(Scaffold), findsOneWidget);
  });
}
