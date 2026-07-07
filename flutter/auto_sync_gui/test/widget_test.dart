import 'package:auto_sync_gui/main.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('shows startup placeholder', (WidgetTester tester) async {
    await tester.pumpWidget(const AutoSyncShell());

    expect(find.text('Starting auto_sync...'), findsOneWidget);
  });
}
