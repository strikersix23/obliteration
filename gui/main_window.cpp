#include "main_window.hpp"
#include "launch_settings.hpp"
#include "resources.hpp"
#include "settings.hpp"

#include <QAction>
#include <QApplication>
#include <QCloseEvent>
#include <QCommandLineOption>
#include <QCommandLineParser>
#include <QDesktopServices>
#include <QDir>
#include <QFile>
#include <QFileDialog>
#include <QIcon>
#include <QMenuBar>
#include <QMessageBox>
#include <QProgressDialog>
#include <QResizeEvent>
#include <QScrollBar>
#include <QSettings>
#include <QSocketNotifier>
#include <QStackedWidget>
#include <QToolBar>
#include <QUrl>

#ifndef _WIN32
#include <fcntl.h>
#endif

namespace Args {
    const QCommandLineOption debug("debug", "Immediate launch the VMM in debug mode.", "addr", "127.0.0.1:1234");
    const QCommandLineOption kernel("kernel", "Use this kernel instead of default one.", "path");
}

MainWindow::MainWindow(const QCommandLineParser &args) :
    m_args(args),
    m_main(nullptr),
    m_launch(nullptr),
    m_debugNoti(nullptr)
{
    setWindowTitle("Obliteration");

    // File menu.
    auto fileMenu = menuBar()->addMenu("&File");
    auto openSystemFolder = new QAction("Open System &Folder", this);
    auto quit = new QAction("&Quit", this);

    connect(openSystemFolder, &QAction::triggered, this, &MainWindow::openSystemFolder);
    connect(quit, &QAction::triggered, this, &MainWindow::close);

    fileMenu->addAction(openSystemFolder);
    fileMenu->addSeparator();
    fileMenu->addAction(quit);

    // Help menu.
    auto helpMenu = menuBar()->addMenu("&Help");
    auto reportIssue = new QAction("&Report Issue", this);
    auto aboutQt = new QAction("About &Qt", this);
    auto about = new QAction("&About Obliteration", this);

    connect(reportIssue, &QAction::triggered, this, &MainWindow::reportIssue);
    connect(aboutQt, &QAction::triggered, &QApplication::aboutQt);
    connect(about, &QAction::triggered, this, &MainWindow::aboutObliteration);

    helpMenu->addAction(reportIssue);
    helpMenu->addSeparator();
    helpMenu->addAction(aboutQt);
    helpMenu->addAction(about);

    // Central widget.
    m_main = new QStackedWidget();

    setCentralWidget(m_main);

    // Launch settings.
    m_launch = new LaunchSettings();

    m_main->addWidget(m_launch);
}

MainWindow::~MainWindow()
{
}

void MainWindow::closeEvent(QCloseEvent *event)
{
    // This will set to accept by QMainWindow::closeEvent.
    event->ignore();

    // Save geometry.
    QSettings settings;

    settings.beginGroup(SettingGroups::mainWindow);

    settings.setValue("size", size());
    settings.setValue("maximized", isMaximized());

    if (qGuiApp->platformName() != "wayland") {
        // Wayland does not allow application to position itself.
        settings.setValue("pos", pos());
    }

    QMainWindow::closeEvent(event);
}

void MainWindow::openSystemFolder()
{
    QString folderPath = readSystemDirectorySetting();
    QDesktopServices::openUrl(QUrl::fromLocalFile(folderPath));
}

void MainWindow::reportIssue()
{
    if (!QDesktopServices::openUrl(QUrl("https://github.com/obhq/obliteration/issues/new"))) {
        QMessageBox::critical(this, "Error", "Failed to open https://github.com/obhq/obliteration/issues/new.");
    }
}

void MainWindow::aboutObliteration()
{
    QMessageBox::about(
        this,
        "About Obliteration",
        "Obliteration is a free and open-source PlayStation 4 kernel. It will allows you to run "
        "the PlayStation 4 system software that you have dumped from your PlayStation 4 on your "
        "PC. This will allows you to play your games forever even if your PlayStation 4 stopped "
        "working in the future.");
}

void MainWindow::vmmError(const QString &msg)
{
    killVmm();

    QMessageBox::critical(this, "Error", msg);

    if (m_args.isSet(Args::debug)) {
        close();
    } else {
        m_main->setCurrentIndex(0);
    }
}

void MainWindow::waitKernelExit(bool success)
{
    killVmm();

    if (!success) {
        QMessageBox::critical(
            this,
            "Error",
            "The kernel was stopped unexpectedly. See the kernel logs for more details.");
    }

    m_main->setCurrentIndex(0);
}

void MainWindow::restoreGeometry()
{
    QSettings settings;

    settings.beginGroup(SettingGroups::mainWindow);

    if (settings.value("maximized", false).toBool()) {
        showMaximized();
    } else {
        resize(settings.value("size", QSize(1000, 500)).toSize());

        if (qGuiApp->platformName() != "wayland") {
            move(settings.value("pos", QPoint(200, 200)).toPoint());
        }

        show();
    }
}

void MainWindow::stopDebug()
{
    // We can't free the VMM here because the thread that trigger this method are waiting
    // for us to return.
    if (m_args.isSet(Args::debug)) {
        QMetaObject::invokeMethod(
            this,
            &MainWindow::close,
            Qt::QueuedConnection);
    } else {
        QMetaObject::invokeMethod(
            this,
            &MainWindow::waitKernelExit,
            Qt::QueuedConnection,
            true);
    }
}

void MainWindow::killVmm()
{
    delete m_debugNoti;
    m_debugNoti = nullptr;
}
