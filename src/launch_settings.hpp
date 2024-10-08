#pragma once

#include "core.h"

#include <QList>
#ifndef __APPLE__
#include <QVulkanInstance>
#endif
#include <QWidget>

class CpuSettings;
#ifndef __APPLE__
class DisplayDevice;
#endif
class DisplaySettings;
class GameListModel;
class ProfileList;
class QComboBox;
class QLayout;
class QTableView;

class LaunchSettings final : public QWidget {
    Q_OBJECT
public:
#ifdef __APPLE__
    LaunchSettings(ProfileList *profiles, GameListModel *games, QWidget *parent = nullptr);
#else
    LaunchSettings(
        ProfileList *profiles,
        GameListModel *games,
        QList<VkPhysicalDevice> &&vkDevices,
        QWidget *parent = nullptr);
#endif
    ~LaunchSettings() override;

    Profile *currentProfile() const;
#ifndef __APPLE__
    DisplayDevice *currentDisplayDevice() const;
#endif
signals:
    void saveClicked(Profile *p);
    void startClicked();
private:
#ifdef __APPLE__
    QWidget *buildSettings(GameListModel *games);
#else
    QWidget *buildSettings(GameListModel *games, QList<VkPhysicalDevice> &&vkDevices);
#endif
    QLayout *buildActions(ProfileList *profiles);

    void requestGamesContextMenu(const QPoint &pos);
    void profileChanged(int index);

    DisplaySettings *m_display;
    CpuSettings *m_cpu;
    QTableView *m_games;
    QComboBox *m_profiles;
};
