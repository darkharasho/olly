import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami
import "logic.js" as Logic

// One frozen app waiting on the user: what it is, the countdown, Close it / Let it run.
Rectangle {
    id: card

    required property var app
    required property var info // an entry of Logic.needsCards()
    readonly property var lines: Logic.needsText(info)

    implicitHeight: column.implicitHeight + 2 * Kirigami.Units.largeSpacing
    radius: Kirigami.Units.cornerRadius
    color: Kirigami.Theme.neutralBackgroundColor
    border.color: Qt.rgba(Kirigami.Theme.neutralTextColor.r, Kirigami.Theme.neutralTextColor.g,
                          Kirigami.Theme.neutralTextColor.b, 0.5)

    ColumnLayout {
        id: column
        anchors.fill: parent
        anchors.margins: Kirigami.Units.largeSpacing
        spacing: Kirigami.Units.smallSpacing

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            text: card.lines.head
            textFormat: Text.StyledText
            wrapMode: Text.Wrap
            maximumLineCount: 3
            elide: Text.ElideRight
        }

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            text: card.lines.sub
            wrapMode: Text.Wrap
            opacity: 0.8
        }

        RowLayout {
            spacing: Kirigami.Units.smallSpacing

            PlasmaComponents3.Button {
                text: "Close it"
                onClicked: card.app.run("kill", card.info.id)
            }
            PlasmaComponents3.Button {
                text: "Let it run"
                onClicked: card.app.run("unfreeze", card.info.id)
            }
        }

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            visible: card.app.actionError !== "" && card.app.actionErrorTarget === card.info.id
            text: card.app.actionError
            textFormat: Text.PlainText
            color: Kirigami.Theme.negativeTextColor
            wrapMode: Text.Wrap
        }
    }
}
