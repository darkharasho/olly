pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami
import org.kde.plasma.plasmoid
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
    readonly property var update: offline ? null
        : Logic.updateCard(status, app.updateDismissedAt, app.now, Plasmoid.metaData.version)
    readonly property var stats: Logic.statRows(status)
    readonly property string swapText: offline ? "" : Logic.swapWarning(status)
    readonly property string gameText: offline ? "" : Logic.gameLine(status)
    readonly property var leakCards: offline ? [] : Logic.leakCards(status, app.dismissedLeaks)
    readonly property var shown: app.incidents.slice(0, showAll ? 20 : 3)

    // A card action's error shows on its card while the card is up, else in the footer.
    function hasCard(id) {
        for (var i = 0; i < cards.length; i++)
            if (cards[i].id === id)
                return true;
        return false;
    }

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
            updateReady: Logic.hasUpdate(popup.status)
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
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                maximumLineCount: 2
                elide: Text.ElideRight
                opacity: 0.8
            }
        }

        Rectangle {
            Layout.alignment: Qt.AlignTop
            visible: !popup.offline && !!popup.status && !!popup.status.dry_run
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

    // 2b. Update card
    UpdateCard {
        Layout.fillWidth: true
        visible: popup.update !== null
        app: popup.app
        info: popup.update
    }

    // 2c. Update check, when no update is offered
    RowLayout {
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.smallSpacing
        Layout.rightMargin: Kirigami.Units.smallSpacing
        visible: !popup.offline && popup.update === null
        spacing: Kirigami.Units.smallSpacing

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            text: Logic.checkLine(popup.status, popup.app.checking, popup.app.checkedAt, popup.app.checkError)
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            opacity: 0.8
            color: popup.app.checkError !== "" && !popup.app.checking
                   ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.textColor
        }
        PlasmaComponents3.ToolButton {
            text: "Check for updates"
            icon.name: "view-refresh"
            enabled: !popup.app.checking
            onClicked: popup.app.checkForUpdate()
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
            visible: !popup.offline && !!popup.status && !!popup.status.top
            spacing: Kirigami.Units.smallSpacing

            PlasmaComponents3.Label {
                Layout.preferredWidth: Kirigami.Units.gridUnit * 5
                text: "Biggest"
                opacity: 0.8
            }
            PlasmaComponents3.Label {
                Layout.fillWidth: true
                text: popup.status && popup.status.top ? popup.status.top.name : ""
                textFormat: Text.PlainText
                elide: Text.ElideRight
            }
            PlasmaComponents3.Label {
                Layout.preferredWidth: Kirigami.Units.gridUnit * 4
                horizontalAlignment: Text.AlignRight
                text: popup.status && popup.status.top ? Logic.gb(popup.status.top.bytes) : ""
            }
        }

        // Game mode: a quiet line, the face is unchanged
        PlasmaComponents3.Label {
            Layout.fillWidth: true
            visible: popup.gameText !== ""
            text: popup.gameText
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            opacity: 0.7
        }

        // Swap warning: information only, so a quiet card under the bars
        Rectangle {
            Layout.fillWidth: true
            visible: popup.swapText !== ""
            implicitHeight: swapLabel.implicitHeight + 2 * Kirigami.Units.smallSpacing
            radius: Kirigami.Units.cornerRadius
            color: Qt.rgba(Kirigami.Theme.neutralBackgroundColor.r, Kirigami.Theme.neutralBackgroundColor.g,
                           Kirigami.Theme.neutralBackgroundColor.b, 0.5)

            PlasmaComponents3.Label {
                id: swapLabel
                anchors.fill: parent
                anchors.margins: Kirigami.Units.smallSpacing
                text: popup.swapText
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                opacity: 0.85
            }
        }

        // Slow-leak heads-up: one soft card per app, information only
        Repeater {
            model: popup.leakCards

            Rectangle {
                id: leakCard
                required property var modelData
                Layout.fillWidth: true
                implicitHeight: leakRow.implicitHeight + 2 * Kirigami.Units.smallSpacing
                radius: Kirigami.Units.cornerRadius
                color: Qt.rgba(Kirigami.Theme.neutralBackgroundColor.r, Kirigami.Theme.neutralBackgroundColor.g,
                               Kirigami.Theme.neutralBackgroundColor.b, 0.5)

                RowLayout {
                    id: leakRow
                    anchors.fill: parent
                    anchors.margins: Kirigami.Units.smallSpacing
                    spacing: Kirigami.Units.smallSpacing

                    PlasmaComponents3.Label {
                        Layout.fillWidth: true
                        text: leakCard.modelData.text
                        textFormat: Text.PlainText
                        wrapMode: Text.Wrap
                        opacity: 0.85
                    }
                    PlasmaComponents3.ToolButton {
                        text: "Dismiss"
                        onClicked: popup.app.dismissedLeaks = Logic.dismissLeak(popup.app.dismissedLeaks, leakCard.modelData.name)
                    }
                }
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
                    diagnoseState: Logic.diagnoseButton(popup.status, modelData)
                    diagnoseError: popup.app.diagnoseErrorId === modelData.id ? popup.app.diagnoseError : ""
                    onAskClaude: id => popup.app.askClaude(id)
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
        visible: popup.app.actionError !== ""
                 && popup.app.actionErrorTarget !== "update"
                 && (popup.app.actionErrorTarget === null || !popup.hasCard(popup.app.actionErrorTarget))
        text: popup.app.actionError
        textFormat: Text.PlainText
        color: Kirigami.Theme.negativeTextColor
        wrapMode: Text.Wrap
    }
}
