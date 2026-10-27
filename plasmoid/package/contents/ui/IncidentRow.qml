import QtQuick
import QtQuick.Layouts
import org.kde.plasma.components as PlasmaComponents3
import org.kde.kirigami as Kirigami
import "logic.js" as Logic

// One incident: dot, summary, sub-line, relative time. Click to expand the details.
Rectangle {
    id: row

    required property var incident
    required property double now
    property bool expanded: false

    implicitHeight: content.implicitHeight + 2 * Kirigami.Units.smallSpacing
    color: hover.hovered ? Kirigami.Theme.hoverColor : "transparent"
    radius: Kirigami.Units.cornerRadius

    HoverHandler {
        id: hover
    }
    TapHandler {
        onTapped: row.expanded = !row.expanded
    }

    RowLayout {
        id: content
        anchors.fill: parent
        anchors.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.largeSpacing

        Rectangle {
            Layout.alignment: Qt.AlignTop
            Layout.topMargin: Math.round(Kirigami.Units.smallSpacing * 1.5)
            implicitWidth: 8
            implicitHeight: 8
            radius: 4
            color: Logic.incidentDot(row.incident)
        }

        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            PlasmaComponents3.Label {
                Layout.fillWidth: true
                text: Logic.incidentSummary(row.incident)
                wrapMode: Text.Wrap
                maximumLineCount: row.expanded ? 100 : 2
                elide: Text.ElideRight
            }

            PlasmaComponents3.Label {
                Layout.fillWidth: true
                visible: !row.expanded
                text: Logic.incidentSub(row.incident)
                font.pointSize: Kirigami.Theme.smallFont.pointSize
                color: Kirigami.Theme.disabledTextColor
                elide: Text.ElideRight
            }

            Repeater {
                model: row.expanded ? (row.incident.actions || []) : []

                PlasmaComponents3.Label {
                    required property var modelData
                    Layout.fillWidth: true
                    text: Logic.actionLine(modelData)
                    font.pointSize: Kirigami.Theme.smallFont.pointSize
                    elide: Text.ElideRight
                }
            }

            PlasmaComponents3.Label {
                Layout.fillWidth: true
                visible: row.expanded
                text: row.incident.diagnosis ? "Claude: " + row.incident.diagnosis
                                             : Logic.incidentSub(row.incident)
                font.pointSize: Kirigami.Theme.smallFont.pointSize
                color: Kirigami.Theme.disabledTextColor
                wrapMode: Text.Wrap
            }
        }

        PlasmaComponents3.Label {
            Layout.alignment: Qt.AlignTop
            text: Logic.relTime(row.incident.start_ms_wall, row.now)
            font.pointSize: Kirigami.Theme.smallFont.pointSize
            color: Kirigami.Theme.disabledTextColor
        }
    }
}
