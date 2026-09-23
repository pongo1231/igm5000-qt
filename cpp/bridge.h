#pragma once

/// Creates QApplication, the Device qobject and the widget shell, then runs the event loop.
///
/// Declared in its own header so the cxx-qt bridge (`include!("cpp/bridge.h")`)
/// never pulls in `shell.h`: that header defines the widget class, which the
/// bridge has no business declaring.
int igm5000_run();
