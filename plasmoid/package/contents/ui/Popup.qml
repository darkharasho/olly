pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami
import "logic.js" as Logic

// The click popup: header, needs-you cards, stats, incidents, footer.
ColumnLayout {
    id: popup

    required property var app
    property bool showAll: false

    readonly property var status: app.status
    readonly property bool offline: app.mood === "offline"
    readonly property var info: Logic.moodInfo(app.mood)
    readonly property bool paused: Logic.pausedNow(status, app.now)
    readonly property var cards: offline ? [] : Logic.needsCards(status, app.now)
    readonly property var stats: Logic.statRows(status)
    readonly property var shown: app.incidents.slice(0, showAll ? 20 : 3)

    Layout.preferredWidth: Kirigami.Units.gridUnit * 22
    Layout.minimumWidth: Kirigami.Units.gridUnit * 22
    Layout.preferredHeight: implicitHeight
    spacing: Kirigami.Units.smallSpacing

    // 1. Header
    RowLayout {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.largeSpacing

        Face {
            Layout.preferredWidth: 52
            Layout.preferredHeight: 52
            mood: popup.app.mood
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            PlasmaComponents3.Label {
                text: "olly"
                font.bold: true
                font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.15
            }
            PlasmaComponents3.Label {
                text: popup.info.name
                color: popup.info.color
                font.bold: true
            }
            PlasmaComponents3.Label {
                Layout.fillWidth: true
                text: popup.app.toolTipSubText
                wrapMode: Text.Wrap
                maximumLineCount: 2
                elide: Text.ElideRight
                opacity: 0.8
            }
        }

        Rectangle {
            Layout.alignment: Qt.AlignTop
            visible: !popup.offline && !!popup.status.dry_run
            implicitWidth: badge.implicitWidth + 2 * Kirigami.Units.smallSpacing
            implicitHeight: badge.implicitHeight
            radius: height / 2
            color: Kirigami.Theme.neutralBackgroundColor
            border.color: Kirigami.Theme.neutralTextColor

            PlasmaComponents3.Label {
                id: badge
                anchors.centerIn: parent
                text: "dry run"
                font.pointSize: Kirigami.Theme.smallFont.pointSize
                color: Kirigami.Theme.neutralTextColor
            }
        }
    }

    // 2. Needs-you cards. The model is a count so the 1 s countdown tick doesn't rebuild them.
    Repeater {
        model: popup.cards.length

        NeedsCard {
            required property int index
            Layout.fillWidth: true
            app: popup.app
            info: popup.cards[index]
        }
    }

    // 3. Stats
    ColumnLayout {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.smallSpacing
        visible: !popup.offline
        spacing: Kirigami.Units.smallSpacing

        Repeater {
            model: popup.stats.length

            RowLayout {
                id: statRow
                required property int index
                readonly property var stat: popup.stats[index]
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing

                PlasmaComponents3.Label {
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 5
                    text: statRow.stat.label
                    opacity: 0.8
                }
                Rectangle {
                    Layout.fillWidth: true
                    implicitHeight: 6
                    radius: 3
                    color: Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g,
                                   Kirigami.Theme.textColor.b, 0.15)

                    Rectangle {
                        width: parent.width * statRow.stat.frac
                        height: parent.height
                        radius: 3
                        color: statRow.stat.color
                    }
                }
                PlasmaComponents3.Label {
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 4
                    horizontalAlignment: Text.AlignRight
                    text: statRow.stat.value
                }
            }
        }

        RowLayout {
            Layout.fillWidth: true
            visible: !popup.offline && !!popup.status.top
            spacing: Kirigami.Units.smallSpacing

            PlasmaComponents3.Label {
                Layout.preferredWidth: Kirigami.Units.gridUnit * 5
                text: "Biggest"
                opacity: 0.8
            }
            PlasmaComponents3.Label {
                Layout.fillWidth: true
                text: popup.status && popup.status.top ? popup.status.top.name : ""
                elide: Text.ElideRight
            }
            PlasmaComponents3.Label {
                Layout.preferredWidth: Kirigami.Units.gridUnit * 4
                horizontalAlignment: Text.AlignRight
                text: popup.status && popup.status.top ? Logic.gb(popup.status.top.bytes) : ""
            }
        }
    }

    // 4. Recent incidents (the last list is kept while offline)
    Kirigami.Separator {
        Layout.fillWidth: true
        visible: popup.shown.length > 0
    }
    PlasmaComponents3.Label {
        Layout.leftMargin: Kirigami.Units.smallSpacing
        visible: popup.shown.length > 0
        text: "Recent incidents"
        font.pointSize: Kirigami.Theme.smallFont.pointSize
        font.capitalization: Font.AllUppercase
        color: Kirigami.Theme.disabledTextColor
    }
    PlasmaComponents3.ScrollView {
        id: scroll
        Layout.fillWidth: true
        Layout.preferredHeight: Math.min(list.implicitHeight, Kirigami.Units.gridUnit * 16)
        visible: popup.shown.length > 0
        contentWidth: availableWidth

        ColumnLayout {
            id: list
            width: scroll.availableWidth
            spacing: 0

            Repeater {
                model: popup.shown

                IncidentRow {
                    required property var modelData
                    Layout.fillWidth: true
                    incident: modelData
                    now: popup.app.now
                }
            }
        }
    }

    // 5. Footer
    Kirigami.Separator {
        Layout.fillWidth: true
    }
    RowLayout {
        Layout.fillWidth: true
        Layout.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.smallSpacing

        PlasmaComponents3.Button {
            text: popup.paused ? "Wake olly up" : "Pause 30 min"
            enabled: !popup.offline
            onClicked: popup.app.run(popup.paused ? "resume" : "pause")
        }
        Item {
            Layout.fillWidth: true
        }
        PlasmaComponents3.ToolButton {
            visible: popup.app.incidents.length > 3
            text: "All incidents…"
            checkable: true
            checked: popup.showAll
            onToggled: popup.showAll = checked
        }
    }
    PlasmaComponents3.Label {
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.smallSpacing
        visible: popup.app.actionError !== "" && popup.app.actionErrorTarget === null
        text: popup.app.actionError
        color: Kirigami.Theme.negativeTextColor
        wrapMode: Text.Wrap
    }
}
