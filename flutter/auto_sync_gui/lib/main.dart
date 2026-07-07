import 'package:flutter/material.dart';

void main() {
  runApp(const AutoSyncShell());
}

class AutoSyncShell extends StatelessWidget {
  const AutoSyncShell({super.key});

  @override
  Widget build(BuildContext context) {
    return const MaterialApp(
      debugShowCheckedModeBanner: false,
      home: Scaffold(
        backgroundColor: Color(0xfff6f7f9),
        body: Center(
          child: Text(
            'Starting auto_sync...',
            style: TextStyle(
              color: Color(0xff667085),
              fontSize: 13,
              fontWeight: FontWeight.w500,
            ),
          ),
        ),
      ),
    );
  }
}
