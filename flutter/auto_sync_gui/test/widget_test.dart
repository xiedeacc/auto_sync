import 'package:auto_sync_gui/main.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('shows native auto_sync chrome', (WidgetTester tester) async {
    await tester.pumpWidget(
      AutoSyncNativeApp(
        api: AutoSyncApi('http://127.0.0.1:1'),
        autoLoad: false,
      ),
    );

    expect(find.text('auto_sync'), findsOneWidget);
    expect(find.text('No source groups configured'), findsOneWidget);
  });
}
