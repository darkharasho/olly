import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami

// The update card: an offered update, one in progress, or how the last one went.
Rectangle {
    id: card

    required property var app
    property var info: null // Logic.updateCard() result; null hides the card
    readonly property var c: info

    implicitHeight: column.implicitHeight + 2 * Kirigami.Units.largeSpacing
    radius: Kirigami.Units.cornerRadius
    color: c && c.kind === "rolledback" ? Kirigami.Theme.neutralBackgroundColor : Kirigami.Theme.positiveBackgroundColor
    border.color: Qt.rgba(Kirigami.Theme.textColor.r, Kirigami.Theme.textColor.g, Kirigami.Theme.textColor.b, 0.3)

    ColumnLayout {
        id: column
        anchors.fill: parent
        anchors.margins: Kirigami.Units.largeSpacing
        spacing: Kirigami.Units.smallSpacing

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            text: card.c ? card.c.title : ""
            textFormat: Text.PlainText
            font.bold: true
            wrapMode: Text.Wrap
        }

        Repeater {
            model: card.c && card.c.notes ? card.c.notes : []

            PlasmaComponents3.Label {
                required property string modelData
                Layout.fillWidth: true
                text: "• " + modelData
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                opacity: 0.8
            }
        }

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            visible: !!card.c && card.c.more > 0
            text: card.c ? "<a href=\"more\">and " + card.c.more + " more</a>" : ""
            textFormat: Text.StyledText
            onLinkActivated: Qt.openUrlExternally(card.c.url)
        }

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            visible: !!card.c && !!card.c.hint
            text: card.c && card.c.hint ? card.c.hint : ""
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            opacity: 0.8
        }

        PlasmaComponents3.Button {
            visible: !!card.c && card.c.kind === "ready"
            text: card.c && card.c.button ? card.c.button : ""
            onClicked: card.app.run("update")
        }

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            visible: card.app.actionError !== "" && card.app.actionErrorTarget === "update"
            text: card.app.actionError
            textFormat: Text.PlainText
            color: Kirigami.Theme.negativeTextColor
            wrapMode: Text.Wrap
        }
    }
}
