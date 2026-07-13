import 'dart:ui';

import 'package:auto_sync_gui/main.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('shows native auto_sync chrome', (WidgetTester tester) async {
    tester.view.physicalSize = const Size(1200, 800);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    await tester.pumpWidget(
      AutoSyncNativeApp(
        api: AutoSyncApi('http://127.0.0.1:1'),
        autoLoad: false,
      ),
    );

    expect(find.text('auto_sync'), findsNothing);
    expect(find.text('Add Source'), findsOneWidget);
  });
}
