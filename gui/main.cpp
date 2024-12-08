#include "main_window.hpp"

#include <QApplication>
#include <QCommandLineParser>
#include <QList>
#include <QMessageBox>
#include <QMetaObject>
#include <QThread>
#ifndef __APPLE__
#include <QVersionNumber>
#include <QVulkanFunctions>
#include <QVulkanInstance>
#endif

#ifndef _WIN32
#include <sys/resource.h>
#endif

int main(int argc, char *argv[])
{
    // Setup application.
    QCoreApplication::setOrganizationName("OBHQ");
    QCoreApplication::setApplicationName("Obliteration");
    QApplication::setStyle("Fusion");

    QApplication app(argc, argv);

    QGuiApplication::setWindowIcon(QIcon(":/resources/obliteration-icon.png"));

    // Parse arguments.
    QCommandLineParser args;

    args.addOption(Args::debug);
    args.addOption(Args::kernel);
    args.process(app);

    // Setup main window.
    MainWindow win(args);

    win.restoreGeometry();

    // Run main window.
    return QApplication::exec();
}
