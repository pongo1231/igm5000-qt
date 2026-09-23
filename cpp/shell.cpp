// Widget shell for the IGM 5000 configurator: the Pointer and Advanced tabs,
// write-on-change with a 300 ms debounce, the battery tray icon, close-to-tray
// and a real Quit.

#include "shell.h"

#include "bridge.h"

#include <QAbstractSpinBox>
#include <QAction>
#include <QApplication>
#include <QCheckBox>
#include <QCloseEvent>
#include <QColor>
#include <QColorDialog>
#include <QComboBox>
#include <QDir>
#include <QFileDialog>
#include <QFont>
#include <QFormLayout>
#include <QFrame>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QLayout>
#include <QMenu>
#include <QMessageBox>
#include <QPainter>
#include <QPen>
#include <QPixmap>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QScrollArea>
#include <QSettings>
#include <QSignalBlocker>
#include <QSpinBox>
#include <QStandardItemModel>
#include <QSystemTrayIcon>
#include <QTabWidget>
#include <QTimer>
#include <QVBoxLayout>

namespace {

enum Field {
    FIELD_RATE = 1,
    FIELD_DPI_BASE = 10,   // + level
    FIELD_COLOR_BASE = 32, // + level
    FIELD_LOD = 60,
    FIELD_LEVEL_BASE = 80, // + level
};

constexpr int kWriteDelayMs = 300;
constexpr int kLevels = 8;

QPushButton* makeColorButton(const QString& tooltip) {
    auto* button = new QPushButton;
    button->setMinimumWidth(64);
    button->setToolTip(tooltip);
    return button;
}

void paintSwatch(QPushButton* button, const QString& rgb) {
    button->setStyleSheet(QStringLiteral("background-color:%1").arg(rgb));
}

/// Make one entry of a combo box selectable, or only displayable. The "(disabled)"
/// DPI entry is a state to show, not one to pick: the firmware crawls rather than
/// skipping a level that holds DPI code 0.
void setEntrySelectable(QComboBox* combo, int index, bool selectable) {
    if (auto* model = qobject_cast<QStandardItemModel*>(combo->model())) {
        if (auto* item = model->item(index)) {
            item->setEnabled(selectable);
        }
    }
}

/// Wrap a tab page in a scroll area: the pointer table alone is taller than a
/// 1280x800 screen at 1.5x scaling, and the window must stay resizable.
QScrollArea* scrollable(QWidget* page) {
    auto* area = new QScrollArea;
    area->setWidget(page);
    area->setWidgetResizable(true);
    area->setFrameShape(QFrame::NoFrame);
    return area;
}

QScrollArea* enclosingScrollArea(QWidget* widget) {
    for (QWidget* parent = widget->parentWidget(); parent; parent = parent->parentWidget()) {
        if (auto* area = qobject_cast<QScrollArea*>(parent)) {
            return area;
        }
    }
    return nullptr;
}

} // namespace

Shell::Shell(igm5000::Device* device, QWidget* parent)
    : QWidget(parent)
    , m_device(device) {
    buildUi();
}

// ---------------------------------------------------------------------------
// ui
// ---------------------------------------------------------------------------

void Shell::buildUi() {
    setWindowTitle(QStringLiteral("IGM 5000 Mouse"));
    resize(640, 520);

    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(6, 6, 6, 6);
    layout->setSpacing(4);
    auto* tabs = new QTabWidget;
    tabs->addTab(scrollable(buildPointerTab()), QStringLiteral("Pointer"));
    tabs->addTab(scrollable(buildAdvancedTab()), QStringLiteral("Advanced"));
    layout->addWidget(tabs, 1);

    m_status = new QLabel;
    m_status->setWordWrap(true);
    m_status->setTextFormat(Qt::PlainText);
    m_status->setMinimumHeight(48);
    layout->addWidget(m_status, 0);

    m_writeTimer = new QTimer(this);
    m_writeTimer->setSingleShot(true);
    m_writeTimer->setInterval(kWriteDelayMs);
    connect(m_writeTimer, &QTimer::timeout, this, [this] { flushDirty(); });

    // See Shell::eventFilter: wheel events over value widgets scroll the tab.
    qApp->installEventFilter(this);

    // Nothing may be written to the mouse before its settings have been read.
    refreshEditorEnablement();
    updateStatus();
}

QWidget* Shell::buildPointerTab() {
    auto* page = new QWidget;
    auto* layout = new QVBoxLayout(page);

    auto* grid = new QGridLayout;
    grid->setVerticalSpacing(4);
    grid->setContentsMargins(0, 0, 0, 0);
    const char* headers[] = {"Level", "Enabled", "DPI", "Colour", "State"};
    for (int column = 0; column < 5; ++column) {
        auto* label = new QLabel(QStringLiteral("<b>%1</b>").arg(headers[column]));
        grid->addWidget(label, 0, column);
    }

    const int steps = m_device->dpiStepCount();
    for (int level = 0; level < kLevels; ++level) {
        const int row = level + 1;
        grid->addWidget(new QLabel(QString::number(level)), row, 0);

        auto* enabled = new QCheckBox;
        m_levelEnabled[level] = enabled;
        grid->addWidget(enabled, row, 1);

        auto* dpi = new QComboBox;
        dpi->addItem(QStringLiteral("(disabled)"), 0);
        for (int index = 0; index < steps; ++index) {
            const int value = m_device->dpiStep(index);
            dpi->addItem(QStringLiteral("%1 dpi").arg(value), value);
        }
        m_levelDpi[level] = dpi;
        grid->addWidget(dpi, row, 2);

        auto* color = makeColorButton(QStringLiteral("Level %1 colour").arg(level));
        m_levelColor[level] = color;
        grid->addWidget(color, row, 3);

        auto* active = new QLabel;
        m_levelActive[level] = active;
        grid->addWidget(active, row, 4);

        connect(enabled, &QCheckBox::toggled, this, [this, level](bool checked) {
            if (checked && m_levelDpiValue[level] <= 0) {
                // A level that joins the profile comes back at 400 dpi.
                const int index = m_levelDpi[level]->findData(400);
                if (index >= 0) {
                    m_levelDpi[level]->setCurrentIndex(index);
                }
            }
            // Levels are added/removed from the profile, which shifts the levels
            // after them - never a per-level "off" value.
            markDirty(FIELD_LEVEL_BASE + level);
        });
        connect(dpi, &QComboBox::currentIndexChanged, this, [this, level](int) {
            m_levelDpiValue[level] = m_levelDpi[level]->currentData().toInt();
            markDirty(FIELD_DPI_BASE + level);
        });
        connect(color, &QPushButton::clicked, this, [this, level] {
            const QColor chosen = QColorDialog::getColor(
                QColor::fromString(m_levelRgb[level]), this,
                QStringLiteral("Level %1 colour").arg(level));
            if (!chosen.isValid()) {
                return;
            }
            m_levelRgb[level] = chosen.name();
            paintSwatch(m_levelColor[level], m_levelRgb[level]);
            markDirty(FIELD_COLOR_BASE + level);
        });
    }
    layout->addLayout(grid);

    auto* hint = new QLabel(
        QStringLiteral("The active level is switched with the mouse's DPI button."));
    hint->setWordWrap(true);
    layout->addWidget(hint);

    auto* rateBox = new QGroupBox(QStringLiteral("Polling rate"));
    auto* rateLayout = new QFormLayout(rateBox);
    auto* rate = new QComboBox;
    const int rates[] = {125, 250, 500, 1000};
    for (const int hz : rates) {
        rate->addItem(QStringLiteral("%1 Hz").arg(hz), hz);
    }
    m_rate = rate;
    rateLayout->addRow(QStringLiteral("Rate"), rate);
    connect(rate, &QComboBox::currentIndexChanged, this, [this](int) { markDirty(FIELD_RATE); });
    layout->addWidget(rateBox);

    layout->addStretch(1);
    return page;
}

QWidget* Shell::buildAdvancedTab() {
    auto* page = new QWidget;
    auto* layout = new QVBoxLayout(page);

    layout->addWidget(new QLabel(QStringLiteral("Settings block as read from the device")));
    auto* hex = new QPlainTextEdit;
    hex->setReadOnly(true);
    QFont mono(QStringLiteral("monospace"));
    mono.setStyleHint(QFont::Monospace);
    hex->setFont(mono);
    m_hex = hex;
    layout->addWidget(hex, 1);

    auto* form = new QFormLayout;

    auto* lod = new QSpinBox;
    lod->setRange(0, 255);
    m_lod = lod;
    form->addRow(QStringLiteral("LOD byte"), lod);
    connect(lod, &QSpinBox::valueChanged, this, [this](int) { markDirty(FIELD_LOD); });

    auto* modeRow = new QHBoxLayout;
    auto* mode = new QComboBox;
    for (int value = 1; value <= 3; ++value) {
        mode->addItem(QStringLiteral("Mode %1").arg(value), value);
    }
    m_mode = mode;
    auto* modeSend = new QPushButton(QStringLiteral("Send mode"));
    modeRow->addWidget(mode);
    modeRow->addWidget(modeSend);
    modeRow->addStretch(1);
    form->addRow(QStringLiteral("Mode"), modeRow);
    connect(modeSend, &QPushButton::clicked, this, [this] {
        const int value = m_mode->currentData().toInt();
        if (value > 0) {
            m_device->sendMode(value);
        }
    });

    auto* debounceRow = new QHBoxLayout;
    auto* debounce = new QSpinBox;
    debounce->setRange(0, 255);
    m_debounce = debounce;
    auto* debounceSend = new QPushButton(QStringLiteral("Send debounce"));
    debounceRow->addWidget(debounce);
    debounceRow->addWidget(debounceSend);
    debounceRow->addStretch(1);
    form->addRow(QStringLiteral("Debounce"), debounceRow);
    connect(debounceSend, &QPushButton::clicked, this, [this] {
        m_device->sendDebounce(m_debounce->value());
    });

    auto* debounceNote = new QLabel(
        QStringLiteral("Register value from the vendor table. It cannot be read back "
                       "and is not covered by a backup."));
    debounceNote->setWordWrap(true);
    form->addRow(debounceNote);

    auto* backupRow = new QHBoxLayout;
    auto* backup = new QPushButton(QStringLiteral("Save backup..."));
    auto* restore = new QPushButton(QStringLiteral("Restore backup..."));
    auto* reload = new QPushButton(QStringLiteral("Reload from device"));
    backupRow->addWidget(backup);
    backupRow->addWidget(restore);
    backupRow->addWidget(reload);
    backupRow->addStretch(1);
    form->addRow(backupRow);

    auto* resetRow = new QHBoxLayout;
    auto* factory = new QPushButton(QStringLiteral("Factory defaults"));
    resetRow->addWidget(factory);
    resetRow->addStretch(1);
    form->addRow(resetRow);
    connect(factory, &QPushButton::clicked, this, [this] {
        const QString saved = QDir::homePath()
            + QStringLiteral("/igm5000.pre-reset.bank%1.bin").arg(m_device->bank());
        const auto answer = QMessageBox::warning(
            this, QStringLiteral("Factory defaults"),
            QStringLiteral(
                "Write the IGM 5000 factory settings?\n\n"
                "DPI levels (400/800/1600/3200/6400/10000), their colours, 500 Hz, "
                "LOD 9 and the red LED effect are restored. Settings the app does not "
                "map are kept.\n\n"
                "The current settings are saved to\n%1\nfirst, so they can be restored. "
                "The debounce value is not restored.")
                .arg(saved),
            QMessageBox::Yes | QMessageBox::No, QMessageBox::No);
        if (answer == QMessageBox::Yes) {
            m_device->factoryReset(saved);
        }
    });

    connect(backup, &QPushButton::clicked, this, [this] {
        const QString suggested = QDir::homePath()
            + QStringLiteral("/igm5000.bank%1.bin").arg(m_device->bank());
        const QString path = QFileDialog::getSaveFileName(
            this, QStringLiteral("Save backup"), suggested,
            QStringLiteral("IGM 5000 backup (*.bin)"));
        if (!path.isEmpty()) {
            m_device->backup(path);
        }
    });
    connect(restore, &QPushButton::clicked, this, [this] {
        const QString path = QFileDialog::getOpenFileName(
            this, QStringLiteral("Restore backup"), QDir::homePath(),
            QStringLiteral("IGM 5000 backup (*.bin)"));
        if (!path.isEmpty()) {
            m_device->restore(path);
        }
    });
    connect(reload, &QPushButton::clicked, this, [this] { m_device->reload(); });

    m_deviceInfo = new QLabel;
    m_deviceInfo->setWordWrap(true);
    form->addRow(QStringLiteral("Device"), m_deviceInfo);

    layout->addLayout(form);
    return page;
}

// ---------------------------------------------------------------------------
// tray
// ---------------------------------------------------------------------------

void Shell::buildTray() {
    if (m_tray) {
        return;
    }
    m_tray = new QSystemTrayIcon(this);
    auto* menu = new QMenu(this);

    QAction* showWindow = menu->addAction(QStringLiteral("Show window"));
    connect(showWindow, &QAction::triggered, this, [this] {
        show();
        raise();
        activateWindow();
    });

    QAction* refresh = menu->addAction(QStringLiteral("Refresh now"));
    connect(refresh, &QAction::triggered, this, [this] { m_device->refresh(); });

    menu->addSeparator();

    m_startMinimized = menu->addAction(QStringLiteral("Start minimized"));
    m_startMinimized->setCheckable(true);
    m_startMinimized->setChecked(QSettings().value(QStringLiteral("startMinimized"), false).toBool());
    connect(m_startMinimized, &QAction::toggled, this, [](bool checked) {
        QSettings settings;
        settings.setValue(QStringLiteral("startMinimized"), checked);
    });

    menu->addSeparator();

    QAction* quit = menu->addAction(QStringLiteral("Quit"));
    connect(quit, &QAction::triggered, this, [this] { quitNow(); });

    m_tray->setContextMenu(menu);
    connect(m_tray, &QSystemTrayIcon::activated, this,
            [this](QSystemTrayIcon::ActivationReason reason) {
                if (reason == QSystemTrayIcon::Trigger || reason == QSystemTrayIcon::DoubleClick) {
                    toggleWindow();
                }
            });
    m_tray->setIcon(batteryIcon());
    updateTray();
    m_tray->show();
}

QIcon Shell::batteryIcon() const {
    const int battery = m_device->getBattery();
    const bool connected = m_device->getConnected();
    const bool charging = m_device->getCharging();

    QColor color(QStringLiteral("#7f8c8d"));
    if (connected) {
        if (charging || battery >= 60) {
            color = QColor(QStringLiteral("#2ecc71"));
        } else if (battery >= 30) {
            color = QColor(QStringLiteral("#f1c40f"));
        } else if (battery >= 0) {
            color = QColor(QStringLiteral("#e74c3c"));
        }
    }

    QPixmap pixmap(64, 64);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing);
    painter.setBrush(color);
    painter.setPen(QPen(color.darker(150), 3));
    painter.drawEllipse(QRectF(6, 6, 52, 52));
    if (connected && charging) {
        painter.setBrush(Qt::NoBrush);
        painter.setPen(QPen(QColor(Qt::white), 3));
        painter.drawEllipse(QRectF(12, 12, 40, 40));
    }
    painter.end();
    return QIcon(pixmap);
}

void Shell::updateTray() {
    const int battery = m_device->getBattery();
    QString text;
    if (!m_device->getConnected()) {
        text = QStringLiteral("IGM 5000 - no device");
    } else if (m_device->getCharging() && battery < 0) {
        // The device reports no charge while it is charging.
        text = QStringLiteral("IGM 5000 - charging");
    } else if (battery < 0) {
        text = QStringLiteral("IGM 5000 - battery unknown");
    } else if (m_device->getCharging()) {
        text = QStringLiteral("IGM 5000 - %1% (charging)").arg(battery);
    } else {
        text = QStringLiteral("IGM 5000 - %1% (on battery)").arg(battery);
    }
    m_tray->setToolTip(text);
    m_tray->setIcon(batteryIcon());
}

void Shell::updateStatus() {
    QStringList lines;
    lines << m_device->getStatusText();
    const int battery = m_device->getBattery();
    if (m_device->getConnected() && m_device->getCharging() && battery < 0) {
        // While charging the device reports no charge to show.
        lines << QStringLiteral("Battery: charging");
    } else if (!m_device->getConnected() || battery < 0) {
        lines << QStringLiteral("Battery: unknown");
    } else {
        lines << QStringLiteral("Battery: %1% (%2)")
                     .arg(battery)
                     .arg(m_device->getCharging() ? QStringLiteral("charging")
                                                  : QStringLiteral("on battery"));
    }
    const QString info = m_device->getDeviceInfo();
    if (!info.isEmpty()) {
        lines << info;
    }
    if (!QSystemTrayIcon::isSystemTrayAvailable()) {
        lines << QStringLiteral("No system tray available");
    }
    m_status->setText(lines.join(QLatin1Char('\n')));
}

void Shell::showError(const QString& message) {
    if (m_status) {
        m_status->setText(message);
    }
    if (m_tray) {
        m_tray->showMessage(QStringLiteral("IGM 5000"), message, QSystemTrayIcon::Warning);
    }
}

void Shell::showNote(const QString& message) {
    if (m_tray) {
        m_tray->showMessage(QStringLiteral("IGM 5000"), message);
    }
}

void Shell::toggleWindow() {
    if (isVisible()) {
        hide();
    } else {
        show();
        raise();
        activateWindow();
    }
}

bool Shell::eventFilter(QObject* watched, QEvent* event) {
    // A wheel over a combo box or spin box would silently change the value and
    // write it to the mouse; scroll the tab instead.
    if (event->type() == QEvent::Wheel) {
        auto* widget = qobject_cast<QWidget*>(watched);
        const bool valueWidget = qobject_cast<QComboBox*>(watched) != nullptr
            || qobject_cast<QAbstractSpinBox*>(watched) != nullptr;
        if (widget && valueWidget && widget->window() == this) {
            if (auto* area = enclosingScrollArea(widget)) {
                QApplication::sendEvent(area->viewport(), event);
                return true;
            }
        }
    }
    return QWidget::eventFilter(watched, event);
}

void Shell::closeEvent(QCloseEvent* event) {
    if (!m_quitting && m_tray && QSystemTrayIcon::isSystemTrayAvailable()) {
        // Keep running in the tray, silently.
        event->ignore();
        hide();
    } else {
        // No tray host: closing must exit, otherwise the app would keep running
        // invisibly (quitOnLastWindowClosed is off).
        event->accept();
        quitNow();
    }
}

void Shell::quitNow() {
    // Invokables only queue: flush, and let the device thread finish what it has.
    flushDirty();
    m_device->shutdown();
    m_quitting = true;
    QApplication::quit();
}

// ---------------------------------------------------------------------------
// device -> widgets
// ---------------------------------------------------------------------------

void Shell::pullFromDevice() {
    const int current = m_device->currentLevel();
    for (int level = 0; level < kLevels; ++level) {
        const bool enabled = m_device->dpiEnabled(level);
        const int dpi = m_device->dpiValue(level);
        // An edit the debounce has not sent yet lives only in these widgets, so
        // overwriting it here would discard it.
        const bool dpiDirty = m_dirty.contains(FIELD_DPI_BASE + level);
        if (!dpiDirty) {
            // 0 is meaningful here: a slot outside the profile holds no DPI, and
            // remembering that is what makes enabling the row seed 400 again.
            m_levelDpiValue[level] = dpi;
        }
        if (!m_dirty.contains(FIELD_LEVEL_BASE + level)) {
            const QSignalBlocker blocker(m_levelEnabled[level]);
            m_levelEnabled[level]->setChecked(enabled);
        }
        if (!dpiDirty) {
            const QSignalBlocker blocker(m_levelDpi[level]);
            const int index = dpi > 0 ? m_levelDpi[level]->findData(dpi) : 0;
            m_levelDpi[level]->setCurrentIndex(index < 0 ? 0 : index);
        }
        setEntrySelectable(m_levelDpi[level], 0, !enabled);
        if (!m_dirty.contains(FIELD_COLOR_BASE + level)) {
            m_levelRgb[level] = m_device->dpiColor(level);
            paintSwatch(m_levelColor[level], m_levelRgb[level]);
        }
        m_levelActive[level]->setText(level == current ? QStringLiteral("active") : QString());
    }

    if (!m_dirty.contains(FIELD_RATE)) {
        const QSignalBlocker blocker(m_rate);
        const int index = m_rate->findData(m_device->rateHz());
        m_rate->setCurrentIndex(index < 0 ? -1 : index);
    }

    {
        const QSignalBlocker blocker(m_hex);
        m_hex->setPlainText(m_device->blockHex());
    }
    if (!m_dirty.contains(FIELD_LOD)) {
        const QSignalBlocker blocker(m_lod);
        m_lod->setValue(m_device->lod());
    }
    updateMode();
    {
        const QSignalBlocker blocker(m_debounce);
        m_debounce->setValue(m_device->debounce());
    }
    m_deviceInfo->setText(m_device->getDeviceInfo());
    // The first block makes the editors meaningful.
    m_editorsEnabled = true;
    refreshEditorEnablement();
    updateStatus();
}

void Shell::updateMode() {
    const QSignalBlocker blocker(m_mode);
    const int mode = m_device->getMode();
    m_mode->setCurrentIndex(mode >= 1 && mode <= 3 ? mode - 1 : -1);
}

void Shell::refreshEditorEnablement() {
    const int levels = m_device->enabledLevels();
    for (int level = 0; level < kLevels; ++level) {
        // Levels are contiguous: the next slot can join, a level inside can leave
        // while more than one remains, nothing else (`remove_level` never empties).
        const bool removable = level < levels && levels > 1;
        const bool joinable = level == levels;
        m_levelEnabled[level]->setEnabled(m_editorsEnabled && (removable || joinable));
        m_levelDpi[level]->setEnabled(m_editorsEnabled);
        m_levelColor[level]->setEnabled(m_editorsEnabled);
    }
    m_rate->setEnabled(m_editorsEnabled);
    m_lod->setEnabled(m_editorsEnabled);
}

// ---------------------------------------------------------------------------
// widgets -> device
// ---------------------------------------------------------------------------

void Shell::markDirty(int field) {
    m_dirty.insert(field);
    m_writeTimer->start();
}

void Shell::flushDirty() {
    const QSet<int> dirty = m_dirty;
    m_dirty.clear();

    if (dirty.contains(FIELD_RATE)) {
        const int hz = m_rate->currentData().toInt();
        if (hz > 0) {
            m_device->applyRate(hz);
        }
    }
    for (int level = 0; level < kLevels; ++level) {
        if (dirty.contains(FIELD_DPI_BASE + level) && m_levelEnabled[level]->isChecked()) {
            m_device->applyDpi(level, m_levelDpiValue[level]);
        }
    }
    for (int level = 0; level < kLevels; ++level) {
        if (dirty.contains(FIELD_COLOR_BASE + level)) {
            m_device->applyColor(level, m_levelRgb[level]);
        }
    }
    if (dirty.contains(FIELD_LOD)) {
        m_device->applyLod(m_lod->value());
    }
    // Levels last, highest row first: a removal shifts every later level down, so
    // ascending order would send the next edit to the wrong level (and refuse an
    // add displaced by a removal).
    for (int level = kLevels - 1; level >= 0; --level) {
        if (dirty.contains(FIELD_LEVEL_BASE + level)) {
            m_device->setLevelEnabled(level, m_levelEnabled[level]->isChecked());
        }
    }
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

int igm5000_run() {
    int argc = 1;
    char appName[] = "igm5000";
    char* argv[] = {appName, nullptr};
    QApplication app(argc, argv);
    QApplication::setQuitOnLastWindowClosed(false);
    QCoreApplication::setOrganizationName(QStringLiteral("igm5000"));
    QCoreApplication::setApplicationName(QStringLiteral("igm5000-gui"));
    QApplication::setApplicationDisplayName(QStringLiteral("IGM 5000 Mouse"));

    auto* device = new igm5000::Device();
    auto* shell = new Shell(device);

    const auto refreshStatus = [shell] {
        // The tray and the window both show the battery, so re-render both.
        shell->updateTray();
        shell->updateStatus();
    };

    QObject::connect(device, &igm5000::Device::blockChanged, shell,
                     [shell] { shell->pullFromDevice(); });
    QObject::connect(device, &igm5000::Device::batteryChanged, shell, refreshStatus);
    QObject::connect(device, &igm5000::Device::chargingChanged, shell, refreshStatus);
    QObject::connect(device, &igm5000::Device::connectedChanged, shell, refreshStatus);
    QObject::connect(device, &igm5000::Device::statusTextChanged, shell,
                     [shell] { shell->updateStatus(); });
    QObject::connect(device, &igm5000::Device::modeChanged, shell,
                     [shell] { shell->updateMode(); });
    QObject::connect(device, &igm5000::Device::errorOccurred, shell,
                     [shell](const QString& message) { shell->showError(message); });
    QObject::connect(device, &igm5000::Device::notified, shell,
                     [shell](const QString& message) { shell->showNote(message); });

    shell->buildTray();
    const QSettings settings;
    const bool startMinimized = settings.value(QStringLiteral("startMinimized"), false).toBool();
    // Without a tray host a hidden window would be the only UI this app has.
    if (!startMinimized || !QSystemTrayIcon::isSystemTrayAvailable()) {
        shell->show();
    }
    device->start();
    return app.exec();
}
