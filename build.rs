use cxx_qt_build::{CppFile, CxxQtBuilder};

fn main() {
    // Qt Core is added implicitly by cxx-qt-build; Gui and Widgets are needed by
    // the C++ shell (QApplication, QSystemTrayIcon, QMenu, QPainter).
    CxxQtBuilder::new()
        .qt_module("Gui")
        .qt_module("Widgets")
        .files(["src/device.rs"])
        .cpp_files([CppFile::from("cpp/shell.cpp")])
        .build();
}
