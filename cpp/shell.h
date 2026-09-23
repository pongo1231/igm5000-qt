#pragma once

#include <QIcon>
#include <QSet>
#include <QString>
#include <QWidget>

#include <igm5000/src/device.cxxqt.h>

class QAction;
class QCheckBox;
class QCloseEvent;
class QComboBox;
class QLabel;
class QPlainTextEdit;
class QPushButton;
class QSpinBox;
class QSystemTrayIcon;
class QTabWidget;
class QTimer;

/// Widget shell for the IGM 5000 configurator: owns every widget and the tray
/// icon, while all device state lives in the Rust `igm5000::Device` qobject.
///
/// Deliberately not a Q_OBJECT class - the UI is wired with lambdas, so no moc
/// input is needed for this header.
class Shell : public QWidget {
public:
    explicit Shell(igm5000::Device* device, QWidget* parent = nullptr);

    void buildTray();
    void updateTray();
    void updateStatus();
    void updateMode();
    void pullFromDevice();
    void refreshEditorEnablement();
    void showError(const QString& message);
    void showNote(const QString& message);

protected:
    void closeEvent(QCloseEvent* event) override;
    bool eventFilter(QObject* watched, QEvent* event) override;

private:
    void buildUi();
    QWidget* buildPointerTab();
    QWidget* buildAdvancedTab();

    void markDirty(int field);
    void flushDirty();
    void quitNow();
    void toggleWindow();
    QIcon batteryIcon() const;

    igm5000::Device* m_device;

    // Pointer tab
    QCheckBox* m_levelEnabled[8]{};
    QComboBox* m_levelDpi[8]{};
    QPushButton* m_levelColor[8]{};
    QLabel* m_levelActive[8]{};
    int m_levelDpiValue[8]{};
    QString m_levelRgb[8];
    QComboBox* m_rate{};

    // Advanced tab
    QPlainTextEdit* m_hex{};
    QSpinBox* m_lod{};
    QComboBox* m_mode{};
    QSpinBox* m_debounce{};
    QLabel* m_deviceInfo{};

    QLabel* m_status{};

    QSystemTrayIcon* m_tray{};
    QAction* m_startMinimized{};

    QTimer* m_writeTimer{};
    QSet<int> m_dirty;
    bool m_editorsEnabled{false};
    bool m_quitting{false};
};
